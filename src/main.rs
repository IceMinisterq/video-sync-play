//! video sync play — watch a local video file in step with someone else.
//!
//! One person runs the binary and shares the printed link. Both open it, both
//! drop in their own copy of the file, and the room keeps the two playheads
//! locked together. The video data itself never leaves either machine: only
//! play/pause/seek state crosses the wire.

mod portmap;
mod protocol;
mod room;

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        ConnectInfo, State,
    },
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{broadcast, mpsc};

use portmap::Mapping;
use protocol::{ClientMsg, ServerMsg};
use room::{now_ms, Room};

#[derive(Clone)]
struct App {
    room: Arc<Room>,
    code: String,
    guard: Arc<Guard>,
}

/// Rate limiter for *wrong* room codes.
///
/// Only strictly needed once `--public` puts the port on the open internet,
/// but it costs a legitimate user nothing, so it is always on.
///
/// It deliberately throttles failures rather than attempts. A correct code is
/// proof of authorisation and is always admitted, so no amount of guessing
/// from elsewhere can lock out the person actually holding the link. The
/// global bucket is the load-bearing part: it caps everyone together, so a
/// botnet gets no more guesses than a single host would.
struct Guard {
    inner: Mutex<GuardState>,
}

struct GuardState {
    tokens: f64,
    refilled: Instant,
    /// Per-source failure counts, so one noisy address cannot spend the whole
    /// global budget on its own.
    offenders: HashMap<IpAddr, (u32, Instant)>,
}

/// Thirty wrong guesses to start, then ten a minute. Against a 60-bit code
/// that is in the order of one chance in 10^14 over a long evening.
const GUESS_BURST: f64 = 30.0;
const GUESS_PER_SEC: f64 = 10.0 / 60.0;
const PER_IP_LIMIT: u32 = 5;
const PER_IP_WINDOW: Duration = Duration::from_secs(60);
const PER_IP_BAN: Duration = Duration::from_secs(900);
/// Cap on how many sources we remember, so the map cannot be grown forever.
const MAX_TRACKED_SOURCES: usize = 4096;
/// Long enough to make guessing hopeless, short enough that a mistyped link
/// still feels like it simply did not work.
const WRONG_CODE_DELAY: Duration = Duration::from_secs(1);

impl Guard {
    fn new() -> Guard {
        Guard {
            inner: Mutex::new(GuardState {
                tokens: GUESS_BURST,
                refilled: Instant::now(),
                offenders: HashMap::new(),
            }),
        }
    }

    /// Record a wrong code. Returns whether to answer at all: once a source
    /// is over budget we simply drop it, denying a guesser both speed and the
    /// feedback that would tell them the service is still listening.
    fn wrong_code(&self, who: IpAddr) -> bool {
        let mut st = self.inner.lock().unwrap();
        let now = Instant::now();

        let elapsed = now.duration_since(st.refilled).as_secs_f64();
        st.tokens = (st.tokens + elapsed * GUESS_PER_SEC).min(GUESS_BURST);
        st.refilled = now;

        // Expired entries would otherwise accumulate one per source address
        // for as long as the program runs, which on a public port is a slow
        // memory leak anybody can drive.
        st.offenders.retain(|_, (_, seen)| now.duration_since(*seen) < PER_IP_BAN);
        if st.offenders.len() >= MAX_TRACKED_SOURCES && !st.offenders.contains_key(&who) {
            // Past this point the global bucket alone does the limiting; it
            // is the part that actually bounds the guess rate anyway.
            return st.tokens >= 1.0 && {
                st.tokens -= 1.0;
                true
            };
        }

        let entry = st.offenders.entry(who).or_insert((0, now));
        let banned = entry.0 >= PER_IP_LIMIT;
        let window = if banned { PER_IP_BAN } else { PER_IP_WINDOW };
        if now.duration_since(entry.1) >= window {
            *entry = (0, now);
        }
        entry.0 += 1;
        let count = entry.0;
        let over_per_ip = count > PER_IP_LIMIT;

        // Worth printing: on a public port this is how you notice a scan.
        eprintln!("video-sync-play: wrong room code from {who} ({count} recently)");

        if over_per_ip || st.tokens < 1.0 {
            return false;
        }
        st.tokens -= 1.0;
        true
    }
}

struct Args {
    port: u16,
    bind: IpAddr,
    code: Option<String>,
    open: bool,
    public: bool,
}

