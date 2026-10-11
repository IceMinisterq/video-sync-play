//! The authoritative room state.
//!
//! Playback is modelled as a *timeline anchor* rather than a stream of
//! play/pause commands: the room says "position P applies at epoch time A, and
//! the clock is running". Every browser derives its own target playhead from
//! that, which means a late or briefly disconnected peer converges on its own
//! without any special catch-up handshake.
//!
//! The gate rule: the clock runs only while **nobody** is paused or stalled.

use std::collections::HashMap;
use std::sync::Mutex;

use tokio::sync::broadcast;

use crate::protocol::{FileInfo, PeerView, ServerMsg};

/// Head start given when the room un-pauses, so both ends have time to receive
/// the state and prime the decoder before the clock actually moves.
const RESUME_GRACE_MS: f64 = 300.0;
/// Same idea after a seek, where the decoder has more work to do.
const SEEK_GRACE_MS: f64 = 500.0;
/// A peer that has not said anything for this long is presumed gone. The
/// client pings every 2s, so this is three missed beats. Needed because a
/// closed laptop lid or a yanked cable does not close the TCP socket for
/// minutes, and until it does the room would happily play on without them.
const PEER_TIMEOUT_MS: f64 = 6_000.0;
/// After this long with no word at all, treat the peer as gone and drop it.
/// Without this a laptop that is closed mid-film - which never closes its
/// TCP connection - would hold the room paused forever, and the person still
/// watching could never start it again.
const PEER_GONE_MS: f64 = 45_000.0;
/// How far a pausing peer's reported position may differ from the room's own
/// estimate before we distrust it and freeze at the room's position instead.
const PAUSE_TRUST_S: f64 = 2.0;

pub fn now_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64() * 1000.0)
        .unwrap_or(0.0)
}

#[derive(Clone)]
struct Peer {
    name: String,
    paused: bool,
    stalled: bool,
    /// Set by the watchdog when the peer stops talking to us.
    silent: bool,
    last_seen: f64,
    drift: f64,
    file: Option<FileInfo>,
}

struct State {
    next_id: u64,
    peers: HashMap<u64, Peer>,
    position: f64,
    anchor: f64,
    playing: bool,
}

impl State {
    /// Shortest duration anybody reported. The timeline must not run past the
    /// end of the film: the <video> elements stop there, so a room clock that
    /// kept counting would invent a position nobody can ever be at.
    fn limit(&self) -> Option<f64> {
        let mut shortest: Option<f64> = None;
        for p in self.peers.values() {
            if let Some(f) = &p.file {
                if f.duration > 0.0 {
                    shortest = Some(shortest.map_or(f.duration, |d: f64| d.min(f.duration)));
                }
            }
        }
        shortest
    }

    /// Where the room's playhead is at `now`.
    fn pos_at(&self, now: f64) -> f64 {
        let raw = if self.playing {
            // Clamp the elapsed term: before `anchor` we are in the shared
            // countdown and the playhead must stay put.
            self.position + (now - self.anchor).max(0.0) / 1000.0
        } else {
            self.position
        };
        match self.limit() {
            Some(end) => raw.min(end),
            None => raw,
        }
    }

    fn blocked(&self) -> bool {
        self.peers.is_empty() || self.peers.values().any(|p| p.paused || p.stalled || p.silent)
    }

    /// Re-derive `playing` from the gate, freezing or re-anchoring as needed.
    fn regate(&mut self, now: f64, grace: f64) {
        if self.blocked() {
            if self.playing {
                self.position = self.pos_at(now);
                self.anchor = now;
                self.playing = false;
            }
        } else if !self.playing {
            self.playing = true;
            self.anchor = now + grace;
        }
    }
}

pub struct Room {
    state: Mutex<State>,
    tx: broadcast::Sender<String>,
}

