# video sync play

Watch a video file in step with someone else. You both already have the file;
this keeps the two playheads locked together.

**The rule:** the film runs only while *nobody* is paused. If either of you
hits pause, both stop. It starts again when you are both ready.

The video never leaves either machine — only play, pause and seek cross the
wire. There is no account, no sign-up, and no server to rent.

## Requirements

**None.** One file, nothing to install.

- **Linux** (Fedora, Arch, CachyOS, Debian, anything else), x86-64 or ARM64.
  X11 or Wayland, KDE or otherwise — it makes no difference, the player is a
  web page.
- **macOS**, Apple Silicon or Intel.
- **Windows** 10 or 11.
- A browser you already have (Firefox, Chrome, Safari, Edge…).

That is the whole list. No Rust, no Python, no Node, no VLC, no libraries, no
admin rights. The Linux builds are statically linked, so there is no glibc
version to match and nothing to break when your system updates.

The two of you do not need the same operating system. Linux and Windows,
macOS and Linux — any mix works.

## Use it

1. **Download** the file for your system from
   [Releases](../../releases/latest):

   | your system | file |
   |---|---|
   | Linux, normal PC | `video-sync-play-linux-x86_64` |
   | Linux, ARM (Raspberry Pi, Asahi) | `video-sync-play-linux-aarch64` |
   | macOS | `video-sync-play-macos` |
   | Windows | `video-sync-play-windows.exe` |

2. **One of you starts it.** Your browser opens by itself.

   On Linux and macOS, allow it to run first:

   ```bash
   chmod +x video-sync-play-*
   ./video-sync-play-*
   ```

   On Windows, double-click the `.exe`.

3. **Send the printed link to the other person.** You both pick your own copy
   of the video, and press play.

That link ends in `#CODE`, and that code is the password — anyone without it
is refused. Treat the link as the invitation it is.

Only one of you runs the program. The other just opens the link, and needs no
download at all.

### First run on macOS and Windows

The binaries are not code-signed — signing needs a paid developer account —
so each system asks once:

- **macOS** says the developer cannot be verified. Either right-click the
  file and choose *Open*, or clear the quarantine flag:

  ```bash
  xattr -d com.apple.quarantine video-sync-play-macos
  ```

- **Windows** SmartScreen shows a blue box: *More info* → *Run anyway*.
  Windows Firewall will then ask whether to allow network access — say yes
  for **private networks**, otherwise your friend cannot reach you.

If that bothers you, build it yourself from source — it takes one command and
is described below.

## Getting the link to reach them

**Same wifi** — nothing to do. The printed link already works.

**Different places** — two options, and the program tells you about both.

### The easy one: `--public`

```bash
./video-sync-play --public
```

This asks your router to open the port for you, using UPnP or NAT-PMP, and
then prints a link that works from anywhere. There is nothing to install and
nothing to configure on the router — the program speaks those protocols
itself. The forward is closed again when you press Ctrl-C, and it carries a
one-hour lease so it expires on its own even if the program is killed.

Be aware of what it does, because it is a real hole while it is open:

- The port is reachable **from the whole internet** for as long as the
  program runs. The room code keeps people out — it is 12 characters when
  `--public` is on, and wrong guesses are rate-limited — but the port is
  visible to anyone scanning.
- The connection is **plain HTTP**. Someone between you can see what you are
  watching and read the chat. They cannot join without the code.
- Many routers have UPnP switched off, so this simply may not work. The
  program says so and carries on.
- Behind carrier-grade NAT (common on mobile and some fibre providers) the
  forward succeeds but is still unreachable. The program detects that and
  tells you, rather than printing a link that cannot work.

Do not use it on a network you do not control — a hotel, a dorm, an office.
The "internet" it opens you to there includes everyone else on that network.

### The private one: Tailscale

Encrypted end to end, and nothing is exposed to the internet at all. It needs
installing on both machines, which is the only reason it is not first.

| | |
|---|---|
| Fedora | `sudo dnf install tailscale && sudo systemctl enable --now tailscaled && sudo tailscale up` |
| Arch / CachyOS | `sudo pacman -S tailscale && sudo systemctl enable --now tailscaled && sudo tailscale up` |
| macOS | `brew install --cask tailscale`, or the App Store |
| Windows | `winget install tailscale.tailscale` |

Same account on both, then start `video-sync-play` again — it prints a
Tailscale link and prefers it over everything else.

If you are deciding: use Tailscale for anything you would rather keep
private, and `--public` when you just want it to work in one command.

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
| `v` | next subtitle track |
| `b` | next audio track |

Play, pause and jumps move the room. **Volume, subtitles, audio track and
aspect ratio are yours alone** — you each pick your own, and changing them
sends nothing to the other person. One of you can watch with French subtitles
while the other has none.

### Audio tracks and subtitles

The button to the right of the volume slider opens the track menu.

**Subtitles** — drag a subtitle file onto the window, or load one from the
menu. `.srt`, `.vtt`, `.ass` and `.ssa` all work; SubRip and SubStation are
converted to WebVTT for you, styling tags and all. Load as many as you like
and switch between them with `v` — they stay parsed, so switching is
instant. Files that are not UTF-8 (a lot of older `.srt`s are Windows-1252)
are detected and decoded properly rather than coming out full of question
marks.

**Audio tracks** — the menu lists every track the browser exposes, and `b`
steps through them. Be warned: *most browsers do not expose them at all*.
Safari does; Firefox does with `media.track.enabled` turned on in
`about:config`; Chrome and Chromium do not implement the API, so a file with
English and French audio will only ever play the first. The menu says which
case you are in rather than pretending.

If you are stuck on a Chromium browser and need the other language, pick the
track when converting instead — `-map 0:v -map 0:a:1` selects the second
audio stream.

## Options

You should not need any of these.

```
--public           ask the router to open the port, so the link works
                   over the internet without Tailscale
--port 8787        port to listen on
--bind 0.0.0.0     interface to bind
--code WORD        fixed room code instead of a fresh random one
                   (at least 12 characters when used with --public)
--no-open          do not open the browser on startup
```

`VSP_TRACE=1` logs every room state change to stderr. `VSP_GATEWAY=<ip>`
overrides the detected router address, which is occasionally useful on an
unusual network and is what the port-mapping tests point at a fake router.

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

For the dependency-free Linux binary that runs on any distribution:

```bash
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
```

Linux ARM64 cross-builds from an x86 machine with no C toolchain — the
repository's `.cargo/config.toml` points that target at the linker Rust
already ships:

```bash
rustup target add aarch64-unknown-linux-musl
cargo build --release --target aarch64-unknown-linux-musl
```

macOS and Windows binaries are built on their own machines by
`.github/workflows/release.yml`, which also runs the behaviour tests on each
operating system before publishing.

The web UI is compiled into the binary, so the result is a single file.

To run the behaviour tests (stdlib Python, nothing to install) — they start
the binary themselves, drive it with real WebSocket peers and check the sync
rules:

```bash
cargo test --release                                      # protocol encoding and parsing
python3 tests/behaviour.py target/release/video-sync-play # sync rules, end to end
python3 tests/portmap.py  target/release/video-sync-play  # port mapping, against a fake router
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

## Licence

MIT — do what you like with it. See [LICENSE](LICENSE).