fn parse_args() -> Args {
    let mut a =
        Args { port: 8787, bind: "0.0.0.0".parse().unwrap(), code: None, open: true, public: false };
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < argv.len() {
        let next = |i: usize| argv.get(i + 1).cloned().unwrap_or_default();
        match argv[i].as_str() {
            "--port" | "-p" => {
                a.port = next(i).parse().unwrap_or_else(|_| die("--port needs a number"));
                i += 1;
            }
            "--bind" | "-b" => {
                a.bind = next(i).parse().unwrap_or_else(|_| die("--bind needs an IP address"));
                i += 1;
            }
            "--code" | "-c" => {
                let v = next(i);
                // Without this, `--code --no-open` silently produces an empty
                // code, and then every passer-by is already authorised.
                if v.is_empty() || v.starts_with("--") {
                    die("--code needs a value");
                }
                a.code = Some(v.to_uppercase());
                i += 1;
            }
            "--no-open" => a.open = false,
            "--public" => a.public = true,
            "--help" | "-h" => {
                println!(
                    "video sync play — watch a video in step with a friend\n\n\
                     Usage: video-sync-play [options]\n\n\
                       --public          ask the router to open the port, so the link\n\
                                         works over the internet with no Tailscale\n\
                       --port 8787       port to listen on\n\
                       --bind 0.0.0.0    interface to bind\n\
                       --code WORD       fixed room code instead of a random one\n\
                       --no-open         do not open the browser on startup\n\n\
                     Run it, open the printed link, send the same link to your friend.\n\
                     Both pick your own copy of the file. Nobody uploads anything."
                );
                std::process::exit(0);
            }
            other => die(&format!("unknown argument: {other}")),
        }
        i += 1;
    }
    a
}

/// Wait for any of the ways this program is asked to stop.
///
/// Worth covering all of them: each one needs to reach the code that hands
/// the router's port back, and a forward left behind is a hole the user did
/// not agree to keep.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            // `kill`, systemd, a logout.
            _ = term.recv() => {}
        }
    }

    #[cfg(windows)]
    {
        use tokio::signal::windows;
        let (mut brk, mut close) = match (windows::ctrl_break(), windows::ctrl_close()) {
            (Ok(b), Ok(c)) => (b, c),
            _ => {
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = brk.recv() => {}
            // Closing the console window gives us only a few seconds, which
            // is why the release path is kept short.
            _ = close.recv() => {}
        }
    }
}

fn die(msg: &str) -> ! {
    eprintln!("video-sync-play: {msg}");
    std::process::exit(2);
}

#[tokio::main]
async fn main() {
    let args = parse_args();

    if args.public && args.bind.is_loopback() {
        die("--public cannot work with a loopback --bind: nothing outside could reach it");
    }

    // On a public port the code is the only thing between the room and the
    // whole internet, so it gets longer. It lives in a link nobody types, so
    // the extra length costs the user nothing.
    let code = match args.code {
        Some(c) => {
            if c.len() < 4 {
                die("--code must be at least 4 characters");
            }
            if args.public && c.len() < 12 {
                die("--code must be at least 12 characters when used with --public");
            }
            c
        }
        None => gen_code(if args.public { 12 } else { 8 }),
    };

    let app = App { room: Arc::new(Room::new()), code: code.clone(), guard: Arc::new(Guard::new()) };

    // Watchdog for peers that stop responding without closing the socket.
    let watchdog = app.room.clone();
    tokio::spawn(async move {
        let mut beat = tokio::time::interval(Duration::from_millis(1000));
        loop {
            beat.tick().await;
            watchdog.reap();
        }
    });

    let router = Router::new()
        .route("/", get(|| async { asset("index.html") }))
        .route("/style.css", get(|| async { asset("style.css") }))
        .route("/app.js", get(|| async { asset("app.js") }))
        .route("/ws", get(ws_upgrade))
        .with_state(app);

    let addr = SocketAddr::new(args.bind, args.port);
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => die(&format!("cannot bind {addr}: {e}")),
    };
    // --port 0 asks the OS to choose; the banner must show what it chose.
    let port = listener.local_addr().map(|a| a.port()).unwrap_or(args.port);

    let mapping = if args.public {
        println!("\n  asking your router to open port {port}...");
        match portmap::map(port) {
            Ok(m) => Some(m),
            Err(why) => {
                println!("  could not: {why}");
                println!("  carrying on without it - the wifi link below still works.");
                None
            }
        }
    } else {
        None
    };

    // Re-assert the forward before the lease lapses. Routers routinely grant
    // less than asked, so renew against what we were actually given.
    if let Some(m) = mapping.clone() {
        if m.lease > 0 {
            tokio::spawn(async move {
                let half = Duration::from_secs((m.lease / 2).max(30) as u64);
                loop {
                    tokio::time::sleep(half).await;
                    if !m.renew() {
                        eprintln!("video-sync-play: the router dropped our port forward");
                        return;
                    }
                }
            });
        }
    }

    banner(port, &code, args.open, mapping.as_ref());
    if args.open {
        open_browser(port, &code);
    }

    let on_exit = mapping.clone();
    let served = axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            // Give the port back. A forward left behind on someone's router
            // is a hole they did not agree to keep. Blocking briefly here is
            // fine: we are on our way out.
            if let Some(m) = on_exit {
                println!("\nclosing the port on your router...");
                m.release();
            }
            println!("bye.");
        })
        .await;

    if let Err(e) = served {
        die(&format!("server stopped: {e}"));
    }
}

