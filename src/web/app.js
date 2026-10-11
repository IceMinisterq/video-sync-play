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
  subfile: $('#subfile'), tracksBtn: $('#tracks-btn'), tracksMenu: $('#tracks-menu'),
  audioList: $('#audio-list'), subList: $('#sub-list'), subLoad: $('#sub-load'),
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
  // Subtitles belong to the film that was playing, not the next one.
  clearSubs();
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
  const lines = text.replace(/\r/g, '').split('\n');
  const out = [];
  for (let i = 0; i < lines.length; i++) {
    // Drop an SRT cue number only when the next line is actually a timing
    // line. Blindly deleting every digit-only line also deletes subtitles
    // whose whole text is a number - a year, a count, a countdown.
    if (/^\s*\d+\s*$/.test(lines[i]) && /-->/.test(lines[i + 1] || '')) continue;
    out.push(lines[i].replace(/(\d{2}:\d{2}:\d{2}),(\d{3})/g, '$1.$2'));
  }
  return 'WEBVTT\n\n' + out.join('\n').trim() + '\n';
}

/** Advanced SubStation: take the dialogue, leave the styling. */
function assToVtt(text) {
  const cues = [];
  let fields = null;
  for (const line of text.replace(/\r/g, '').split('\n')) {
    if (/^Format:/i.test(line) && /Start/i.test(line)) {
      fields = line.slice(line.indexOf(':') + 1).split(',').map((f) => f.trim().toLowerCase());
      continue;
    }
    if (!/^Dialogue:/i.test(line) || !fields) continue;

    // Only the last field (Text) may itself contain commas.
    const rest = line.slice(line.indexOf(':') + 1);
    const parts = rest.split(',');
    const head = parts.slice(0, fields.length - 1).map((p) => p.trim());
    const body = parts.slice(fields.length - 1).join(',');

    const at = (name) => {
      const i = fields.indexOf(name);
      return i === -1 ? null : head[i];
    };
    const start = assTime(at('start'));
    const end = assTime(at('end'));
    if (!start || !end) continue;

    const txt = body
      .replace(/\{[^}]*\}/g, '')     // {\pos(..)} and friends
      .replace(/\\[Nn]/g, '\n')      // hard line breaks
      .replace(/\\h/g, ' ')
      .trim();
    if (txt) cues.push(`${start} --> ${end}\n${txt}`);
  }
  return 'WEBVTT\n\n' + cues.join('\n\n') + '\n';
}

/** ASS writes 0:00:01.23; WebVTT wants 00:00:01.230. */
function assTime(t) {
  const m = /^(\d+):(\d{2}):(\d{2})[.,](\d{1,3})$/.exec((t || '').trim());
  if (!m) return null;
  return `${String(m[1]).padStart(2, '0')}:${m[2]}:${m[3]}.${m[4].padEnd(3, '0')}`;
}

/** Decode subtitle bytes, falling back when the file is not UTF-8.
 *
 * A large share of subtitle files in circulation are Windows-1252 rather
 * than UTF-8, and decoding those as UTF-8 turns every accent into a
 * replacement character. */
function decodeSubs(buffer) {
  try {
    return new TextDecoder('utf-8', { fatal: true }).decode(buffer);
  } catch (_) {
    return new TextDecoder('windows-1252').decode(buffer);
  }
}

/** Add a subtitle file as another selectable track, keeping any already loaded. */
function loadSubs(f) {
  if (!f) return;
  const reader = new FileReader();
  reader.onload = () => {
    const raw = decodeSubs(reader.result);
    const vtt = /\.vtt$/i.test(f.name) ? raw
      : /\.(ass|ssa)$/i.test(f.name) ? assToVtt(raw)
      : srtToVtt(raw);

    // A file we could not make sense of yields a track with no cues, which
    // would sit silently in the menu looking like it works.
    if (!vtt.includes('-->')) {
      note(`${f.name}: no subtitles found in that file`, 'sys');
      osd('could not read those subtitles');
      return;
    }

    const url = URL.createObjectURL(new Blob([vtt], { type: 'text/vtt' }));
    subUrls.push(url);

    const track = document.createElement('track');
    track.kind = 'subtitles';
    // Strip the extension: the bare name reads better in the menu.
    track.label = f.name.replace(/\.[^.]+$/, '');
    track.src = url;
    el.video.appendChild(track);

    // A track added from script starts 'disabled', and a disabled track never
    // fetches its cues - so waiting for its load event would wait forever.
    // Give it a mode straight away: show the first one loaded, and keep the
    // rest parsed but hidden so switching between them is instant.
    const tt = track.track;
    if (tt) tt.mode = currentSubs() === -1 ? 'showing' : 'hidden';

    track.addEventListener('load', renderTracks);
    note(`subtitles: ${f.name}`, 'sys');
    renderTracks();
  };
  reader.readAsArrayBuffer(f);
}

/** Blob URLs for the loaded subtitles, so a file swap can release them. */
let subUrls = [];

function clearSubs() {
  for (const t of [...el.video.querySelectorAll('track')]) t.remove();
  for (const u of subUrls) URL.revokeObjectURL(u);
  subUrls = [];
}

/** Subtitle tracks worth offering: captions and subtitles, not metadata. */
function subTracks() {
  const out = [];
  const list = el.video.textTracks;
  for (let i = 0; i < list.length; i++) {
    if (list[i].kind === 'subtitles' || list[i].kind === 'captions') out.push(i);
  }
  return out;
}

