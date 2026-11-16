//! ember-pad: a LAN touch deck for an always-on tablet next to the desktop (built for an iPad mini on
//! iOS 9 / Safari 9, so the page is plain ES5).
//!
//!   ember-pad [serve]   HTTP server (ember-pad.service)
//!   ember-pad url       the URL to open on the tablet (contains the access key)
//!
//! Routes: GET /?k=KEY page · GET /api/state · /api/discord · /api/clips · /api/deck · /thumb?id= · /img?u=
//! · POST /api/act {"id", "arg"}.
//! Every route needs the key (query `k`, header `X-Pad-Key` or cookie `pad`) and a private-network peer.
//!
//! State comes from the same places waybar uses (ember state files, Mullvad watcher, magi-overlay for
//! OBS) plus a long-running `pactl subscribe` watcher for the mic and a 5 s `wlopm` check, so the
//! refresh loop barely forks. Discord is driven over Chromium's DevTools protocol on loopback
//! (discord-wayland starts it with --remote-debugging-port); see discord.js for what it clicks.

mod cdp;
mod more;
mod util;

use serde_json::{Value, json};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use util::{home, sh};

const PAGE: &str = include_str!("page.html");
const DISCORD_JS: &str = include_str!("discord.js");

struct Cfg {
    bind: String,
    discord_port: u16,
    overlay: String,
    scenes: Vec<String>,
    share_screen: String,
    side_screen: String,
    mic: String,
    custom_statuses: Vec<String>,
    key: String,
}

struct Shared {
    cfg: Cfg,
    state: Mutex<Value>,
    mic: Mutex<Option<bool>>,
    stream_screen: Mutex<String>,
    vols: Mutex<Value>,
    media: Mutex<Value>,
    game: Mutex<Game>,
    send: Mutex<Value>,
    bomb: Mutex<Bomb>,
    bomb_cv: Condvar,
    discord: Mutex<Option<cdp::Conn>>,
    discord_retry: Mutex<Option<Instant>>,
    last_poll: AtomicU64,
    clip_saved: AtomicU64,
    shot_at: AtomicU64,
    kick: AtomicBool,
}

/// The bomb as seen by our own CS2 game-state feed (gamestate_integration_ember_pad.cfg: no buffer, no
/// throttle). Times are the desktop's clock in ms; the page converts them with its measured clock offset.
#[derive(Default)]
struct Bomb {
    generation: u64,
    state: String,
    deadline: u64,
    defuse_deadline: u64,
    src: &'static str,
    last_gsi: u64,
}

/// Edges in the CS2 feed: when the bomb went down and when a match map loaded.
#[derive(Default)]
struct Game {
    bomb: String,
    bomb_at: u64,
    active: bool,
    found_at: u64,
    /// The first sample only records the current state (no "match found" flash on a restart mid-match).
    seen: bool,
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn state_dir() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME").map(PathBuf::from).unwrap_or_else(|| home().join(".local/state")).join("ember")
}

fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", unsafe { libc::getuid() })))
}

fn load_cfg() -> Cfg {
    let t: toml::Table =
        fs::read_to_string(home().join(".config/ember/pad.toml")).ok().and_then(|s| s.parse().ok()).unwrap_or_default();
    let s = |k: &str, d: &str| t.get(k).and_then(|v| v.as_str()).unwrap_or(d).to_owned();
    let scenes = t
        .get("scenes")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect())
        .unwrap_or_else(|| ["starting", "stream", "chatting", "brb", "ending"].map(String::from).to_vec());
    Cfg {
        bind: s("bind", "0.0.0.0:7790"),
        discord_port: t.get("discord_port").and_then(|v| v.as_integer()).unwrap_or(9223) as u16,
        overlay: s("overlay", "127.0.0.1:7788"),
        scenes,
        share_screen: s("share_screen", "").pipe_or(|| monitor(0)),
        side_screen: s("side_screen", "").pipe_or(|| monitor(1)),
        mic: s("mic", "@DEFAULT_SOURCE@"),
        custom_statuses: t
            .get("custom_statuses")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect())
            .unwrap_or_else(|| ["In a match, back in 40", "Streaming", "AFK, back soon"].map(String::from).to_vec()),
        key: key(),
    }
}

trait OrElse {
    fn pipe_or(self, f: impl FnOnce() -> String) -> String;
}

impl OrElse for String {
    fn pipe_or(self, f: impl FnOnce() -> String) -> String {
        if self.is_empty() { f() } else { self }
    }
}

/// Hyprland's monitor `n` (0 = the first one listed), used when the config doesn't name the screens.
fn monitor(n: usize) -> String {
    let out = sh(&["hyprctl", "monitors", "-j"], Duration::from_secs(2));
    let v: Value = serde_json::from_str(&out).unwrap_or(Value::Null);
    let list = v.as_array().cloned().unwrap_or_default();
    list.get(n).or(list.first()).and_then(|m| m["name"].as_str()).unwrap_or("").to_owned()
}

/// The access key: ~/.config/ember/pad.key, created (0600) on first start.
fn key() -> String {
    let p = home().join(".config/ember/pad.key");
    if let Ok(k) = fs::read_to_string(&p) {
        let k = k.trim();
        if k.len() >= 16 {
            return k.to_owned();
        }
    }
    let mut b = [0u8; 16];
    if fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b)).is_err() {
        eprintln!("ember-pad: no /dev/urandom");
        std::process::exit(1);
    }
    let k: String = b.iter().map(|x| format!("{x:02x}")).collect();
    use std::os::unix::fs::OpenOptionsExt;
    let _ = fs::create_dir_all(p.parent().unwrap());
    let ok = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&p)
        .and_then(|mut f| f.write_all(format!("{k}\n").as_bytes()));
    if let Err(e) = ok {
        eprintln!("ember-pad: can't write {}: {e}", p.display());
        std::process::exit(1);
    }
    k
}