fn banner(port: u16, code: &str, open: bool, mapping: Option<&Mapping>) {
    // Lead with the one link worth sending: whichever reaches the other
    // person from furthest away. Tailscale outranks a public forward because
    // it is encrypted; the forward outranks the LAN because it reaches
    // further.
    let tail = format!("/#{code}");
    let ts = tailscale_ip();
    // A full-tunnel VPN silently breaks a port forward: the connection comes
    // in on the ethernet card and the reply goes out the tunnel.
    let blocked_by_vpn = mapping
        .and_then(|m| m.internal_ip())
        .map(|ip| portmap::misrouted(ip, portmap::internet_facing_addr()))
        .unwrap_or(false);

    let (share, how) = match (&ts, mapping, lan_ip()) {
        (Some(ts), _, _) => (format!("http://{ts}:{port}{tail}"), "works from anywhere, encrypted"),
        (None, Some(m), _) if blocked_by_vpn => (
            format!("http://{}{tail}", m.external),
            "your VPN stops this working - see below",
        ),
        (None, Some(m), _) => (
            format!("http://{}{tail}", m.external),
            "works from anywhere - but not encrypted, see below",
        ),
        (None, None, Some(lan)) => (format!("http://{lan}:{port}{tail}"), "works on this wifi"),
        (None, None, None) => (format!("http://localhost:{port}{tail}"), "this machine only"),
    };

    println!("\n  video sync play\n");
    println!("  Send this link to the person you are watching with:\n");
    println!("      {share}");
    println!("      ({how})\n");
    println!("  Then you both pick your own copy of the video and press play.");
    println!("  Anyone without that link is refused.\n");

    if let Some(m) = mapping {
        if blocked_by_vpn {
            println!("  Careful: your router is forwarding the port, but a VPN is carrying all");
            println!("  of this machine's outgoing traffic. An incoming connection arrives on");
            println!("  your network card and the reply leaves down the VPN tunnel, so the");
            println!("  other person's browser never gets an answer and the link above will");
            println!("  just hang.\n");
            println!("  Either disconnect the VPN while you watch, or use Tailscale instead,");
            println!("  which is built for exactly this and is encrypted too.\n");
        }
        println!("  Your router is now forwarding port {} to this machine, via {}.",
                 m.external.port(), m.protocol());
        if m.lease == 0 {
            println!("  Your router would only accept a permanent forward, so it is closed");
            println!("  on Ctrl-C rather than expiring on its own. If this is killed instead,");
            println!("  remove the forward in the router's admin page.");
        } else {
            println!("  It is closed on Ctrl-C, and expires by itself within {} minutes.",
                     m.lease / 60);
        }
        println!("  While it is open, that port is reachable from the whole internet, and the");
        println!("  connection is plain HTTP - the room code keeps people out, but anyone on");
        println!("  the path can see what you are watching. Tailscale avoids that.\n");
    }

    if open {
        println!("  Opening it here now. Ctrl-C to stop.\n");
    } else {
        println!("  Open it here: http://localhost:{port}{tail}   -   Ctrl-C to stop.\n");
    }

    if ts.is_none() && mapping.is_none() {
        println!("  Not on the same wifi as them? Two ways:");
        println!("    - run this again with --public, to ask your router to open the port");
        println!("    - or install Tailscale on both machines, which is encrypted:");
        if cfg!(target_os = "macos") {
            println!("        brew install --cask tailscale   (or get it from the App Store)\n");
        } else if cfg!(target_os = "windows") {
            println!("        winget install tailscale.tailscale\n");
        } else {
            println!("        Fedora:  sudo dnf install tailscale && sudo systemctl enable --now tailscaled && sudo tailscale up");
            println!("        Arch:    sudo pacman -S tailscale   && sudo systemctl enable --now tailscaled && sudo tailscale up\n");
        }
    }
}

