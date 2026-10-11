'use strict';

/* video sync play client.
 *
 * The server never says "play now" / "pause now". It publishes a timeline:
 *   { playing, position, anchor }  -- position applies at epoch ms `anchor`.
 * We derive our own target playhead from that every 200ms and nudge the
 * <video> element towards it. A peer that lags, stalls or reconnects therefore
 * converges on its own with no catch-up protocol.
 *
 * Small drift is corrected by bending playbackRate (inaudible); large drift by
 * seeking. The user's wishes ("intent") are only ever sent upstream -- what the
 * element actually does is decided by room state alone, so the two ends cannot
 * disagree about who is in charge.
 */

const $ = (s) => document.querySelector(s);

// Drift thresholds, in seconds.
const SOFT_DRIFT = 0.10;  // below this: leave it alone
const HARD_DRIFT = 0.60;  // above this: seek instead of glide
const MAX_RATE_TRIM = 0.07;

const el = {
  setup: $('#setup'), name: $('#name'), drop: $('#drop'), file: $('#file'),
  browse: $('#browse'), lastfile: $('#lastfile'), setupStatus: $('#setup-status'),
  player: $('#player'), stage: $('#stage'), video: $('#video'),
  gate: $('#gate'), gateText: $('#gate-text'), countdown: $('#countdown'),
  play: $('#play'), playIcon: $('#play-icon'), time: $('#time'), duration: $('#duration'),
  bar: $('#bar'), progress: $('#progress'), buffered: $('#buffered'),
  knob: $('#knob'), scrub: $('#scrub'),
  mute: $('#mute'), volume: $('#volume'), full: $('#full'),
  subs: $('#subs'), subfile: $('#subfile'),
  linkState: $('#link-state'), linkText: $('#link-text'), drift: $('#drift'),
  peers: $('#peers'), warn: $('#warn'), log: $('#log'),
  chatForm: $('#chat-form'), chatInput: $('#chat-input'), swap: $('#swap'),
  osd: $('#osd'), help: $('#help'), helpbtn: $('#helpbtn'),
};

const PLAY_PATH = 'M8 5v14l11-7z';
const PAUSE_PATH = 'M6 5h4v14H6zm8 0h4v14h-4z';