fn lan_ip() -> String {
    let out = sh(&["ip", "-4", "-o", "addr", "show", "scope", "global"], Duration::from_secs(2));
    for line in out.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        let (Some(dev), Some(addr)) = (f.get(1), f.get(3)) else { continue };
        if dev.starts_with("wg") || dev.starts_with("tun") || dev.contains("mullvad") || dev.starts_with("docker") {
            continue;
        }
        let ip = addr.split('/').next().unwrap_or("");
        if ip.parse::<IpAddr>().is_ok_and(private) {
            return ip.to_owned();
        }
    }
    "127.0.0.1".into()
}

fn private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => v.is_private() || v.is_loopback(),
        IpAddr::V6(v) => {
            v.is_loopback()
                || v.to_ipv4_mapped().is_some_and(|m| m.is_private() || m.is_loopback())
                || (v.segments()[0] & 0xfe00) == 0xfc00
        }
    }
}

fn main() {
    let cmd = std::env::args().nth(1).unwrap_or_else(|| "serve".into());
    match cmd.as_str() {
        "serve" => serve(),
        "url" => {
            let cfg = load_cfg();
            let port = cfg.bind.rsplit(':').next().unwrap_or("7790").to_owned();
            println!("http://{}:{port}/?k={}", lan_ip(), cfg.key);
        }
        _ => {
            eprintln!("usage: ember-pad [serve|url]");
            std::process::exit(2);
        }
    }
}

fn serve() {
    let cfg = load_cfg();
    let listener = TcpListener::bind(&cfg.bind).unwrap_or_else(|e| {
        eprintln!("ember-pad: bind {}: {e}", cfg.bind);
        std::process::exit(1);
    });
    eprintln!("ember-pad: listening on {}", cfg.bind);
    let sh_ = Arc::new(Shared {
        cfg,
        state: Mutex::new(json!({})),
        mic: Mutex::new(None),
        stream_screen: Mutex::new(String::new()),
        vols: Mutex::new(json!({})),
        media: Mutex::new(json!({})),
        game: Mutex::new(Game::default()),
        send: Mutex::new(Value::Null),
        bomb: Mutex::new(Bomb::default()),
        bomb_cv: Condvar::new(),
        discord: Mutex::new(None),
        discord_retry: Mutex::new(None),
        last_poll: AtomicU64::new(0),
        clip_saved: AtomicU64::new(0),
        shot_at: AtomicU64::new(0),
        kick: AtomicBool::new(true),
    });
    {
        let s = sh_.clone();
        std::thread::spawn(move || mic_watch(s));
        let s = sh_.clone();
        std::thread::spawn(move || refresher(s));
        let s = sh_.clone();
        std::thread::spawn(move || media_watch(s));
    }
    for conn in listener.incoming().flatten() {
        let s = sh_.clone();
        std::thread::spawn(move || {
            let _ = conn.set_read_timeout(Some(Duration::from_secs(10)));
            let _ = conn.set_write_timeout(Some(Duration::from_secs(10)));
            handle(s, conn);
        });
    }
}

// ── watchers ─────────────────────────────────────────────────────────────────

fn read_mic(s: &Shared) {
    let out = sh(&["pactl", "get-source-mute", &s.cfg.mic], Duration::from_secs(2));
    let v = if out.contains("yes") {
        Some(true)
    } else if out.contains("no") {
        Some(false)
    } else {
        None
    };
    *s.mic.lock().unwrap() = v;
}

/// `pactl subscribe` prints a line per change; re-read the mute flag only when a source changed.
fn mic_watch(s: Arc<Shared>) {
    loop {
        read_mic(&s);
        *s.vols.lock().unwrap() = more::volumes();
        let child = Command::new("pactl").arg("subscribe").stdout(Stdio::piped()).stderr(Stdio::null()).spawn();
        if let Ok(mut c) = child {
            if let Some(out) = c.stdout.take() {
                for line in BufReader::new(out).lines().map_while(Result::ok) {
                    if line.contains("on source") || line.contains("on server") {
                        read_mic(&s);
                        s.kick.store(true, Ordering::Relaxed);
                    }
                    if line.contains("'change' on sink #") {
                        *s.vols.lock().unwrap() = more::volumes();
                        s.kick.store(true, Ordering::Relaxed);
                    }
                }
            }
            let _ = c.kill();
            let _ = c.wait();
        }
        std::thread::sleep(Duration::from_secs(5));
    }
}

