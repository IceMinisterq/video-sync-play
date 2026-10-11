# video sync play

Watch a video file in step with someone else. You both already have the file;
this keeps the two playheads locked together.

**The rule:** the film runs only while *nobody* is paused. If either of you
hits pause, both stop. It starts again when you are both ready.

The video never leaves either machine — only play, pause and seek cross the
wire. There is no account, no sign-up, and no server to rent.

## Requirements

**None.** One file, nothing to install.

- Any 64-bit Linux — Fedora 44, Arch, CachyOS, anything else. X11 or Wayland,
  KDE or otherwise; it makes no difference, the player is a web page.
- A browser you already have (Firefox, Chromium, Brave…).
- That is the whole list. No Rust, no Python, no Node, no VLC, no libraries,
  no root. The binary is statically linked, so there is no glibc version to
  match and nothing to break when your system updates.

## Use it

1. **Download** `video-sync-play` from
   [Releases](../../releases/latest), and allow it to run:

   ```bash
   chmod +x video-sync-play
   ```

2. **One of you starts it.** Your browser opens automatically.

   ```bash
   ./video-sync-play
   ```

3. **Send the printed link to the other person.** You both pick your own copy
   of the video, and press play.

That link ends in `#CODE`, and that code is the password — anyone without it
is refused. Treat the link as the invitation it is.

Only one of you runs the program. The other just opens the link.

## Getting the link to reach them

**Same wifi** — nothing to do. The printed link already works.

**Different places** — install Tailscale on both machines. It puts the two of
you on a private, encrypted network with nothing in the middle, free, no
server to rent:

```bash
sudo dnf install tailscale && sudo systemctl enable --now tailscaled && sudo tailscale up
```

```bash
sudo pacman -S tailscale && sudo systemctl enable --now tailscaled && sudo tailscale up
```

Same account on both, then start `video-sync-play` again — it will print a
link that works from anywhere.

You *can* forward a port on your router instead, but that connection is plain
HTTP: anyone in between can read your chat and see what you are watching.
Tailscale encrypts it and needs no router changes.

## Controls

VLC's keys, so your muscle memory carries over. Press `?` in the page for
this list.

| | |
|---|---|
| `space` / `k` | play · pause (for both of you) |
| `←` `→` | jump 5s |
| `shift` + `←` `→` | jump 3s |
| `alt` + `←` `→` | jump 10s |
| `ctrl` + `←` `→` | jump 1 min |
| `ctrl`+`alt` + `←` `→` | jump 5 min |
| `j` / `l` | jump 10s |
| `e` | step one frame |
| `↑` `↓`, mouse wheel | volume |
| `shift` + wheel | jump |
| `m` | mute |
| `f`, double-click | fullscreen |
| `a` | aspect ratio |
| `v` | subtitles on / off |

Play, pause and jumps move the room. Volume, subtitles and aspect ratio are
yours alone — change them freely.

Drag an `.srt` or `.vtt` file onto the window for subtitles. You each load
your own.

## Options

You should not need any of these.

```
--port 8787        port to listen on
--bind 0.0.0.0     interface to bind
--code WORD        fixed room code instead of a fresh random one
--no-open          do not open the browser on startup
```

`VSP_TRACE=1` logs every room state change to stderr.

## If the video will not play

Your browser has to decode it. H.264 + AAC in an `.mp4` always works;
HEVC/H.265, AC-3 audio and some MKVs do not. Quick test: drag the file into
an empty browser tab. If it plays there, it plays here.

To convert, have **one** of you do it and send the result, so you are both on
an identical file:

```bash
ffmpeg -i original.mkv -c:v libx264 -crf 20 -preset fast -c:a aac -b:a 192k -movflags +faststart shared.mp4
```

The sidebar compares file size and duration and warns you if your two files
differ — the clocks would still match, but you would not be looking at the
same frame.

## Build from source

Only if you want to; the release binary needs nothing. Requires Rust 1.75+.

```bash
cargo build --release
```

For the dependency-free binary that runs on any Linux:

```bash
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
```

The web UI is compiled into the binary, so the result is a single file.

To run the behaviour tests (stdlib Python, nothing to install) — they start
the binary themselves, drive it with real WebSocket peers and check the sync
rules:

```bash
python3 tests/behaviour.py target/release/video-sync-play
```

## How it stays in sync

The server does not send "play now" commands. It publishes a timeline —
*position P applies at time A, and the clock is running* — and each browser
derives its own target playhead from it, after measuring its clock offset
against the server the way NTP does.

Corrections are proportional: under 0.1s of drift is ignored, up to 0.6s is
absorbed by bending playback speed a few percent (inaudible), and anything
larger is a seek. Un-pausing is scheduled slightly in the future so both ends
start on the same wall-clock instant rather than whenever their packet lands.

A peer that is buffering says so, and the room waits. A peer that disconnects
stops the room — on a flaky link a reconnect can take tens of seconds, and
drifting that far apart is the one thing this exists to prevent. Playback
resumes when you both ask for it.
