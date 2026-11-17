// Runs inside Discord's main window via DevTools: function (act, arg) -> object.
// Reads go through Vencord's store lookups (Vencord.Webpack); clicks use the voice panel's
// aria-labels (English client) so Discord's own checks and prompts still run. Checked against the
// Linux client 2026-11-16.
//   state                       mic/deafen/camera/stream flags (polled every second)
//   panel                       voice channels per server, people in your call, soundboard, status, viewers
//   deafen mute leave camera stream          voice panel buttons
//   people                      just the people in your call (CONTROL tab)
//   join <channelId>  vol {id, vol}  lmute <userId>  sbmute <userId>  sound {id, guild}
//   status <online|idle|dnd|invisible>  custom {text, mins} | null  post <text>
//   upload-prep  upload-start {name}         clip upload into the voice channel's chat
function (act, arg) {
  var W = window.Vencord && window.Vencord.Webpack;
  var C = W ? W.Common : {};
  // Common first, then findStore, then a scan of webpack's module cache: Vencord's findStore broke on
  // Discord 0.0.169-era builds (throws "Reflect.get called on non-object"), which emptied the panel.
  function scanStore(n) {
    var cache = window.__emberPadStores || (window.__emberPadStores = {});
    if (cache[n]) return cache[n];
    var c = W && (W.cache || (W.wreq && W.wreq.c));
    function hit(x) {
      try { return x && typeof x === 'object' && typeof x.getName === 'function' && x.getName() === n; } catch (e) { return false; }
    }
    for (var k in c) {
      var e = c[k] && c[k].exports;
      if (!e) continue;
      if (hit(e)) return (cache[n] = e);
      if (typeof e === 'object') for (var p in e) { var v; try { v = e[p]; } catch (_) { continue; } if (hit(v)) return (cache[n] = v); }
    }
    return null;
  }
  function store(n) {
    if (!W) return null;
    try { if (C && C[n]) return C[n]; } catch (e) {}
    try { var s = W.findStore ? W.findStore(n) : null; if (s) return s; } catch (e) {}
    try { return scanStore(n); } catch (e) { return null; }
  }
  function props(n) { try { return W ? W.findByProps(n) : null; } catch (e) { return null; } }
  function call(f) { try { return f(); } catch (e) { return undefined; } }
  function btn(re, root) {
    var b = (root || document).querySelectorAll('button,[role="button"],[role="switch"]');
    for (var i = 0; i < b.length; i++) {
      if (re.test(b[i].getAttribute('aria-label') || '')) return b[i];
    }
    return null;
  }
  function byText(re) {
    var d = document.querySelectorAll('[role="dialog"] button');
    for (var i = 0; i < d.length; i++) {
      if (re.test((d[i].textContent || '').trim()) && !d[i].disabled) return d[i];
    }
    return null;
  }
  function setting(name) {
    try { return Vencord.Api.UserSettings.getUserSettingLazy('status', name); } catch (e) { return null; }
  }
  var me = store('MediaEngineStore'), sel = store('SelectedChannelStore'), ss = store('ApplicationStreamingStore');
  var users = store('UserStore'), members = store('GuildMemberStore'), chans = store('ChannelStore');
  var voiceStates = store('VoiceStateStore'), speaking = store('SpeakingStore');
  function myId() { return call(function () { return users.getCurrentUser().id; }); }
  function snap() {
    var vc = sel ? call(function () { return sel.getVoiceChannelId(); }) : undefined;
    if (vc === undefined) vc = btn(/^disconnect$/i) ? 'dom' : null;
    var deafBtn = btn(/^(un)?deafen$/i);
    var deaf = me ? call(function () { return me.isSelfDeaf(); }) : undefined;
    if (deaf === undefined && deafBtn) {
      deaf = /^undeafen$/i.test(deafBtn.getAttribute('aria-label')) || deafBtn.getAttribute('aria-checked') === 'true';
    }
    var mute = me ? call(function () { return me.isSelfMute(); }) : undefined;
    var video = me ? call(function () { return me.isVideoEnabled(); }) : undefined;
    if (video === undefined) video = !!btn(/turn off camera/i);
    var live = ss ? call(function () { return ss.getCurrentUserActiveStream(); }) : undefined;
    var streaming = live === undefined ? !!btn(/stop stream/i) : !!live;
    var viewers = live ? (call(function () { return ss.getViewerIds(live); }) || []).length : 0;
    return { ok: true, vencord: !!W, voice: !!vc, deaf: !!deaf, mute: !!mute, video: !!video, streaming: streaming,
             viewers: viewers, upload: window.__emberPadUp || null };
  }
  function avatar(u, gid) {
    var url = call(function () { return u.getAvatarURL(gid, 64, false); });
    return typeof url === 'string' && url.indexOf('https://') === 0 ? url : null;
  }
  function uname(u, gid) {
    var m = gid && members ? call(function () { return members.getMember(gid, u.id); }) : null;
    return (m && m.nick) || u.globalName || u.username;
  }
  function person(uid, gid, st) {
    var u = call(function () { return users.getUser(uid); });
    if (!u) return null;
    return {
      id: uid, name: uname(u, gid), avatar: avatar(u, gid),
      speaking: !!(speaking && call(function () { return speaking.isSpeaking(uid); })),
      mute: !!(st && (st.selfMute || st.mute)), deaf: !!(st && (st.selfDeaf || st.deaf)),
      video: !!(st && st.selfVideo), stream: !!(st && st.selfStream)
    };
  }
  function voiceChannel() {
    var vc = sel ? call(function () { return sel.getVoiceChannelId(); }) : null;
    return vc ? call(function () { return chans.getChannel(vc); }) : null;
  }

  if (act === 'state') return snap();

  function callPeople(cur, meId) {
    var out = [];
    if (!cur) return out;
    var sbs = store('SoundboardStore');
    var states = call(function () { return voiceStates.getVoiceStatesForChannel(cur.id); }) || {};
    for (var id in states) {
      if (id === meId) continue;
      var pp = person(id, cur.guild_id, states[id]);
      if (!pp) continue;
      pp.volume = Math.round(call(function () { return me.getLocalVolume(id); }) || 100);
      pp.localMute = !!call(function () { return me.isLocalMute(id); });
      pp.soundMuted = !!(sbs && call(function () { return sbs.isLocalSoundboardMuted(id); }));
      out.push(pp);
    }
    return out;
  }

  if (act === 'people') {
    var pc = voiceChannel();
    return { ok: true, voice: pc ? { id: pc.id, name: pc.name, guild: pc.guild_id } : null, people: callPeople(pc, myId()) };
  }

  if (act === 'panel') {
    var meId = myId(), cur = voiceChannel();
    var gs = store('GuildStore'), gcs = store('GuildChannelStore'), sgs = store('SortedGuildStore');
    var ids = call(function () { return sgs.getFlattenedGuildIds(); }) || call(function () { return Object.keys(gs.getGuilds()); }) || [];
    var guilds = [];
    for (var i = 0; i < ids.length && guilds.length < 20; i++) {
      var g = call(function () { return gs.getGuild(ids[i]); });
      if (!g) continue;
      var voc = (call(function () { return gcs.getChannels(g.id).VOCAL; }) || []).slice(0, 14);
      if (!voc.length) continue;
      var list = [], busy = 0;
      for (var j = 0; j < voc.length; j++) {
        var ch = voc[j].channel;
        var vs = call(function () { return voiceStates.getVoiceStatesForChannel(ch.id); }) || {};
        var ppl = [];
        for (var uid in vs) {
          var p = person(uid, g.id, vs[uid]);
          if (p) ppl.push({ id: p.id, name: p.name, avatar: p.avatar, speaking: p.speaking });
        }
        busy += ppl.length;
        list.push({ id: ch.id, name: ch.name, users: ppl });
      }
      guilds.push({ id: g.id, name: g.name, icon: g.icon ? 'https://cdn.discordapp.com/icons/' + g.id + '/' + g.icon + '.png?size=64' : null,
                    busy: busy, channels: list });
    }
    var people = callPeople(cur, meId);
    var sb = store('SoundboardStore'), sounds = [], seen = {};
    var all = call(function () { return sb.getSounds(); });
    var favs = call(function () { return sb.getFavorites(); });
    function addSound(s) {
      if (!s || seen[s.soundId] || sounds.length >= 30) return;
      seen[s.soundId] = 1;
      sounds.push({ id: s.soundId, guild: s.guildId, name: s.name, emoji: s.emojiName || null });
    }
    if (all && all.forEach) {
      all.forEach(function (arr) { (arr || []).forEach(function (s) { if (favs && favs.has && favs.has(s.soundId)) addSound(s); }); });
      if (cur && all.get) (all.get(cur.guild_id) || []).forEach(addSound);
      all.forEach(function (arr, gid) { if (!/^\d+$/.test(String(gid))) (arr || []).forEach(addSound); });
    }
    var live = ss ? call(function () { return ss.getCurrentUserActiveStream(); }) : null;
    var viewers = [];
    if (live) {
      (call(function () { return ss.getViewerIds(live); }) || []).forEach(function (v) {
        var u = call(function () { return users.getUser(v); });
        if (u) viewers.push({ id: v, name: uname(u, cur && cur.guild_id), avatar: avatar(u, cur && cur.guild_id) });
      });
    }
    var ps = store('PresenceStore'), cs = setting('customStatus');
    var custom = cs ? call(function () { return cs.getSetting(); }) : null;
    return {
      ok: true, voice: cur ? { id: cur.id, name: cur.name, guild: cur.guild_id } : null,
      guilds: guilds, people: people, sounds: sounds, viewers: viewers,
      status: call(function () { return ps.getStatus(meId); }) || null,
      custom: custom && custom.text ? custom.text : null
    };
  }

  var s = snap();
  if (act === 'deafen') {
    var d = btn(/^(un)?deafen$/i);
    if (!d) return { ok: false, err: 'deafen button not found' };
    d.click();
  } else if (act === 'mute') {
    var mu = btn(/^(un)?mute$/i);
    if (!mu) return { ok: false, err: 'mute button not found' };
    mu.click();
  } else if (act === 'leave') {
    if (!s.voice) return { ok: false, err: 'not in a voice channel' };
    var dc = btn(/^disconnect$/i);
    if (!dc) return { ok: false, err: 'disconnect button not found' };
    dc.click();
  } else if (act === 'camera') {
    if (!s.voice) return { ok: false, err: 'join a voice channel first' };
    var c = btn(s.video ? /turn off camera/i : /turn on camera/i) || btn(/camera/i);
    if (!c) return { ok: false, err: 'camera button not found' };
    c.click();
  } else if (act === 'stream') {
    if (!s.voice) return { ok: false, err: 'join a voice channel first' };
    if (s.streaming) {
      var stop = btn(/stop stream/i);
      if (!stop) return { ok: false, err: 'stop button not found' };
      stop.click();
    } else {
      var share = btn(/share your screen|share screen|go live/i);
      if (!share) return { ok: false, err: 'share button not found' };
      share.click();
      // Picker modal: "Share entire screen" → "Make Selection"; the xdg portal then asks
      // ember-share-picker (armed with the monitor) and the stream starts at the saved quality.
      // Older clients end in a "Go Live" button instead.
      var tries = 0;
      var t = setInterval(function () {
        tries++;
        var dlg = document.querySelector('[role="dialog"]');
        if (dlg) {
          var radios = dlg.querySelectorAll('[role="radio"]');
          for (var i = 0; i < radios.length; i++) {
            if (/entire screen/i.test(radios[i].textContent || '') && radios[i].getAttribute('aria-checked') !== 'true') radios[i].click();
          }
        }
        var go = byText(/^make selection$/i) || byText(/^go live$/i);
        if (go) { go.click(); clearInterval(t); }
        else if (tries > 50) clearInterval(t);
      }, 250);
    }
  } else if (act === 'join') {
    var va = props('selectVoiceChannel');
    if (!va) return { ok: false, err: 'voice actions not found' };
    va.selectVoiceChannel(String(arg));
  } else if (act === 'vol' || act === 'lmute') {
    var la = props('setLocalVolume');
    if (!la) return { ok: false, err: 'local volume actions not found' };
    if (act === 'vol') la.setLocalVolume(String(arg.id), Math.max(0, Math.min(200, Number(arg.vol) || 0)), 'default');
    else la.toggleLocalMute(String(arg), 'default');
  } else if (act === 'sbmute') {
    var sa = props('toggleLocalSoundboardMute');
    if (!sa) return { ok: false, err: 'soundboard mute action not found' };
    sa.toggleLocalSoundboardMute(String(arg));
  } else if (act === 'sound') {
    var vch = voiceChannel();
    if (!vch) return { ok: false, err: 'join a voice channel first' };
    var body = { sound_id: String(arg.id) };
    if (/^\d+$/.test(String(arg.guild || ''))) body.source_guild_id = String(arg.guild);
    C.RestAPI.post({ url: '/channels/' + vch.id + '/send-soundboard-sound', body: body })
      .catch(function (e) { window.__emberPadErr = 'soundboard: ' + ((e && e.body && e.body.message) || e); });
  } else if (act === 'status') {
    var st = setting('status');
    if (!st || !/^(online|idle|dnd|invisible)$/.test(String(arg))) return { ok: false, err: 'status setting not found' };
    st.updateSetting(String(arg));
  } else if (act === 'custom') {
    var cst = setting('customStatus');
    if (!cst) return { ok: false, err: 'custom status setting not found' };
    if (arg && arg.text) {
      var v = { text: String(arg.text).slice(0, 128) };
      if (arg.mins) v.expiresAtMs = String(Date.now() + arg.mins * 60000);
      cst.updateSetting(v);
    } else {
      cst.updateSetting(undefined);
    }
  } else if (act === 'post') {
    var pch = voiceChannel();
    if (!pch) return { ok: false, err: 'join a voice channel first (links go to its chat)' };
    C.RestAPI.post({ url: '/channels/' + pch.id + '/messages', body: { content: String(arg).slice(0, 2000) } })
      .catch(function (e) { window.__emberPadErr = 'post: ' + ((e && e.body && e.body.message) || e); });
  } else if (act === 'upload-prep') {
    if (!voiceChannel()) return { ok: false, err: 'join a voice channel first (clips go to its chat)' };
    var inp = document.getElementById('ember-pad-file');
    if (!inp) {
      inp = document.createElement('input');
      inp.type = 'file'; inp.id = 'ember-pad-file'; inp.style.display = 'none';
      document.body.appendChild(inp);
    }
    inp.value = '';
  } else if (act === 'upload-start') {
    // Discord's own large-file path: reserve an upload URL, PUT the file there, then post the
    // message that references it. The File comes from DOM.setFileInputFiles, so the browser reads it
    // straight from disk.
    var uch = voiceChannel(), f = (document.getElementById('ember-pad-file') || {}).files;
    f = f && f[0];
    if (!uch || !f) return { ok: false, err: !uch ? 'not in a voice channel' : 'no file staged' };
    var up = window.__emberPadUp = { state: 'uploading', name: arg && arg.name || f.name, pct: 0, size: f.size, at: Date.now() };
    C.RestAPI.post({ url: '/channels/' + uch.id + '/attachments', body: { files: [{ filename: f.name, file_size: f.size, id: '0' }] } })
      .then(function (r) {
        var a = r.body.attachments[0];
        return new Promise(function (res, rej) {
          var x = new XMLHttpRequest();
          x.open('PUT', a.upload_url, true);
          x.upload.onprogress = function (e) { if (e.lengthComputable) up.pct = Math.round(e.loaded * 100 / e.total); };
          x.onload = function () { x.status < 300 ? res(a) : rej('upload HTTP ' + x.status); };
          x.onerror = function () { rej('upload failed'); };
          x.send(f);
        });
      })
      .then(function (a) {
        return C.RestAPI.post({ url: '/channels/' + uch.id + '/messages', body: {
          content: '', attachments: [{ id: '0', filename: f.name, uploaded_filename: a.upload_filename }] } });
      })
      .then(function () { up.state = 'sent'; up.pct = 100; up.at = Date.now(); })
      .catch(function (e) { up.state = 'error'; up.err = String((e && e.body && e.body.message) || (e && e.message) || e); up.at = Date.now(); });
  } else {
    return { ok: false, err: 'unknown act ' + act };
  }
  if (window.__emberPadErr) { var er = window.__emberPadErr; window.__emberPadErr = null; s.lastErr = er; }
  s.acted = act;
  return s;
}