/// `playerctl -F` prints a line whenever the active player's status or track changes (idle screen).
fn media_watch(s: Arc<Shared>) {
    loop {
        let child = Command::new("playerctl")
            .args(["-F", "metadata", "--format", "{{status}}\t{{playerName}}\t{{artist}}\t{{title}}"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        if let Ok(mut c) = child {
            if let Some(out) = c.stdout.take() {
                for line in BufReader::new(out).lines().map_while(Result::ok) {
                    let f: Vec<&str> = line.split('\t').collect();
                    *s.media.lock().unwrap() = if f.len() >= 4 {
                        json!({ "status": f[0], "player": f[1], "artist": f[2], "title": f[3] })
                    } else {
                        json!({})
                    };
                }
            }
            let _ = c.kill();
            let _ = c.wait();
        }
        std::thread::sleep(Duration::from_secs(5));
    }
}

fn replay_running() -> bool {
    let Ok(rd) = fs::read_dir("/proc") else { return false };
    rd.flatten().any(|e| {
        e.file_name().to_str().is_some_and(|n| n.bytes().all(|b| b.is_ascii_digit()))
            && fs::read_to_string(e.path().join("comm")).is_ok_and(|c| c.starts_with("gpu-screen-reco"))
    })
}

/// Rebuilds the state about once a second, but only while a tablet is actually polling.
fn refresher(s: Arc<Shared>) {
    let mut last = Instant::now() - Duration::from_secs(60);
    let mut replay = (false, Instant::now() - Duration::from_secs(60));
    loop {
        std::thread::sleep(Duration::from_millis(100));
        let idle = now_ms().saturating_sub(s.last_poll.load(Ordering::Relaxed));
        if idle > 15_000 {
            if idle > 120_000 {
                s.discord.lock().unwrap().take();
            }
            continue;
        }
        if last.elapsed() < Duration::from_millis(1000) && !s.kick.swap(false, Ordering::Relaxed) {
            continue;
        }
        last = Instant::now();
        if replay.1.elapsed() > Duration::from_secs(5) {
            replay = (replay_running(), Instant::now());
        }
        let st = gather(&s, replay.0);
        *s.state.lock().unwrap() = st;
    }
}

fn read_trim(name: &str) -> String {
    fs::read_to_string(state_dir().join(name)).map(|t| t.trim().to_owned()).unwrap_or_default()
}

fn gather(s: &Shared, replay: bool) -> Value {
    let vpn: Value = serde_json::from_str(&read_trim("vpn.json")).unwrap_or(json!({}));
    let overlay = http(&s.cfg.overlay, "GET", "/api/state", None, Duration::from_millis(800))
        .ok()
        .and_then(|(_, b)| serde_json::from_str::<Value>(&b).ok());
    let mut d = discord(s, "state", &Value::Null);
    {
        let mut cur = s.stream_screen.lock().unwrap();
        if d["streaming"] != true {
            cur.clear();
        }
        d["screen"] = if cur.is_empty() { Value::Null } else { json!(*cur) };
    }
    let obs = overlay.as_ref().map(|o| o["state"]["obs"].clone()).unwrap_or(Value::Null);
    let ost = overlay.as_ref().map(|o| o["state"].clone()).unwrap_or(Value::Null);
    let health = http(&s.cfg.overlay, "GET", "/api/health", None, Duration::from_millis(1500))
        .ok()
        .and_then(|(_, b)| serde_json::from_str::<Value>(&b).ok())
        .unwrap_or(Value::Null);
    let cs2 = &ost["cs2"];
    let (bomb_at, found_at) = {
        let mut g = s.game.lock().unwrap();
        let bomb = cs2["bomb"].as_str().unwrap_or("").to_owned();
        if bomb == "planted" && g.bomb != "planted" && g.seen {
            g.bomb_at = now_ms();
        } else if bomb != "planted" {
            g.bomb_at = 0;
        }
        g.bomb = bomb;
        let active = cs2["active"] == true;
        if active && !g.active && g.seen {
            g.found_at = now_ms();
        }
        g.active = active;
        g.seen = overlay.is_some();
        (g.bomb_at, g.found_at)
    };
    let lf = &ost["leetify"];
    json!({
        "time": now_ms(),
        "mic": { "muted": *s.mic.lock().unwrap() },
        "mix": read_trim("mix").parse::<i64>().unwrap_or(0),
        "mode": read_trim("mode"),
        "vpn": { "state": vpn["state"], "city": vpn["city"], "blocked": vpn["blocked"], "competitive": vpn["competitive"] },
        "clip": { "replay": replay, "saved_at": s.clip_saved.load(Ordering::Relaxed) },
        "shot_at": s.shot_at.load(Ordering::Relaxed),
        "vols": s.vols.lock().unwrap().clone(),
        "media": s.media.lock().unwrap().clone(),
        "away": runtime_dir().join("ember-away").exists(),
        "perf": more::perf(&ost["stats"]),
        "cs2": {
            "active": cs2["active"], "map": cs2["map"], "mode": cs2["mode"], "phase": cs2["phase"],
            "round": cs2["round"], "round_phase": cs2["round_phase"], "team": cs2["team"],
            "score_ct": cs2["score_ct"], "score_t": cs2["score_t"], "kills": cs2["kills"], "assists": cs2["assists"],
            "deaths": cs2["deaths"], "mvps": cs2["mvps"], "hs_pct": cs2["hs_pct"], "adr": cs2["adr"],
            "health": cs2["health"], "round_kills": cs2["round_kills"],
            "bomb": cs2["bomb"], "bomb_at": bomb_at, "found_at": found_at,
        },
        "leetify": {
            "aim": lf["aim"], "positioning": lf["positioning"], "utility": lf["utility"], "counter_strafe": lf["counter_strafe"],
            "form": lf["form"].as_array().map(|a| a.iter().take(5).cloned().collect::<Vec<_>>()),
            "premier": lf["premier"], "fetched_at": lf["fetched_at"],
        },
        "twitch": { "live": ost["stream"]["live"], "viewers": ost["stream"]["viewers"], "started_at": ost["stream"]["started_at"],
                    "followers": ost["stream"]["followers"], "title": ost["stream"]["title"] },
        "health": health,
        "send": s.send.lock().unwrap().clone(),
        "statuses": s.cfg.custom_statuses,
        "screens": { "main": s.cfg.share_screen, "side": s.cfg.side_screen },
        "obs": {
            "overlay": overlay.is_some(),
            "connected": obs["connected"],
            "streaming": obs["streaming"],
            "since": obs["stream_started_at"],
            "scene": obs["scene"],
        },
        "scenes": s.cfg.scenes,
        "discord": d,
    })
}

// ── Discord over DevTools ────────────────────────────────────────────────────

fn discord(s: &Shared, act: &str, arg: &Value) -> Value {
    let mut g = s.discord.lock().unwrap();
    if g.is_none() {
        let mut retry = s.discord_retry.lock().unwrap();
        if act == "state" && retry.is_some_and(|t| t.elapsed() < Duration::from_secs(10)) {
            return json!({ "ok": false, "err": "offline" });
        }
        match cdp::Conn::connect(s.cfg.discord_port) {
            Ok(c) => *g = Some(c),
            Err(e) => {
                *retry = Some(Instant::now());
                return json!({ "ok": false, "err": e });
            }
        }
    }
    let expr = format!("({})({},{})", DISCORD_JS.trim().trim_end_matches(';'), json!(act), arg);
    match g.as_mut().unwrap().eval(&expr) {
        Ok(v) if v.is_object() => v,
        Ok(v) => json!({ "ok": false, "err": format!("unexpected {v}") }),
        Err(e) => {
            *g = None;
            json!({ "ok": false, "err": e })
        }
    }
}

// ── bomb timer (CS2 game-state integration) ──────────────────────────────────

/// One GSI post. `phase_countdowns` (when CS2 sends it) is the game's own timer with 0.1 s steps: every
/// post gives an estimate `received + phase_ends_in`, and since a post can only arrive late, the smallest
/// estimate is the best one. Without it the plant is stamped when its post arrives (buffer 0 = right away).
fn gsi(s: &Shared, body: &[u8]) -> bool {
    let Ok(v) = serde_json::from_slice::<Value>(body) else { return false };
    if v["auth"]["token"].as_str() != Some(s.cfg.key.as_str()) {
        return false;
    }
    let now = now_ms();
    let round_bomb = v["round"]["bomb"].as_str().unwrap_or("");
    let phase = v["phase_countdowns"]["phase"].as_str().unwrap_or("");
    let ends = v["phase_countdowns"]["phase_ends_in"]
        .as_str()
        .and_then(|x| x.parse::<f64>().ok())
        .or(v["phase_countdowns"]["phase_ends_in"].as_f64());
    let mut b = s.bomb.lock().unwrap();
    let before = (b.state.clone(), b.deadline, b.defuse_deadline);
    b.last_gsi = now;
    match (phase, ends) {
        ("bomb", Some(e)) => {
            let est = now + (e * 1000.0).max(0.0) as u64;
            if b.state != "planted" || b.deadline == 0 || est < b.deadline {
                b.deadline = est;
            }
            b.state = "planted".into();
            b.src = "countdown";
            b.defuse_deadline = 0;
        }
        ("defuse", Some(e)) => {
            let est = now + (e * 1000.0).max(0.0) as u64;
            if b.defuse_deadline == 0 || est < b.defuse_deadline {
                b.defuse_deadline = est;
            }
            if b.state != "planted" {
                b.state = "planted".into();
                b.deadline = 0;
            }
        }
        _ => match round_bomb {
            "planted" => {
                if b.state != "planted" {
                    b.state = "planted".into();
                    b.deadline = now + 40_000;
                    b.src = "plant";
                }
                b.defuse_deadline = 0;
            }
            "defused" | "exploded" => {
                b.state = round_bomb.into();
                b.defuse_deadline = 0;
            }
            _ => {
                b.state.clear();
                b.deadline = 0;
                b.defuse_deadline = 0;
            }
        },
    }
    if (b.state.clone(), b.deadline, b.defuse_deadline) != before {
        b.generation += 1;
        s.bomb_cv.notify_all();
    }
    true
}

fn bomb_json(b: &Bomb) -> Value {
    let now = now_ms();
    json!({
        "gen": b.generation, "state": b.state, "deadline": b.deadline, "defuse_deadline": b.defuse_deadline,
        "src": b.src, "now": now, "gsi": b.last_gsi > 0 && now - b.last_gsi < 30_000,
    })
}

// ── actions ──────────────────────────────────────────────────────────────────

fn run_detached(cmd: &str) {
    let path = format!("{}:{}", home().join(".local/bin").display(), std::env::var("PATH").unwrap_or_default());
    let child = Command::new("sh")
        .args(["-c", cmd])
        .env("PATH", path)
        .current_dir(home())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    if let Ok(mut c) = child {
        std::thread::spawn(move || {
            let _ = c.wait();
        });
    }
}

fn deck_actions() -> Vec<Value> {
    let t: toml::Table =
        fs::read_to_string(home().join(".config/ember/deck.toml")).ok().and_then(|s| s.parse().ok()).unwrap_or_default();
    t.get("action")
        .and_then(|a| a.as_array())
        .map(|a| {
            a.iter()
                .enumerate()
                .filter_map(|(i, x)| {
                    let x = x.as_table()?;
                    Some(json!({
                        "i": i,
                        "label": x.get("label")?.as_str()?,
                        "group": x.get("group").and_then(|g| g.as_str()).unwrap_or(""),
                        "key": x.get("key").and_then(|g| g.as_str()).unwrap_or(""),
                        "run": x.get("run")?.as_str()?,
                    }))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn overlay_post(s: &Shared, path: &str, body: &Value) -> Value {
    match http(&s.cfg.overlay, "POST", path, Some(&body.to_string()), Duration::from_secs(15)) {
        Ok((code, b)) if code < 300 => serde_json::from_str(&b).unwrap_or(json!({ "ok": true })),
        Ok((code, b)) => json!({ "ok": false, "err": format!("overlay {code}: {}", b.chars().take(200).collect::<String>()) }),
        Err(e) => json!({ "ok": false, "err": format!("magi-overlay: {e}") }),
    }
}

/// SEND on a clip: full-quality share copy (`ember-clip share`), staged on a hidden file input in
/// Discord through DevTools (the browser reads it from disk), then uploaded into the voice channel's chat.
fn clip_send(s: &Arc<Shared>, cid: &str) -> Value {
    if !more::clip_exists(cid) || cid.contains('\'') {
        return json!({ "ok": false, "err": "no such clip" });
    }
    let busy = s.send.lock().unwrap()["state"].as_str().is_some_and(|st| st == "encoding" || st == "staging");
    if busy {
        return json!({ "ok": false, "err": "already preparing a clip" });
    }
    let prep = discord(s, "upload-prep", &Value::Null);
    if prep["ok"] == false {
        return prep;
    }
    *s.send.lock().unwrap() = json!({ "id": cid, "state": "encoding", "at": now_ms() });
    let s2 = s.clone();
    let cid = cid.to_owned();
    std::thread::spawn(move || {
        let set = |v: Value| *s2.send.lock().unwrap() = v;
        let out =
            sh(&["sh", "-c", &format!("PATH=\"$HOME/.local/bin:$PATH\" ember-clip share '{cid}'")], Duration::from_secs(1800));
        let Some(path) =
            out.lines().rev().map(str::trim).find(|l| l.starts_with('/') && std::path::Path::new(l).exists()).map(str::to_owned)
        else {
            return set(json!({ "id": cid, "state": "error", "err": "share copy failed", "at": now_ms() }));
        };
        set(json!({ "id": cid, "state": "staging", "at": now_ms() }));
        let name = std::path::Path::new(&path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let r = stage_upload(&s2, &path).and_then(|_| {
            let st = discord(&s2, "upload-start", &json!({ "name": name }));
            if st["ok"] == false { Err(st["err"].as_str().unwrap_or("upload failed").to_owned()) } else { Ok(()) }
        });
        match r {
            Ok(()) => set(json!({ "id": cid, "state": "uploading", "name": name, "at": now_ms() })),
            Err(e) => set(json!({ "id": cid, "state": "error", "err": e, "at": now_ms() })),
        }
    });
    json!({ "ok": true })
}

fn stage_upload(s: &Shared, path: &str) -> Result<(), String> {
    let prep = discord(s, "upload-prep", &Value::Null);
    if prep["ok"] == false {
        return Err(prep["err"].as_str().unwrap_or("discord offline").to_owned());
    }
    let mut g = s.discord.lock().unwrap();
    let c = g.as_mut().ok_or("discord offline")?;
    let doc = c.call("DOM.getDocument", json!({ "depth": 0 }))?;
    let root = doc["root"]["nodeId"].as_i64().ok_or("no document")?;
    let q = c.call("DOM.querySelector", json!({ "nodeId": root, "selector": "#ember-pad-file" }))?;
    let node = q["nodeId"].as_i64().filter(|n| *n > 0).ok_or("file input missing")?;
    c.call("DOM.setFileInputFiles", json!({ "files": [path], "nodeId": node }))?;
    Ok(())
}

fn act(s: &Arc<Shared>, id: &str, arg: &Value) -> Value {
    let simple = match id {
        "mic" => Some("ember mic toggle"),
        "mix-game" => Some("ember mix game"),
        "mix-comms" => Some("ember mix comms"),
        "mix-reset" => Some("ember mix reset"),
        "mode" => Some("ember mode toggle"),
        "vpn" => Some("ember-vpn toggle"),
        _ => None,
    };
    let res = if let Some(cmd) = simple {
        run_detached(cmd);
        json!({ "ok": true })
    } else {
        match id {
            "clip" => {
                run_detached("clip-save");
                s.clip_saved.store(now_ms(), Ordering::Relaxed);
                json!({ "ok": true })
            }
            "deafen" | "camera" => discord(s, id, &Value::Null),
            "dmute" => discord(s, "mute", &Value::Null),
            "dleave" => discord(s, "leave", &Value::Null),
            "djoin" => discord(s, "join", arg),
            "dvol" => discord(s, "vol", arg),
            "dlmute" => discord(s, "lmute", arg),
            "dsbmute" => discord(s, "sbmute", arg),
            "dsound" => discord(s, "sound", arg),
            "dstatus" => discord(s, "status", arg),
            "dcustom" => discord(s, "custom", arg),
            "privacy" => overlay_post(s, "/api/privacy", &json!({ "on": arg.as_bool().unwrap_or(true), "secs": 10 })),
            "stream-mic" => overlay_post(s, "/api/stream-mic", &json!({})),
            "twitch-clip" => {
                let r = overlay_post(s, "/api/clip", &json!({}));
                match r["url"].as_str() {
                    Some(url) => {
                        let d = discord(s, "post", &json!(url));
                        if d["ok"] == false {
                            run_detached(&format!("printf %s '{}' | wl-copy", url.replace('\'', "")));
                            json!({ "ok": true, "url": url, "note": "clip link copied (not in voice)" })
                        } else {
                            json!({ "ok": true, "url": url, "note": "clip link posted in the voice chat" })
                        }
                    }
                    None => r,
                }
            }
            "vol" => {
                let r = match (arg["bus"].as_str(), arg["pct"].as_i64()) {
                    (Some(b), Some(p)) => more::set_volume(b, p),
                    (Some(b), None) if arg["mute"] == true => more::set_mute(b),
                    _ => Err("bad volume request".into()),
                };
                match r {
                    Ok(()) => json!({ "ok": true }),
                    Err(e) => json!({ "ok": false, "err": e }),
                }
            }
            "clip-keep" | "clip-trash" => {
                let cid = arg.as_str().unwrap_or("");
                if !more::clip_exists(cid) || cid.contains('\'') {
                    json!({ "ok": false, "err": "no such clip" })
                } else {
                    run_detached(&format!("ember-clip {} '{cid}'", if id == "clip-keep" { "keep" } else { "discard" }));
                    json!({ "ok": true })
                }
            }
            "clip-send" => clip_send(s, arg.as_str().unwrap_or("")),
            "dstream" | "dstream-side" => {
                let screen = if id == "dstream" { &s.cfg.share_screen } else { &s.cfg.side_screen };
                let live = s.state.lock().unwrap()["discord"]["streaming"] == true;
                if !live {
                    // Arm the share picker: the next screen-share request within 30 s gets this monitor.
                    let _ = fs::write(runtime_dir().join("share-autopick"), screen);
                }
                let r = discord(s, "stream", &Value::Null);
                if r["ok"] != false && !live {
                    *s.stream_screen.lock().unwrap() = screen.clone();
                }
                r
            }
            "shot" => {
                let out = s.cfg.share_screen.replace('\'', "");
                run_detached(&format!(
                    "d=\"$HOME/Pictures/Screenshots\"; mkdir -p \"$d\"; f=\"$d/deck-$(date +%F_%H-%M-%S).png\"; \
                     grim -o '{out}' \"$f\" && wl-copy --type image/png < \"$f\" && \
                     notify-send -a EMBER -i \"$f\" 'Screenshot' 'Main screen copied (and saved in Screenshots)'"
                ));
                s.shot_at.store(now_ms(), Ordering::Relaxed);
                json!({ "ok": true })
            }
            "golive" => {
                let presets = http(&s.cfg.overlay, "GET", "/api/presets", None, Duration::from_secs(3))
                    .ok()
                    .and_then(|(_, b)| serde_json::from_str::<Value>(&b).ok())
                    .unwrap_or(json!([]));
                let list = presets.as_array().cloned().unwrap_or_default();
                let p = list.iter().find(|p| p["name"] == "Last stream").or(list.first()).cloned();
                match p {
                    Some(p) => overlay_post(s, "/api/golive", &p),
                    None => json!({ "ok": false, "err": "no go-live preset saved in magi-overlay yet" }),
                }
            }
            "endstream" => overlay_post(s, "/api/endstream", &json!({})),
            "scene" => {
                let name = arg.as_str().unwrap_or("");
                if !s.cfg.scenes.iter().any(|x| x == name) {
                    json!({ "ok": false, "err": "unknown scene" })
                } else {
                    overlay_post(s, "/api/scene", &json!({ "scene": name }))
                }
            }
            "deck" => {
                let i = arg.as_u64().unwrap_or(u64::MAX);
                match deck_actions().into_iter().find(|a| a["i"].as_u64() == Some(i)) {
                    Some(a) => {
                        run_detached(a["run"].as_str().unwrap_or("true"));
                        json!({ "ok": true })
                    }
                    None => json!({ "ok": false, "err": "no such deck action" }),
                }
            }
            _ => json!({ "ok": false, "err": "unknown action" }),
        }
    };
    eprintln!("ember-pad: {id} {}", if res["ok"] == false { res["err"].to_string() } else { "ok".into() });
    s.kick.store(true, Ordering::Relaxed);
    res
}

// ── HTTP ─────────────────────────────────────────────────────────────────────

struct Req {
    method: String,
    path: String,
    query: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Req {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

fn read_req(conn: &mut TcpStream) -> Option<Req> {
    let mut r = BufReader::new(conn.try_clone().ok()?);
    let mut line = String::new();
    r.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_owned();
    let target = parts.next()?.to_owned();
    let mut headers = Vec::new();
    loop {
        let mut h = String::new();
        if r.read_line(&mut h).ok()? == 0 {
            return None;
        }
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.push((k.trim().to_owned(), v.trim().to_owned()));
        }
        if headers.len() > 100 {
            return None;
        }
    }
    let (path, query) = target.split_once('?').map(|(a, b)| (a.to_owned(), b.to_owned())).unwrap_or((target, String::new()));
    let mut req = Req { method, path, query, headers, body: Vec::new() };
    let len: usize = req.header("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
    if len > 64 * 1024 {
        return None;
    }
    req.body.resize(len, 0);
    r.read_exact(&mut req.body).ok()?;
    Some(req)
}

fn respond(conn: &mut TcpStream, code: u16, ctype: &str, extra: &str, body: &[u8]) {
    let reason = match code {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        _ => "Not Found",
    };
    let head = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n",
        body.len()
    );
    let _ = conn.write_all(head.as_bytes());
    let _ = conn.write_all(body);
}

fn query_get<'a>(q: &'a str, key: &str) -> Option<&'a str> {
    q.split('&').filter_map(|kv| kv.split_once('=')).find(|(k, _)| *k == key).map(|(_, v)| v)
}

fn authed(s: &Shared, req: &Req) -> bool {
    let k = &s.cfg.key;
    query_get(&req.query, "k") == Some(k.as_str())
        || req.header("x-pad-key") == Some(k.as_str())
        || req
            .header("cookie")
            .is_some_and(|c| c.split(';').filter_map(|p| p.trim().split_once('=')).any(|(n, v)| n == "pad" && v == k))
}

fn handle(s: Arc<Shared>, mut conn: TcpStream) {
    let peer_ok = conn.peer_addr().is_ok_and(|a| private(a.ip()));
    let Some(req) = read_req(&mut conn) else { return };
    if !peer_ok {
        return respond(&mut conn, 403, "text/plain", "", b"private network only\n");
    }
    let nostore = "Cache-Control: no-store\r\n";
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/font/regular.ttf") | ("GET", "/font/bold.ttf") => {
            let style = if req.path.contains("bold") { "Bold" } else { "Regular" };
            let file = sh(&["fc-match", "-f", "%{file}", &format!("JetBrainsMono Nerd Font:style={style}")], Duration::from_secs(3));
            match fs::read(file.trim()) {
                Ok(b) => respond(&mut conn, 200, "font/ttf", "Cache-Control: max-age=31536000\r\n", &b),
                Err(_) => respond(&mut conn, 404, "text/plain", "", b"no font\n"),
            }
        }
        ("POST", "/gsi") => {
            let local = conn.peer_addr().is_ok_and(|a| a.ip().is_loopback());
            if local && gsi(&s, &req.body) {
                respond(&mut conn, 200, "text/plain", "", b"");
            } else {
                respond(&mut conn, 403, "text/plain", "", b"no\n");
            }
        }
        _ if !authed(&s, &req) => respond(
            &mut conn,
            401,
            "text/html; charset=utf-8",
            nostore,
            b"<body style='background:#120d0a;color:#d8cbbf;font:20px monospace;padding:40px'>Open the address from <b>ember-pad url</b> on the desktop.</body>",
        ),
        ("GET", "/") | ("GET", "/index.html") => {
            let cookie = format!("{nostore}Set-Cookie: pad={}; Max-Age=315360000; Path=/; HttpOnly; SameSite=Strict\r\n", s.cfg.key);
            respond(&mut conn, 200, "text/html; charset=utf-8", &cookie, PAGE.as_bytes());
        }
        ("GET", "/api/state") => {
            s.last_poll.store(now_ms(), Ordering::Relaxed);
            // First poll after a quiet spell: build the state right away instead of serving a stale one.
            let stale = s.state.lock().unwrap()["time"].as_u64().is_none_or(|t| now_ms().saturating_sub(t) > 5_000);
            if stale {
                let st = gather(&s, replay_running());
                *s.state.lock().unwrap() = st;
            }
            let mut body = s.state.lock().unwrap().clone();
            body["now"] = json!(now_ms()); // for the page's clock-offset estimate
            body["bomb"] = bomb_json(&s.bomb.lock().unwrap());
            respond(&mut conn, 200, "application/json", nostore, body.to_string().as_bytes());
        }
        ("GET", "/api/bomb") => {
            // Long poll: answers as soon as the bomb changes (or after 20 s), so a plant reaches the tablet at once.
            let want = query_get(&req.query, "gen").and_then(|g| g.parse::<u64>().ok());
            let end = Instant::now() + Duration::from_secs(20);
            let mut b = s.bomb.lock().unwrap();
            while want == Some(b.generation) {
                let left = end.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    break;
                }
                b = s.bomb_cv.wait_timeout(b, left).unwrap().0;
            }
            let v = bomb_json(&b);
            drop(b);
            respond(&mut conn, 200, "application/json", nostore, v.to_string().as_bytes());
        }
        ("GET", "/api/people") => {
            let v = discord(&s, "people", &Value::Null);
            respond(&mut conn, 200, "application/json", nostore, v.to_string().as_bytes());
        }
        ("GET", "/api/discord") => {
            s.last_poll.store(now_ms(), Ordering::Relaxed);
            let v = discord(&s, "panel", &Value::Null);
            respond(&mut conn, 200, "application/json", nostore, v.to_string().as_bytes());
        }
        ("GET", "/api/clips") => {
            respond(&mut conn, 200, "application/json", nostore, more::clips().to_string().as_bytes());
        }
        ("GET", "/thumb") => match query_get(&req.query, "id").and_then(more::clip_thumb) {
            Some(b) => respond(&mut conn, 200, "image/jpeg", "Cache-Control: max-age=86400\r\n", &b),
            None => respond(&mut conn, 404, "text/plain", "", b"no thumb\n"),
        },
        ("GET", "/img") => match query_get(&req.query, "u").map(more::unescape).and_then(|u| more::image(&u)) {
            Some(b) => respond(&mut conn, 200, "image/png", "Cache-Control: max-age=86400\r\n", &b),
            None => respond(&mut conn, 404, "text/plain", "", b"no image\n"),
        },
        ("GET", "/api/deck") => {
            let list: Vec<Value> = deck_actions()
                .into_iter()
                .map(|mut a| {
                    a.as_object_mut().map(|o| o.remove("run"));
                    a
                })
                .collect();
            respond(&mut conn, 200, "application/json", nostore, Value::from(list).to_string().as_bytes());
        }
        ("POST", "/api/act") => {
            let v: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
            let Some(id) = v["id"].as_str() else {
                return respond(&mut conn, 400, "application/json", nostore, br#"{"ok":false,"err":"no id"}"#);
            };
            let res = act(&s, id, &v["arg"]);
            respond(&mut conn, 200, "application/json", nostore, res.to_string().as_bytes());
        }
        _ => respond(&mut conn, 404, "text/plain", "", b"not found\n"),
    }
}

/// Tiny HTTP/1.1 client for loopback services (magi-overlay, DevTools). Returns (status, body).
pub fn http(addr: &str, method: &str, path: &str, body: Option<&str>, timeout: Duration) -> Result<(u16, String), String> {
    let sa = addr.parse::<std::net::SocketAddr>().map_err(|e| format!("{addr}: {e}"))?;
    let mut c = TcpStream::connect_timeout(&sa, timeout.min(Duration::from_secs(2))).map_err(|e| e.to_string())?;
    c.set_read_timeout(Some(timeout)).ok();
    c.set_write_timeout(Some(timeout)).ok();
    let body = body.unwrap_or("");
    // X-Magi: magi-overlay refuses control POSTs without it (cross-site request guard).
    let ctype = if method == "POST" { "Content-Type: application/json\r\nX-Magi: 1\r\n" } else { "X-Magi: 1\r\n" };
    let head =
        format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n{ctype}Content-Length: {}\r\n\r\n", body.len());
    c.write_all(head.as_bytes()).and_then(|_| c.write_all(body.as_bytes())).map_err(|e| e.to_string())?;
    // Read until the body is complete: Chromium's DevTools server ignores `Connection: close` and keeps the
    // socket open, so read_to_end would only end at the timeout.
    let mut raw = Vec::new();
    let mut buf = [0u8; 16384];
    loop {
        if let Some(h) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&raw[..h]).to_ascii_lowercase();
            let len = head.lines().find_map(|l| l.strip_prefix("content-length:")).and_then(|v| v.trim().parse::<usize>().ok());
            if len.is_some_and(|n| raw.len() >= h + 4 + n) {
                break;
            }
        }
        match c.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => raw.extend_from_slice(&buf[..n]),
            Err(e) if raw.is_empty() => return Err(e.to_string()),
            Err(_) => break,
        }
    }
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").ok_or("bad response")?;
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let mut rest = &raw[split + 4..];
    let code = head.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
    let chunked = head.lines().any(|l| {
        let l = l.to_ascii_lowercase();
        l.starts_with("transfer-encoding:") && l.contains("chunked")
    });
    if !chunked {
        return Ok((code, String::from_utf8_lossy(rest).into_owned()));
    }
    let mut out = Vec::new();
    while let Some(nl) = rest.windows(2).position(|w| w == b"\r\n") {
        let size =
            usize::from_str_radix(String::from_utf8_lossy(&rest[..nl]).split(';').next().unwrap_or("").trim(), 16).unwrap_or(0);
        if size == 0 || rest.len() < nl + 2 + size {
            break;
        }
        out.extend_from_slice(&rest[nl + 2..nl + 2 + size]);
        rest = &rest[(nl + 4 + size).min(rest.len())..];
    }
    Ok((code, String::from_utf8_lossy(&out).into_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shared() -> Shared {
        Shared {
            cfg: Cfg {
                bind: String::new(),
                discord_port: 0,
                overlay: String::new(),
                scenes: vec![],
                share_screen: String::new(),
                side_screen: String::new(),
                mic: String::new(),
                custom_statuses: vec![],
                key: "k".into(),
            },
            state: Mutex::new(json!({})),
            mic: Mutex::new(None),
            stream_screen: Mutex::new(String::new()),
            vols: Mutex::new(json!({})),
            media: Mutex::new(json!({})),
            game: Mutex::new(Game::default()),
            send: Mutex::new(Value::Null),
            bomb: Mutex::new(Bomb::default()),
            bomb_cv: Condvar::new(),
            discord: Mutex::new(None),
            discord_retry: Mutex::new(None),
            last_poll: AtomicU64::new(0),
            clip_saved: AtomicU64::new(0),
            shot_at: AtomicU64::new(0),
            kick: AtomicBool::new(false),
        }
    }

    fn post(s: &Shared, v: Value) -> bool {
        gsi(s, v.to_string().as_bytes())
    }

    #[test]
    fn bomb_timer_from_gsi() {
        let s = shared();
        assert!(!post(&s, json!({ "auth": { "token": "wrong" } })), "token is checked");
        // plant without phase_countdowns: stamped on arrival, 40 s
        let t0 = now_ms();
        assert!(post(&s, json!({ "auth": { "token": "k" }, "round": { "bomb": "planted" } })));
        let d = s.bomb.lock().unwrap().deadline;
        assert!(d >= t0 + 40_000 && d <= now_ms() + 40_000);
        // the game's countdown takes over; a later (more delayed) estimate never pushes it out
        post(
            &s,
            json!({ "auth": { "token": "k" }, "round": { "bomb": "planted" }, "phase_countdowns": { "phase": "bomb", "phase_ends_in": "39.5" } }),
        );
        let first = s.bomb.lock().unwrap().deadline;
        assert!(first <= now_ms() + 39_500 && s.bomb.lock().unwrap().src == "countdown");
        std::thread::sleep(Duration::from_millis(30));
        post(
            &s,
            json!({ "auth": { "token": "k" }, "round": { "bomb": "planted" }, "phase_countdowns": { "phase": "bomb", "phase_ends_in": "39.5" } }),
        );
        assert_eq!(s.bomb.lock().unwrap().deadline, first);
        // defuse starts, then stops
        post(
            &s,
            json!({ "auth": { "token": "k" }, "round": { "bomb": "planted" }, "phase_countdowns": { "phase": "defuse", "phase_ends_in": "4.9" } }),
        );
        assert!(s.bomb.lock().unwrap().defuse_deadline > 0);
        assert_eq!(s.bomb.lock().unwrap().deadline, first, "bomb deadline kept while defusing");
        post(
            &s,
            json!({ "auth": { "token": "k" }, "round": { "bomb": "planted" }, "phase_countdowns": { "phase": "bomb", "phase_ends_in": "30.0" } }),
        );
        assert_eq!(s.bomb.lock().unwrap().defuse_deadline, 0);
        // round over → defused; next round → cleared
        post(
            &s,
            json!({ "auth": { "token": "k" }, "round": { "bomb": "defused" }, "phase_countdowns": { "phase": "over", "phase_ends_in": "6.0" } }),
        );
        assert_eq!(s.bomb.lock().unwrap().state, "defused");
        let g = s.bomb.lock().unwrap().generation;
        post(
            &s,
            json!({ "auth": { "token": "k" }, "round": {}, "phase_countdowns": { "phase": "freezetime", "phase_ends_in": "15.0" } }),
        );
        let b = s.bomb.lock().unwrap();
        assert!(b.state.is_empty() && b.deadline == 0 && b.generation > g);
    }
}