impl Room {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(256);
        Room {
            state: Mutex::new(State {
                next_id: 1,
                peers: HashMap::new(),
                position: 0.0,
                anchor: now_ms(),
                playing: false,
            }),
            tx,
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<String> {
        self.tx.subscribe()
    }

    /// New peers always arrive paused: joining mid-film should not drag anyone
    /// along before the newcomer has even picked their file.
    pub fn join(&self, name: String, file: Option<FileInfo>) -> u64 {
        let mut st = self.state.lock().unwrap();
        let id = st.next_id;
        st.next_id += 1;
        st.peers.insert(
            id,
            Peer {
                name,
                paused: true,
                stalled: false,
                silent: false,
                last_seen: now_ms(),
                drift: 0.0,
                file,
            },
        );
        let now = now_ms();
        st.regate(now, RESUME_GRACE_MS);
        drop(st);
        self.publish();
        id
    }

    /// Somebody's connection went away. Always stop the room: letting whoever
    /// is left race ahead is the one failure this app exists to prevent, and a
    /// reconnect can easily take tens of seconds on a flaky link. Everyone
    /// still here is marked paused, so playback only restarts when a human
    /// asks for it again.
    pub fn leave(&self, id: u64) {
        let mut st = self.state.lock().unwrap();
        if st.peers.remove(&id).is_none() {
            return;
        }
        let now = now_ms();
        if st.playing {
            st.position = st.pos_at(now);
            st.anchor = now;
            st.playing = false;
        }
        for p in st.peers.values_mut() {
            p.paused = true;
        }
        drop(st);
        self.publish();
    }

    /// Note that a peer is alive. Called for every inbound message.
    pub fn touch(&self, id: u64) {
        let mut st = self.state.lock().unwrap();
        let now = now_ms();
        let revived = match st.peers.get_mut(&id) {
            Some(p) => {
                p.last_seen = now;
                std::mem::replace(&mut p.silent, false)
            }
            None => false,
        };
        if revived {
            st.regate(now, RESUME_GRACE_MS);
            drop(st);
            self.publish();
        }
    }

    /// Watchdog: gate the room on anyone who has gone quiet.
    pub fn reap(&self) {
        let mut st = self.state.lock().unwrap();
        let now = now_ms();
        let mut changed = false;

        // Long gone: forget them entirely, exactly as if the socket had
        // closed, so whoever is left can carry on watching.
        let gone: Vec<u64> = st
            .peers
            .iter()
            .filter(|(_, p)| now - p.last_seen > PEER_GONE_MS)
            .map(|(id, _)| *id)
            .collect();
        for id in &gone {
            st.peers.remove(id);
            changed = true;
        }
        if !gone.is_empty() && st.playing {
            st.position = st.pos_at(now);
            st.anchor = now;
            st.playing = false;
        }
        if !gone.is_empty() {
            for p in st.peers.values_mut() {
                p.paused = true;
            }
        }

        for p in st.peers.values_mut() {
            let silent = now - p.last_seen > PEER_TIMEOUT_MS;
            if silent != p.silent {
                p.silent = silent;
                changed = true;
            }
        }
        if !changed {
            return;
        }
        st.regate(now, RESUME_GRACE_MS);
        drop(st);
        self.publish();
    }

    pub fn set_file(&self, id: u64, file: FileInfo) {
        let mut st = self.state.lock().unwrap();
        if let Some(p) = st.peers.get_mut(&id) {
            p.file = Some(file);
        }
        drop(st);
        self.publish();
    }

    pub fn pause(&self, id: u64, reported: f64) {
        let mut st = self.state.lock().unwrap();
        let now = now_ms();
        let room_pos = st.pos_at(now);
        if let Some(p) = st.peers.get_mut(&id) {
            p.paused = true;
        }
        // Freeze where the person who hit pause actually was, unless their
        // playhead is wildly off — then trust the room.
        let pos = if (reported - room_pos).abs() <= PAUSE_TRUST_S { reported } else { room_pos };
        st.position = pos.max(0.0);
        st.anchor = now;
        st.playing = false;
        drop(st);
        self.publish();
    }

    pub fn resume(&self, id: u64) {
        let mut st = self.state.lock().unwrap();
        if let Some(p) = st.peers.get_mut(&id) {
            p.paused = false;
            // An explicit play is a statement of readiness; let the stall
            // detector re-raise it if the decoder really is starving.
            p.stalled = false;
        }
        let now = now_ms();
        st.regate(now, RESUME_GRACE_MS);
        drop(st);
        self.publish();
    }

    pub fn seek(&self, position: f64) {
        let mut st = self.state.lock().unwrap();
        let now = now_ms();
        let end = st.limit().unwrap_or(f64::MAX);
        st.position = position.clamp(0.0, end);
        // Everyone is about to re-buffer, so drop stale stall flags and give
        // the room a moment before the clock moves again.
        for p in st.peers.values_mut() {
            p.stalled = false;
        }
        st.anchor = if st.playing { now + SEEK_GRACE_MS } else { now };
        st.regate(now, SEEK_GRACE_MS);
        drop(st);
        self.publish();
    }

    pub fn stall(&self, id: u64, stalled: bool) {
        let mut st = self.state.lock().unwrap();
        let changed = match st.peers.get_mut(&id) {
            Some(p) if p.stalled != stalled => {
                p.stalled = stalled;
                true
            }
            _ => false,
        };
        if !changed {
            return;
        }
        let now = now_ms();
        st.regate(now, RESUME_GRACE_MS);
        drop(st);
        self.publish();
    }

    /// Keepalive. Records the peer's measured drift for the UI but never moves
    /// the timeline — one lagging client must not drag the room with it.
    pub fn tick(&self, id: u64, drift: f64) {
        let mut st = self.state.lock().unwrap();
        if let Some(p) = st.peers.get_mut(&id) {
            p.drift = drift;
        }
    }

    pub fn name_of(&self, id: u64) -> String {
        self.state
            .lock()
            .unwrap()
            .peers
            .get(&id)
            .map(|p| p.name.clone())
            .unwrap_or_else(|| "someone".into())
    }

    pub fn send(&self, msg: &ServerMsg) {
        if let Ok(s) = serde_json::to_string(msg) {
            let _ = self.tx.send(s);
        }
    }

    /// Broadcast the whole room state. Cheap at this scale and removes any
    /// chance of two peers holding divergent partial views.
    pub fn publish(&self) {
        let st = self.state.lock().unwrap();
        let now = now_ms();

        // A file "mismatch" is judged against the most common (size, duration)
        // pair in the room, so the odd one out gets flagged, not everyone.
        let mut tally: HashMap<(u64, u64), usize> = HashMap::new();
        for p in st.peers.values() {
            if let Some(f) = &p.file {
                *tally.entry(fingerprint(f)).or_insert(0) += 1;
            }
        }
        let majority = tally.iter().max_by_key(|(_, n)| **n).map(|(k, _)| *k);

        let peers = st
            .peers
            .iter()
            .map(|(id, p)| PeerView {
                id: *id,
                name: p.name.clone(),
                paused: p.paused,
                stalled: p.stalled,
                silent: p.silent,
                drift: p.drift,
                file: p.file.clone(),
                mismatch: match (&p.file, majority) {
                    (Some(f), Some(m)) => fingerprint(f) != m,
                    _ => false,
                },
            })
            .collect();

        if std::env::var_os("VSP_TRACE").is_some() {
            let who: Vec<String> = st.peers.values()
                .map(|p| format!("{}[{}{}{}]", p.name,
                    if p.paused {"P"} else {"-"},
                    if p.stalled {"S"} else {"-"},
                    if p.silent {"Q"} else {"-"}))
                .collect();
            eprintln!("[trace] playing={} pos={:.2} anchor_in={:.0}ms peers={}",
                st.playing, st.position, st.anchor - now, who.join(","));
        }

        let msg = ServerMsg::State {
            now,
            playing: st.playing,
            position: st.position,
            anchor: st.anchor,
            peers,
        };
        drop(st);
        self.send(&msg);
    }
}

/// Size plus duration rounded to the second: enough to tell "same rip" from
/// "different rip" without reading a byte of the file.
fn fingerprint(f: &FileInfo) -> (u64, u64) {
    (f.size as u64, f.duration.round() as u64)
}
