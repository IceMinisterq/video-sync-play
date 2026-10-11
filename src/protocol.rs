//! Wire format between the browser and the sync server.
//!
//! Everything is JSON with a short tag in `t`. Positions are media seconds
//! (f64); timestamps are unix epoch milliseconds (f64) so the browser can
//! compare them against its own clock once it knows the offset.

use serde::{Deserialize, Serialize};

/// What the browser knows about the file the user picked. Used only to warn
/// when two people are clearly watching different files.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct FileInfo {
    pub name: String,
    pub size: f64,
    pub duration: f64,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
// Some fields are carried for diagnostics and future use rather than consumed
// by the current server logic; they are part of the wire format regardless.
#[allow(dead_code)]
pub enum ClientMsg {
    /// First message on every connection. Carries the room code.
    Hello {
        code: String,
        name: String,
        #[serde(default)]
        file: Option<FileInfo>,
    },
    /// The user picked (or swapped) a file.
    File { file: FileInfo },
    /// This peer wants everyone stopped, at `position`.
    Pause { position: f64 },
    /// This peer is ready to roll again.
    Resume { position: f64 },
    /// Move the whole room to `position`.
    Seek { position: f64 },
    /// Buffering / decoding behind. Treated as a soft pause.
    Stall { stalled: bool },
    /// Periodic keepalive carrying this peer's real playhead.
    Tick { position: f64, drift: f64 },
    /// Clock synchronisation. `c` is echoed back untouched.
    Ping { c: f64 },
    Chat { text: String },
}

#[derive(Debug, Serialize)]
pub struct PeerView {
    pub id: u64,
    pub name: String,
    pub paused: bool,
    pub stalled: bool,
    /// Has gone quiet; presumed disconnected until it speaks again.
    pub silent: bool,
    pub drift: f64,
    pub file: Option<FileInfo>,
    /// True when this peer's file looks different from the majority's.
    pub mismatch: bool,
}

#[derive(Debug, Serialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ServerMsg {
    Welcome { id: u64, now: f64 },
    /// Full room state. Small enough to just resend on every change.
    State {
        now: f64,
        playing: bool,
        /// Media position that `anchor` refers to.
        position: f64,
        /// Epoch ms at which playback starts from `position`. May be slightly
        /// in the future: that is the shared countdown that makes everyone
        /// un-pause on the same wall-clock instant.
        anchor: f64,
        peers: Vec<PeerView>,
    },
    Pong { c: f64, s: f64 },
    Chat { from: String, text: String },
    Error { message: String },
}