/// Hand the host's own browser the page, so starting up is one step.
fn open_browser(port: u16, code: &str) {
    let url = format!("http://localhost:{port}/#{code}");

    // Every desktop has a "hand this to whatever handles it" command; they
    // just disagree about its name.
    let (exe, lead): (&str, &[&str]) = if cfg!(target_os = "macos") {
        ("open", &[])
    } else if cfg!(target_os = "windows") {
        // `start` is a cmd builtin. The empty string is its window-title
        // argument: without it cmd takes the URL as the title and opens
        // nothing.
        ("cmd", &["/C", "start", ""])
    } else {
        ("xdg-open", &[])
    };

    let _ = std::process::Command::new(exe)
        .args(lead)
        .arg(&url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// The address someone on the same wifi can actually reach us at.
///
/// The obvious trick - connect a socket towards the internet and read back
/// the local address - picks the VPN tunnel whenever one is up, and a
/// Mullvad or Tailscale address is useless to a friend in the next room. Ask
/// the routing table for the way to the *gateway* instead, which stays on
/// the real network.
fn lan_ip() -> Option<IpAddr> {
    let candidate = match portmap::default_gateway() {
        Some(gw) => portmap::local_addr_toward(gw),
        // No gateway worked out: fall back to the old probe, which is still
        // right on a machine with no VPN.
        None => portmap::local_addr_toward(Ipv4Addr::new(1, 1, 1, 1)),
    }?;

    // Advertising any of these as "works on this wifi" would be a lie.
    if candidate.is_loopback() || candidate.is_link_local() || portmap::is_cgnat(candidate) {
        return None;
    }
    Some(IpAddr::V4(candidate))
}

fn tailscale_ip() -> Option<String> {
    // On macOS and Windows the CLI ships inside the app and is often not on
    // PATH, so try the install locations as well.
    const CANDIDATES: &[&str] = &[
        "tailscale",
        "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
        "/usr/local/bin/tailscale",
        r"C:\Program Files\Tailscale\tailscale.exe",
    ];

    for exe in CANDIDATES {
        let Ok(out) = std::process::Command::new(exe).args(["ip", "-4"]).output() else {
            continue;
        };
        if !out.status.success() {
            continue;
        }
        let Ok(text) = String::from_utf8(out.stdout) else { continue };
        if let Some(first) = text.lines().next() {
            let first = first.trim();
            if !first.is_empty() {
                return Some(first.to_string());
            }
        }
    }
    None
}

/// Room code doubles as the shared secret, so take it from the OS CSPRNG.
/// Each character is 5 bits of a 32-symbol alphabet with no lookalikes.
fn gen_code(chars: usize) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut raw = vec![0u8; chars];
    // No weaker fallback: a guessable code is the one failure that matters,
    // and a machine with no working CSPRNG has larger problems.
    if getrandom::getrandom(&mut raw).is_err() {
        die("cannot read secure random bytes from the operating system");
    }
    raw.iter().map(|b| ALPHABET[(*b as usize) % ALPHABET.len()] as char).collect()
}

fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut diff = (a.len() ^ b.len()) as u32;
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).copied().unwrap_or(0) as u32;
        let y = b.get(i).copied().unwrap_or(0) as u32;
        diff |= x ^ y;
    }
    diff == 0
}

// ---------------------------------------------------------------- static files

