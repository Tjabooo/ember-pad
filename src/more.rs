//! Data for the bigger deck pages: audio buses, performance, clips, the idle flag and cached images.

use crate::util::{home, sh};
use serde_json::{Value, json};
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

pub const BUSES: [(&str, &str); 4] =
    [("game", "ember_game"), ("comms", "ember_comms"), ("media", "ember_media"), ("fx", "ember_fx")];

/// Volume (%) and mute of the EMBER buses.
pub fn volumes() -> Value {
    let out = sh(&["pactl", "-f", "json", "list", "sinks"], Duration::from_secs(3));
    let list: Value = serde_json::from_str(&out).unwrap_or(json!([]));
    let mut m = serde_json::Map::new();
    for (key, sink) in BUSES {
        if let Some(s) = list.as_array().into_iter().flatten().find(|s| s["name"] == sink) {
            let pct = s["volume"]
                .as_object()
                .and_then(|o| o.values().next())
                .and_then(|c| c["value_percent"].as_str())
                .and_then(|p| p.trim_end_matches('%').trim().parse::<i64>().ok());
            m.insert(key.into(), json!({ "pct": pct, "muted": s["mute"] }));
        }
    }
    Value::Object(m)
}

pub fn set_volume(bus: &str, pct: i64) -> Result<(), String> {
    let sink = BUSES.iter().find(|(k, _)| *k == bus).map(|(_, s)| *s).ok_or("unknown bus")?;
    let pct = pct.clamp(0, 150);
    if crate::util::ok(&["pactl", "set-sink-volume", sink, &format!("{pct}%")]) { Ok(()) } else { Err("pactl failed".into()) }
}

pub fn set_mute(bus: &str) -> Result<(), String> {
    let sink = BUSES.iter().find(|(k, _)| *k == bus).map(|(_, s)| *s).ok_or("unknown bus")?;
    if crate::util::ok(&["pactl", "set-sink-mute", sink, "toggle"]) { Ok(()) } else { Err("pactl failed".into()) }
}

fn state_dir() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME").map(PathBuf::from).unwrap_or_else(|| home().join(".local/state")).join("ember")
}

/// Latest MangoHud log row (autostart logging into ~/.local/state/ember/mangohud), if a game is logging right now.
fn mangohud() -> Option<(f64, f64)> {
    let dir = state_dir().join("mangohud");
    let newest = fs::read_dir(&dir)
        .ok()?
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "csv"))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .max_by_key(|(t, _)| *t)?;
    if SystemTime::now().duration_since(newest.0).unwrap_or_default() > Duration::from_secs(5) {
        return None;
    }
    let mut f = fs::File::open(&newest.1).ok()?;
    let mut head = vec![0u8; 4096];
    let n = f.read(&mut head).ok()?;
    let head = String::from_utf8_lossy(&head[..n]).to_string();
    let header: Vec<&str> = head.lines().find(|l| l.starts_with("fps,"))?.split(',').collect();
    let len = f.metadata().ok()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(4096))).ok()?;
    let mut tail = String::new();
    f.read_to_string(&mut tail).ok()?;
    let last: Vec<&str> = tail.lines().rev().find(|l| l.split(',').count() == header.len())?.split(',').collect();
    let col = |name: &str| header.iter().position(|h| *h == name).and_then(|i| last.get(i)?.trim().parse::<f64>().ok());
    Some((col("fps")?, col("frametime").unwrap_or(0.0)))
}

/// FPS (MangoHud), CPU/GPU load + temps (magi-overlay's stats), ping/jitter/loss (emberd's probe).
pub fn perf(overlay_stats: &Value) -> Value {
    let net: Value =
        fs::read_to_string(state_dir().join("net.json")).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(Value::Null);
    let fresh = net["time"].as_f64().is_some_and(|t| {
        SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0) - t < 10.0
    });
    let (fps, ft) = mangohud().map(|(a, b)| (Some(a.round()), Some((b * 10.0).round() / 10.0))).unwrap_or((None, None));
    json!({
        "fps": fps, "frametime": ft,
        "cpu": overlay_stats["cpu"], "cpu_temp": overlay_stats["cpu_temp"],
        "gpu": overlay_stats["gpu"], "gpu_temp": overlay_stats["gpu_temp"], "gpu_power": overlay_stats["gpu_power"],
        "ram_used": overlay_stats["ram_used"], "ram_total": overlay_stats["ram_total"],
        "ping": if fresh { net["wan"].clone() } else { Value::Null },
        "jitter": if fresh { net["jitter"].clone() } else { Value::Null },
        "loss": if fresh { net["loss"].clone() } else { Value::Null },
    })
}