/** -1 turns subtitles off; otherwise index into video.textTracks. */
function showSubs(index) {
  const list = el.video.textTracks;
  for (let i = 0; i < list.length; i++) {
    if (list[i].kind === 'subtitles' || list[i].kind === 'captions') {
      // 'hidden' rather than 'disabled' for the others: the cues stay parsed,
      // so switching back is instant instead of re-fetching.
      list[i].mode = i === index ? 'showing' : 'hidden';
    }
  }
  renderTracks();
}

function currentSubs() {
  const list = el.video.textTracks;
  for (let i = 0; i < list.length; i++) if (list[i].mode === 'showing') return i;
  return -1;
}

/** VLC's `v`: step through off -> each track -> off. */
function cycleSubs() {
  const available = subTracks();
  if (!available.length) {
    osd('no subtitles - load a file from the tracks menu');
    return;
  }
  const at = available.indexOf(currentSubs());
  const next = at + 1 >= available.length ? -1 : available[at + 1];
  showSubs(next);
  osd(next === -1 ? 'subtitles off'
    : `subtitles: ${el.video.textTracks[next].label || 'track ' + (next + 1)}`);
}

/* Audio track switching depends on the browser exposing AudioTrackList.
 * Safari does; Chrome does not implement it at all, and Firefox keeps it
 * behind media.track.enabled. Where it is missing there is no way to reach
 * the other tracks from a web page, so the menu says so rather than
 * pretending. */
/** Whether this browser implements AudioTrackList at all. */
function audioSupported() {
  return !!el.video.audioTracks;
}

function audioTracks() {
  const list = el.video.audioTracks;
  return list && list.length ? list : null;
}

function selectAudio(index) {
  const list = audioTracks();
  if (!list) return;
  for (let i = 0; i < list.length; i++) list[i].enabled = i === index;
  renderTracks();
}

function currentAudio() {
  const list = audioTracks();
  if (!list) return -1;
  for (let i = 0; i < list.length; i++) if (list[i].enabled) return i;
  return -1;
}

/** VLC's `b`. */
function cycleAudio() {
  const list = audioTracks();
  if (!audioSupported()) { osd('this browser cannot switch audio tracks'); return; }
  if (!list) { osd('this file has no audio track'); return; }
  if (list.length < 2) { osd('only one audio track'); return; }
  const next = (currentAudio() + 1) % list.length;
  selectAudio(next);
  osd(`audio: ${trackName(list[next], next)}`);
}

function trackName(t, i) {
  return t.label || t.language || `track ${i + 1}`;
}

/** Rebuild both lists from whatever the media element currently exposes. */
function renderTracks() {
  const row = (label, checked, onPick) => {
    const li = document.createElement('li');
    const b = document.createElement('button');
    b.type = 'button';
    b.textContent = label;
    b.setAttribute('aria-checked', checked ? 'true' : 'false');
    b.addEventListener('click', onPick);
    li.appendChild(b);
    return li;
  };

  el.audioList.textContent = '';
  const audio = audioTracks();
  if (!audio) {
    const p = document.createElement('p');
    p.className = 'menu-empty';
    p.textContent = !audioSupported()
      ? 'This browser cannot list audio tracks. Safari can; in Firefox enable media.track.enabled.'
      : 'No audio track in this file.';
    el.audioList.appendChild(p);
  } else {
    for (let i = 0; i < audio.length; i++) {
      const idx = i;
      el.audioList.appendChild(
        row(trackName(audio[i], i), currentAudio() === i, () => selectAudio(idx)));
    }
  }

  el.subList.textContent = '';
  el.subList.appendChild(row('Off', currentSubs() === -1, () => showSubs(-1)));
  for (const i of subTracks()) {
    const idx = i;
    el.subList.appendChild(
      row(el.video.textTracks[i].label || `Track ${i + 1}`,
          currentSubs() === i, () => showSubs(idx)));
  }
}

function toggleTracksMenu(force) {
  const show = force === undefined ? el.tracksMenu.classList.contains('hidden') : force;
  if (show) renderTracks();
  el.tracksMenu.classList.toggle('hidden', !show);
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
  if (/\.(srt|vtt|ass|ssa)$/i.test(f.name)) loadSubs(f);
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
  // A click with the track menu open means "put that away", not "pause".
  if (!el.tracksMenu.classList.contains('hidden')) {
    toggleTracksMenu(false);
    return;
  }
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

el.tracksBtn.addEventListener('click', (ev) => { ev.stopPropagation(); toggleTracksMenu(); });
el.tracksMenu.addEventListener('click', (ev) => ev.stopPropagation());
el.subLoad.addEventListener('click', () => el.subfile.click());
el.subfile.addEventListener('change', () => {
  for (const f of el.subfile.files) loadSubs(f);
  el.subfile.value = '';
});
document.addEventListener('click', (ev) => {
  if (!el.tracksMenu.contains(ev.target) && ev.target !== el.subfile) toggleTracksMenu(false);
});

/* Tracks can appear after metadata loads, and in-band tracks later still,
 * so keep the menu honest. Each event is bound on the object that actually
 * emits it. */
el.video.addEventListener('loadedmetadata', renderTracks);
for (const list of [el.video.textTracks, el.video.audioTracks]) {
  if (!list || !list.addEventListener) continue;
  for (const ev of ['addtrack', 'removetrack', 'change']) list.addEventListener(ev, renderTracks);
}

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
  // Space and Enter belong to whatever control has focus - a track in the
  // menu, say - rather than to the room.
  if ((ev.key === ' ' || ev.key === 'Enter') && ev.target.closest('button')) return;

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
    case 'v': cycleSubs(); break;
    case 'b': cycleAudio(); break;
    case '?': toggleHelp(); break;
    case 'Escape': toggleHelp(false); toggleTracksMenu(false); break;
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