const room = {
  code: (location.hash || '').replace(/^#/, '').toUpperCase(),
  id: null,
  state: { playing: false, position: 0, anchor: Date.now(), peers: [] },
  // Clock offset against the server, estimated from the lowest-RTT ping.
  offset: 0,
  bestRtt: Infinity,
};

let ws = null;
let wsReady = false;
let retry = 0;
let myFile = null;
let objectUrl = null;
let dragging = false;
let lastLocalAction = 0;   // guards our own programmatic play/pause/seek
let starvedSince = 0;
let stallSent = false;
let needsGesture = false;
let knownPeers = new Map();

// ------------------------------------------------------------------- helpers

const serverNow = () => Date.now() + room.offset;
const clamp = (v, lo, hi) => Math.min(hi, Math.max(lo, v));

function fmt(t) {
  if (!isFinite(t) || t < 0) t = 0;
  const h = Math.floor(t / 3600);
  const m = Math.floor((t % 3600) / 60);
  const s = Math.floor(t % 60);
  return h ? `${h}:${String(m).padStart(2, '0')}:${String(s).padStart(2, '0')}`
           : `${m}:${String(s).padStart(2, '0')}`;
}

function send(msg) {
  if (ws && wsReady) ws.send(JSON.stringify(msg));
}

function note(text, cls) {
  const li = document.createElement('li');
  if (cls) li.className = cls;
  li.textContent = text;
  el.log.appendChild(li);
  while (el.log.children.length > 200) el.log.firstChild.remove();
  el.log.scrollTop = el.log.scrollHeight;
}

function say(from, text) {
  const li = document.createElement('li');
  const b = document.createElement('b');
  b.textContent = from + ': ';
  li.append(b, document.createTextNode(text));
  el.log.appendChild(li);
  el.log.scrollTop = el.log.scrollHeight;
}

/* Our own play/pause/seek calls fire the same DOM events a user would. Rather
 * than juggling flags across async boundaries, ignore element events for a
 * moment after we touch it. */
const touched = () => { lastLocalAction = Date.now(); };
const recentlyTouched = () => Date.now() - lastLocalAction < 250;

// ------------------------------------------------------------ timeline maths

/** Where the room's playhead should be right now. */
function target() {
  const st = room.state;
  if (!st.playing) return st.position;
  return st.position + Math.max(0, serverNow() - st.anchor) / 1000;
}

/** True once the shared countdown has elapsed and the clock is really moving. */
function rolling() {
  return room.state.playing && serverNow() >= room.state.anchor;
}

/** Peers other than us who are holding the room. */
function blockers() {
  return room.state.peers.filter(
    (p) => p.id !== room.id && (p.paused || p.stalled || p.silent));
}

function iAmPaused() {
  const me = room.state.peers.find((p) => p.id === room.id);
  return me ? me.paused : true;
}

// ------------------------------------------------------------------ the loop

function syncTick() {
  const v = el.video;
  if (!myFile || !v.duration) return;

  const tgt = target();
  const want = rolling();

  if (dragging) {
    // Hands off while the user is scrubbing.
  } else if (want) {
    if (v.ended) {
      // Calling play() on a finished element restarts it from the top in some
      // browsers. Sit still and let the `ended` handler stop the room.
    } else if (v.paused) {
      if (Math.abs(v.currentTime - tgt) > 0.3) seekLocal(tgt);
      play();
    } else {
      const d = v.currentTime - tgt;            // + means we are ahead
      if (Math.abs(d) > HARD_DRIFT) {
        seekLocal(tgt);
        v.playbackRate = 1;
      } else if (Math.abs(d) > SOFT_DRIFT) {
        // Glide back over a few seconds instead of jumping.
        v.playbackRate = clamp(1 - d * 0.4, 1 - MAX_RATE_TRIM, 1 + MAX_RATE_TRIM);
      } else {
        v.playbackRate = 1;
      }
      el.drift.textContent = (d >= 0 ? '+' : '') + d.toFixed(2) + 's';
    }
  } else {
    v.playbackRate = 1;
    if (!v.paused) { touched(); v.pause(); }
    if (Math.abs(v.currentTime - tgt) > 0.25) seekLocal(tgt);
    el.drift.textContent = '';
  }

  // Tell the room when our decoder is starving, so it waits for us.
  const starved = want && v.readyState < 3;
  if (starved) {
    if (!starvedSince) starvedSince = Date.now();
    if (Date.now() - starvedSince > 400) setStall(true);
  } else {
    starvedSince = 0;
    setStall(false);
  }

  paint(tgt);
}

function setStall(s) {
  if (s === stallSent) return;
  stallSent = s;
  send({ t: 'stall', stalled: s });
}

function seekLocal(t) {
  touched();
  try { el.video.currentTime = clamp(t, 0, el.video.duration || t); } catch (_) {}
}

function play() {
  touched();
  const p = el.video.play();
  if (p && p.catch) {
    p.catch(() => {
      // Autoplay policy: we need a click before audio may start.
      needsGesture = true;
      setStall(true);
      renderGate();
    });
  }
}

// --------------------------------------------------------------- user intent

function wantPlay() {
  if (!myFile) return;
  needsGesture = false;
  send({ t: 'resume', position: el.video.currentTime });
}

function wantPause() {
  if (!myFile) return;
  send({ t: 'pause', position: el.video.currentTime });
}

function toggle() {
  if (iAmPaused() || !room.state.playing) wantPlay();
  else wantPause();
}

function wantSeek(t) {
  if (!myFile) return;
  send({ t: 'seek', position: clamp(t, 0, el.video.duration || 0) });
}

function nudge(delta) {
  wantSeek(target() + delta);
}

// ------------------------------------------------------------------ painting

function paint(tgt) {
  const v = el.video;
  const dur = v.duration || 0;
  const pos = dragging ? v.currentTime : clamp(tgt, 0, dur || tgt);

  el.time.textContent = fmt(pos);
  el.duration.textContent = fmt(dur);

  const pct = dur ? (pos / dur) * 100 : 0;
  el.progress.style.width = pct + '%';
  el.knob.style.left = pct + '%';

  let buffered = 0;
  for (let i = 0; i < v.buffered.length; i++) {
    if (v.buffered.start(i) <= pos && pos <= v.buffered.end(i)) buffered = v.buffered.end(i);
  }
  el.buffered.style.width = (dur ? (buffered / dur) * 100 : 0) + '%';

  el.playIcon.firstElementChild.setAttribute('d', rolling() ? PAUSE_PATH : PLAY_PATH);

  // Shared countdown, only when it is long enough to be worth showing.
  const lead = room.state.playing ? room.state.anchor - serverNow() : 0;
  if (lead > 900) {
    el.countdown.textContent = Math.ceil(lead / 1000);
    el.countdown.classList.remove('hidden');
  } else {
    el.countdown.classList.add('hidden');
  }
}

function renderGate() {
  if (needsGesture) {
    el.gateText.textContent = 'click anywhere to allow playback';
    el.gate.classList.remove('hidden');
    return;
  }
  const held = blockers();
  if (held.length && !room.state.playing) {
    const who = held.map((p) => p.name + holdReason(p)).join(', ');
    el.gateText.textContent = `waiting for ${who}`;
    el.gate.classList.remove('hidden');
  } else {
    el.gate.classList.add('hidden');
  }
}

/** Why a peer is holding the room, for the overlay text. */
function holdReason(p) {
  if (p.silent) return ' (not responding)';
  if (p.stalled && !p.paused) return ' (buffering)';
  return '';
}

function renderPeers() {
  el.peers.textContent = '';
  let mismatch = null;

  for (const p of room.state.peers) {
    const li = document.createElement('li');
    const held = p.paused || p.stalled || p.silent;
    li.className = 'peer ' + (held ? 'held' : 'playing');

    const name = document.createElement('span');
    name.className = 'peer-name' + (p.id === room.id ? ' me' : '');
    name.textContent = p.id === room.id ? p.name + ' (you)' : p.name;

    const state = document.createElement('span');
    state.className = 'peer-state';
    state.textContent = p.silent ? 'not responding'
      : p.stalled && !p.paused ? 'buffering'
      : p.paused ? 'paused'
      : room.state.playing ? 'playing' : 'ready';

    li.append(name, state);
    el.peers.appendChild(li);

    if (p.mismatch && p.file) mismatch = p;
  }

  if (mismatch) {
    el.warn.textContent =
      `${mismatch.name} has a different file (${mismatch.file.name}, ${fmt(mismatch.file.duration)}). ` +
      `Positions will line up but you will not be seeing the same frame.`;
    el.warn.classList.remove('hidden');
  } else {
    el.warn.classList.add('hidden');
  }

  // Join / leave notices, derived from successive peer lists.
  const now = new Map(room.state.peers.map((p) => [p.id, p.name]));
  if (knownPeers.size) {
    for (const [id, n] of now) if (!knownPeers.has(id)) note(`${n} joined`, 'sys');
    for (const [id, n] of knownPeers) if (!now.has(id)) note(`${n} left`, 'sys');
  }
  knownPeers = now;
}

// ----------------------------------------------------------------- transport

function connect() {
  const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
  ws = new WebSocket(`${proto}//${location.host}/ws`);

  ws.onopen = () => {
    wsReady = true;
    retry = 0;
    el.linkState.className = 'dot up';
    el.linkText.textContent = 'connected';
    send({
      t: 'hello',
      code: room.code,
      name: el.name.value.trim() || 'guest',
      file: myFile ? fileInfo() : null,
    });
    ping();
  };

  ws.onmessage = (ev) => {
    let m;
    try { m = JSON.parse(ev.data); } catch (_) { return; }
    switch (m.t) {
      case 'welcome':
        room.id = m.id;
        break;
      case 'state':
        room.state = { playing: m.playing, position: m.position, anchor: m.anchor, peers: m.peers };
        renderPeers();
        renderGate();
        syncTick();
        break;
      case 'pong': {
        const t1 = Date.now();
        const rtt = t1 - m.c;
        if (rtt <= room.bestRtt) {
          room.bestRtt = rtt;
          // Server timestamp was taken ~rtt/2 ago.
          room.offset = m.s + rtt / 2 - t1;
        }
        break;
      }
      case 'chat':
        say(m.from, m.text);
        break;
      case 'error':
        wsReady = false;
        el.linkState.className = 'dot down';
        el.linkText.textContent = m.message;
        el.setupStatus.textContent =
          m.message + '. Use the full link including the #CODE at the end.';
        retry = 99;  // don't hammer on a bad code
        break;
    }
  };

  ws.onclose = () => {
    wsReady = false;
    el.linkState.className = 'dot down';
    if (retry < 99) {
      const wait = Math.min(5000, 300 * 2 ** retry++);
      el.linkText.textContent = 'reconnecting…';
      setTimeout(connect, wait);
    }
  };

  ws.onerror = () => { el.linkState.className = 'dot down'; };
}

function ping() {
  send({ t: 'ping', c: Date.now() });
  // Let the best-RTT estimate age out so a one-off lucky sample does not stick.
  if (isFinite(room.bestRtt)) room.bestRtt *= 1.08;
}

// --------------------------------------------------------------------- files

function fileInfo() {
  return { name: myFile.name, size: myFile.size, duration: el.video.duration || 0 };
}

function loadFile(f) {
  if (!f) return;
  if (!/^video\//.test(f.type) && !/\.(mp4|mkv|webm|m4v|mov)$/i.test(f.name)) {
    el.setupStatus.textContent = 'That does not look like a video file.';
    return;
  }

  myFile = f;
  if (objectUrl) URL.revokeObjectURL(objectUrl);
  objectUrl = URL.createObjectURL(f);

  el.setupStatus.textContent = 'loading…';
  el.video.src = objectUrl;

  el.video.onloadedmetadata = () => {
    try { localStorage.setItem('vsp.file', f.name); } catch (_) {}
    el.setup.classList.add('hidden');
    el.player.classList.remove('hidden');
    send({ t: 'file', file: fileInfo() });
    note(`loaded ${f.name}`, 'sys');
    syncTick();
  };

  el.video.onerror = () => {
    el.setupStatus.textContent =
      `Your browser cannot decode ${f.name}. Likely HEVC/H.265, AC-3 audio or an exotic ` +
      `MKV. Remux or transcode it to H.264 + AAC (see the README) and try again.`;
    myFile = null;
  };
}

// -------------------------------------------------------------- subtitles

function srtToVtt(text) {
  return 'WEBVTT\n\n' + text
    .replace(/\r/g, '')
    .replace(/^\s*\d+\s*$/gm, '')
    .replace(/(\d{2}:\d{2}:\d{2}),(\d{3})/g, '$1.$2')
    .trim() + '\n';
}

function loadSubs(f) {
  if (!f) return;
  const reader = new FileReader();
  reader.onload = () => {
    const raw = String(reader.result);
    const vtt = /\.vtt$/i.test(f.name) ? raw : srtToVtt(raw);
    const url = URL.createObjectURL(new Blob([vtt], { type: 'text/vtt' }));
    for (const t of [...el.video.querySelectorAll('track')]) t.remove();
    const track = document.createElement('track');
    track.kind = 'subtitles';
    track.label = f.name;
    track.src = url;
    track.default = true;
    el.video.appendChild(track);
    track.addEventListener('load', () => { el.video.textTracks[0].mode = 'showing'; });
    note(`subtitles: ${f.name}`, 'sys');
  };
  reader.readAsText(f);
}

// ---------------------------------------------------------------- seek bar

function barPosition(ev) {
  const r = el.bar.getBoundingClientRect();
  return clamp((ev.clientX - r.left) / r.width, 0, 1) * (el.video.duration || 0);
}

el.bar.addEventListener('pointerdown', (ev) => {
  if (!el.video.duration) return;
  dragging = true;
  el.bar.setPointerCapture(ev.pointerId);
  el.scrub.classList.remove('hidden');
  previewSeek(ev);
});

el.bar.addEventListener('pointermove', (ev) => {
  if (!el.video.duration) return;
  if (dragging) previewSeek(ev);
  else showScrubHint(ev);
});

el.bar.addEventListener('pointerup', (ev) => {
  if (!dragging) return;
  dragging = false;
  el.scrub.classList.add('hidden');
  wantSeek(barPosition(ev));
});

el.bar.addEventListener('pointerleave', () => {
  if (!dragging) el.scrub.classList.add('hidden');
});

function previewSeek(ev) {
  const t = barPosition(ev);
  seekLocal(t);
  showScrubHint(ev, t);
}

function showScrubHint(ev, t) {
  const pos = t === undefined ? barPosition(ev) : t;
  const r = el.bar.getBoundingClientRect();
  el.scrub.textContent = fmt(pos);
  el.scrub.style.left = clamp(ev.clientX - r.left, 24, r.width - 24) + 'px';
  el.scrub.classList.remove('hidden');
}

el.bar.addEventListener('keydown', (ev) => {
  if (ev.key === 'ArrowLeft') { nudge(-5); ev.preventDefault(); }
  if (ev.key === 'ArrowRight') { nudge(5); ev.preventDefault(); }
});

// ------------------------------------------- on-screen display & local view

let osdTimer = null;
/** VLC-style transient readout: volume, jump size, aspect. */
function osd(text) {
  el.osd.textContent = text;
  el.osd.classList.add('show');
  clearTimeout(osdTimer);
  osdTimer = setTimeout(() => el.osd.classList.remove('show'), 900);
}

function setVolume(delta) {
  const v = clamp(el.video.volume + delta, 0, 1);
  el.video.volume = v;
  el.video.muted = v === 0;
  el.volume.value = v;
  try { localStorage.setItem('vsp.volume', String(v)); } catch (_) {}
  osd(`volume ${Math.round(v * 100)}%`);
}

/** Room-wide jump, with the OSD showing which way and how far. */
function jump(delta) {
  const sign = delta > 0 ? '+' : '-';
  const n = Math.abs(delta);
  osd(`${sign}${n >= 60 ? (n / 60) + ' min' : n + 's'}`);
  nudge(delta);
}

const FITS = ['contain', 'cover', 'fill'];
let fitIndex = 0;
function cycleAspect() {
  fitIndex = (fitIndex + 1) % FITS.length;
  el.video.style.objectFit = FITS[fitIndex];
  osd('aspect: ' + FITS[fitIndex]);
}

function toggleSubs() {
  const tracks = el.video.textTracks;
  if (!tracks.length) { osd('no subtitles loaded'); return; }
  const on = tracks[0].mode === 'showing';
  tracks[0].mode = on ? 'hidden' : 'showing';
  osd(on ? 'subtitles off' : 'subtitles on');
}

function toggleHelp(force) {
  const show = force === undefined ? el.help.classList.contains('hidden') : force;
  el.help.classList.toggle('hidden', !show);
}

// ------------------------------------------------------------------- wiring

el.browse.addEventListener('click', (ev) => { ev.stopPropagation(); el.file.click(); });
el.drop.addEventListener('click', () => el.file.click());
el.file.addEventListener('change', () => loadFile(el.file.files[0]));

for (const type of ['dragenter', 'dragover']) {
  el.drop.addEventListener(type, (ev) => { ev.preventDefault(); el.drop.classList.add('over'); });
}
for (const type of ['dragleave', 'drop']) {
  el.drop.addEventListener(type, () => el.drop.classList.remove('over'));
}
el.drop.addEventListener('drop', (ev) => {
  ev.preventDefault();
  loadFile(ev.dataTransfer.files[0]);
});
// Dropping anywhere on the page works too.
document.addEventListener('dragover', (ev) => ev.preventDefault());
document.addEventListener('drop', (ev) => {
  ev.preventDefault();
  const f = ev.dataTransfer.files[0];
  if (!f) return;
  if (/\.(srt|vtt)$/i.test(f.name)) loadSubs(f);
  else loadFile(f);
});

el.name.addEventListener('change', () => {
  const n = el.name.value.trim();
  try { localStorage.setItem('vsp.name', n); } catch (_) {}
  // Reconnect so the room picks up the new name.
  if (ws && wsReady) ws.close();
});

el.play.addEventListener('click', toggle);
el.stage.addEventListener('click', (ev) => {
  if (ev.target === el.video || ev.target === el.gate || ev.target.closest('.gate-card')) {
    if (needsGesture) { needsGesture = false; wantPlay(); }
    else toggle();
  }
});

el.mute.addEventListener('click', () => {
  el.video.muted = !el.video.muted;
  el.mute.style.opacity = el.video.muted ? 0.45 : 0.9;
});
el.volume.addEventListener('input', () => {
  el.video.volume = Number(el.volume.value);
  el.video.muted = el.video.volume === 0;
});

el.full.addEventListener('click', () => {
  if (document.fullscreenElement) document.exitFullscreen();
  else el.stage.requestFullscreen().catch(() => {});
});

el.subs.addEventListener('click', () => el.subfile.click());
el.subfile.addEventListener('change', () => loadSubs(el.subfile.files[0]));

el.swap.addEventListener('click', () => {
  wantPause();
  el.player.classList.add('hidden');
  el.setup.classList.remove('hidden');
  el.setupStatus.textContent = '';
});

el.chatForm.addEventListener('submit', (ev) => {
  ev.preventDefault();
  const text = el.chatInput.value.trim();
  if (!text) return;
  send({ t: 'chat', text });
  el.chatInput.value = '';
});

/* Keyboard map follows VLC, including its four jump sizes, so muscle memory
 * carries over. Jumps and play/pause act on the room; volume, aspect and
 * subtitles are local to whoever pressed the key. */
function jumpSize(ev) {
  if (ev.ctrlKey && ev.altKey) return 300;  // VLC "long"
  if (ev.ctrlKey) return 60;                // VLC "medium"
  if (ev.altKey) return 10;                 // VLC "short"
  if (ev.shiftKey) return 3;                // VLC "very short"
  return 5;
}

document.addEventListener('keydown', (ev) => {
  if (ev.target.tagName === 'INPUT' || ev.metaKey) return;

  // Arrows carry the modifiers; everything else must be unmodified so we do
  // not swallow Ctrl+R, Ctrl+T and friends.
  if (ev.key === 'ArrowLeft' || ev.key === 'ArrowRight') {
    jump(ev.key === 'ArrowLeft' ? -jumpSize(ev) : jumpSize(ev));
    ev.preventDefault();
    return;
  }
  if ((ev.key === 'ArrowUp' || ev.key === 'ArrowDown') && !ev.altKey) {
    setVolume(ev.key === 'ArrowUp' ? 0.05 : -0.05);
    ev.preventDefault();
    return;
  }
  if (ev.ctrlKey || ev.altKey) return;

  switch (ev.key) {
    case ' ': case 'k': toggle(); break;
    case 'j': jump(-10); break;
    case 'l': jump(10); break;
    case 'e': osd('frame step'); nudge(1 / 25); break;
    case 'f': el.full.click(); break;
    case 'm': el.mute.click(); break;
    case 'a': cycleAspect(); break;
    case 'v': toggleSubs(); break;
    case '?': toggleHelp(); break;
    case 'Escape': toggleHelp(false); break;
    default: return;
  }
  ev.preventDefault();
});

/* Mouse wheel over the picture changes volume, as VLC does; hold shift to
 * jump instead. Over the seek bar it always jumps. */
el.stage.addEventListener('wheel', (ev) => {
  if (!myFile) return;
  const up = ev.deltaY < 0;
  if (ev.shiftKey || ev.target === el.bar || el.bar.contains(ev.target)) {
    jump(up ? 5 : -5);
  } else {
    setVolume(up ? 0.05 : -0.05);
  }
  ev.preventDefault();
}, { passive: false });

el.video.addEventListener('dblclick', (ev) => {
  ev.preventDefault();
  el.full.click();
});

el.helpbtn.addEventListener('click', () => toggleHelp());
el.help.addEventListener('click', () => toggleHelp(false));

/* The element can also be driven from outside our UI (media keys, the Plasma
 * media controller). Route those through intent so the room follows. */
el.video.addEventListener('pause', () => {
  if (!recentlyTouched() && rolling()) wantPause();
});
el.video.addEventListener('play', () => {
  if (!recentlyTouched() && !rolling()) wantPlay();
});
el.video.addEventListener('ended', () => {
  // Stop the room at the credits rather than letting the clock run on.
  if (rolling()) wantPause();
});
el.video.addEventListener('waiting', () => { if (rolling()) setStall(true); });
el.video.addEventListener('canplay', () => setStall(false));

// ---------------------------------------------------------------- bootstrap

try {
  const vol = localStorage.getItem('vsp.volume');
  if (vol !== null) { el.video.volume = Number(vol); el.volume.value = vol; }
  el.name.value = localStorage.getItem('vsp.name') || '';
  const last = localStorage.getItem('vsp.file');
  if (last) el.lastfile.textContent = 'last time: ' + last;
} catch (_) {}

if (!room.code) {
  el.setupStatus.textContent =
    'No room code in the address. Open the full link your friend sent, ending in #CODE.';
}

document.addEventListener('visibilitychange', () => {
  // Background tabs clamp timers to once a minute, which can stretch a
  // reconnect out absurdly. Retry the moment we are looked at again.
  if (!document.hidden && !wsReady && retry < 99) { retry = 0; connect(); }
});

connect();
setInterval(syncTick, 200);
setInterval(ping, 2000);
setInterval(() => {
  if (myFile) send({ t: 'tick', position: el.video.currentTime, drift: el.video.currentTime - target() });
}, 3000);