fn clips_file() -> PathBuf {
    state_dir().join("clips.json")
}

/// Clips waiting for review first, then the latest kept ones (newest first, 12 max).
pub fn clips() -> Value {
    let all: Value = fs::read_to_string(clips_file()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(json!([]));
    let mut list: Vec<&Value> = all
        .as_array()
        .into_iter()
        .flatten()
        // Kept clips live on the home server's automount: trust clips.json instead of stat()ing it on every
        // poll (a hung or locked share would stall the request; Windows locks the account after bad logins).
        .filter(|c| {
            c["status"] == "kept" || (c["status"] == "new" && c["path"].as_str().is_some_and(|p| PathBuf::from(p).exists()))
        })
        .collect();
    list.sort_by(|a, b| {
        let new = |c: &Value| c["status"] == "new";
        new(b).cmp(&new(a)).then(b["time"].as_f64().unwrap_or(0.0).total_cmp(&a["time"].as_f64().unwrap_or(0.0)))
    });
    Value::from(
        list.into_iter()
            .take(12)
            .map(|c| {
                json!({
                    "id": c["id"], "game": c["game"], "time": c["time"], "duration": c["duration"],
                    "size": c["size"], "status": c["status"], "thumb": c["thumb"].as_str().is_some_and(|t| !t.is_empty()),
                    "cam": c["cam"].as_str().is_some_and(|t| !t.is_empty()),
                })
            })
            .collect::<Vec<_>>(),
    )
}

/// Thumbnail of a clip in clips.json (only paths listed there are served).
pub fn clip_thumb(id: &str) -> Option<Vec<u8>> {
    let all: Value = serde_json::from_str(&fs::read_to_string(clips_file()).ok()?).ok()?;
    let c = all.as_array()?.iter().find(|c| c["id"] == id)?;
    fs::read(c["thumb"].as_str()?).ok()
}

pub fn clip_exists(id: &str) -> bool {
    fs::read_to_string(clips_file())
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .is_some_and(|a| a.as_array().is_some_and(|l| l.iter().any(|c| c["id"] == id)))
}

/// Discord CDN images (avatars, server icons), fetched once with curl and cached as PNG
/// (iOS 9 can't show WebP). Only cdn.discordapp.com is allowed.
pub fn image(url: &str) -> Option<Vec<u8>> {
    if !url.starts_with("https://cdn.discordapp.com/") || url.contains("..") || url.len() > 400 {
        return None;
    }
    let base = url.split('?').next()?;
    let png = format!(
        "{}.png?size=64",
        base.strip_suffix(".webp").or(base.strip_suffix(".png")).or(base.strip_suffix(".gif")).unwrap_or(base)
    );
    let name: String = png.bytes().map(|b| if b.is_ascii_alphanumeric() { b as char } else { '_' }).collect();
    let dir = home().join(".cache/ember-pad/img");
    let file = dir.join(&name[name.len().saturating_sub(120)..]);
    if let Ok(b) = fs::read(&file) {
        return Some(b);
    }
    let _ = fs::create_dir_all(&dir);
    let tmp = file.with_extension("part");
    let out = tmp.to_string_lossy().to_string();
    if !crate::util::ok(&["curl", "-sf", "--max-time", "6", "--max-filesize", "2000000", "-o", &out, &png]) {
        let _ = fs::remove_file(&tmp);
        return None;
    }
    fs::rename(&tmp, &file).ok()?;
    fs::read(&file).ok()
}

/// Percent-decoding for query values.
pub fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => match u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("zz"), 16) {
                Ok(v) => {
                    out.push(v);
                    i += 3;
                    continue;
                }
                Err(_) => out.push(b'%'),
            },
            b'+' => out.push(b' '),
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}