fn asset(name: &str) -> Response {
    let (body, ctype) = match name {
        "index.html" => (include_str!("web/index.html"), "text/html; charset=utf-8"),
        "style.css" => (include_str!("web/style.css"), "text/css; charset=utf-8"),
        "app.js" => (include_str!("web/app.js"), "text/javascript; charset=utf-8"),
        _ => return (StatusCode::NOT_FOUND, "not found").into_response(),
    };
    let mut res = Response::new(body.into());
    res.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static(ctype));
    res.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res
}

// ----------------------------------------------------------------- websocket

async fn ws_upgrade(
    ws: WebSocketUpgrade,
    ConnectInfo(who): ConnectInfo<SocketAddr>,
    State(app): State<App>,
) -> Response {
    // Nothing this protocol sends is large. Tungstenite would otherwise
    // accept 64 MiB per message from an unauthenticated peer.
    ws.max_message_size(16 * 1024)
        .max_frame_size(16 * 1024)
        .on_upgrade(move |socket| serve_peer(socket, app, who.ip()))
}

async fn serve_peer(socket: WebSocket, app: App, who: IpAddr) {
    let (mut sink, mut stream) = socket.split();

    // The connection is anonymous until a `hello` with the right code
    // arrives, and it has a moment to do so rather than holding a slot open.
    let first = tokio::time::timeout(Duration::from_secs(10), stream.next()).await;
    let hello = match first {
        Ok(Some(Ok(Message::Text(t)))) => serde_json::from_str::<ClientMsg>(&t).ok(),
        _ => None,
    };
    let (name, file) = match hello {
        Some(ClientMsg::Hello { code, name, file }) if ct_eq(&code.to_uppercase(), &app.code) => {
            (clean_name(&name), file)
        }
        _ => {
            if app.guard.wrong_code(who) {
                // A second's pause costs an honest mistyped link nothing and
                // takes the speed out of guessing.
                tokio::time::sleep(WRONG_CODE_DELAY).await;
                let err = ServerMsg::Error { message: "wrong or missing room code".into() };
                let _ = sink.send(Message::Text(serde_json::to_string(&err).unwrap())).await;
            }
            return;
        }
    };

    let id = app.room.join(name, file);

    // Two outbound sources: room broadcasts, and replies meant only for us
    // (pong). One task owns the sink and multiplexes them.
    let (ptx, mut prx) = mpsc::unbounded_channel::<String>();
    let mut brx = app.room.subscribe();
    let pump = tokio::spawn(async move {
        loop {
            let msg = tokio::select! {
                m = prx.recv() => match m { Some(m) => m, None => break },
                m = brx.recv() => match m {
                    Ok(m) => m,
                    Err(broadcast::error::RecvError::Closed) => break,
                    // Lagged: the next full state snapshot repairs us anyway.
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                },
            };
            if sink.send(Message::Text(msg)).await.is_err() {
                break;
            }
        }
    });

    let welcome = ServerMsg::Welcome { id, now: now_ms() };
    let _ = ptx.send(serde_json::to_string(&welcome).unwrap());
    app.room.publish();

    while let Some(Ok(msg)) = stream.next().await {
        let text = match msg {
            Message::Text(t) => t,
            Message::Close(_) => break,
            _ => continue,
        };
        app.room.touch(id);
        let Ok(parsed) = serde_json::from_str::<ClientMsg>(&text) else { continue };
        match parsed {
            ClientMsg::Hello { .. } => {}
            ClientMsg::File { file } => app.room.set_file(id, file),
            ClientMsg::Pause { position } => app.room.pause(id, position),
            ClientMsg::Resume { .. } => app.room.resume(id),
            ClientMsg::Seek { position } => app.room.seek(position),
            ClientMsg::Stall { stalled } => app.room.stall(id, stalled),
            ClientMsg::Tick { drift, .. } => app.room.tick(id, drift),
            ClientMsg::Ping { c } => {
                let pong = ServerMsg::Pong { c, s: now_ms() };
                let _ = ptx.send(serde_json::to_string(&pong).unwrap());
            }
            ClientMsg::Chat { text } => {
                let text: String = text.chars().take(500).collect();
                if !text.trim().is_empty() {
                    app.room.send(&ServerMsg::Chat { from: app.room.name_of(id), text });
                }
            }
        }
    }

    pump.abort();
    app.room.leave(id);
}

fn clean_name(raw: &str) -> String {
    let n: String = raw.trim().chars().filter(|c| !c.is_control()).take(24).collect();
    if n.is_empty() {
        "guest".into()
    } else {
        n
    }
}
