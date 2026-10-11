//! video sync play — watch a local video file in step with someone else.
//!
//! One person runs the binary and shares the printed link. Both open it, both
//! drop in their own copy of the file, and the room keeps the two playheads
//! locked together. The video data itself never leaves either machine: only
//! play/pause/seek state crosses the wire.

mod protocol;
mod room;

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{broadcast, mpsc};

use protocol::{ClientMsg, ServerMsg};
use room::{now_ms, Room};

#[derive(Clone)]
struct App {
    room: Arc<Room>,
    code: String,
}

struct Args {
    port: u16,
    bind: IpAddr,
    code: Option<String>,
    open: bool,
}

fn parse_args() -> Args {
    let mut a = Args { port: 8787, bind: "0.0.0.0".parse().unwrap(), code: None, open: true };
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
                a.code = Some(next(i).to_uppercase());
                i += 1;
            }
            "--no-open" => a.open = false,
            "--help" | "-h" => {
                println!(
                    "video sync play — watch a video in step with a friend\n\n\
                     Usage: video-sync-play [--port 8787] [--bind 0.0.0.0] [--code WORD] [--no-open]\n\n\
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

fn die(msg: &str) -> ! {
    eprintln!("video-sync-play: {msg}");
    std::process::exit(2);
}

#[tokio::main]
async fn main() {
    let args = parse_args();
    let code = args.code.unwrap_or_else(gen_code);

    let app = App { room: Arc::new(Room::new()), code: code.clone() };

    // Watchdog for peers that stop responding without closing the socket.
    let watchdog = app.room.clone();
    tokio::spawn(async move {
        let mut beat = tokio::time::interval(std::time::Duration::from_millis(1000));
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

    banner(args.port, &code, args.open);
    if args.open {
        open_browser(args.port, &code);
    }

    axum::serve(listener, router)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            println!("\nbye.");
        })
        .await
        .unwrap();
}

fn banner(port: u16, code: &str, open: bool) {
    // Lead with the one link worth sending: whichever reaches the other person
    // from furthest away.
    let tail = format!(":{port}/#{code}");
    let (share, how) = match (tailscale_ip(), lan_ip()) {
        (Some(ts), _) => (format!("http://{ts}{tail}"), "works from anywhere"),
        (None, Some(lan)) => (format!("http://{lan}{tail}"), "works on this wifi"),
        (None, None) => (format!("http://localhost{tail}"), "this machine only"),
    };

    println!("\n  video sync play\n");
    println!("  Send this link to the person you are watching with:\n");
    println!("      {share}");
    println!("      ({how})\n");
    println!("  Then you both pick your own copy of the video and press play.");
    println!("  Anyone without that link is refused.\n");

    if open {
        println!("  Opening it here now. Ctrl-C to stop.\n");
    } else {
        println!("  Open it here: http://localhost{tail}   -   Ctrl-C to stop.\n");
    }

    if tailscale_ip().is_none() {
        println!("  Not on the same wifi as them? Install Tailscale on both machines");
        println!("  (free, nothing to rent, encrypted) and re-run this:");
        if cfg!(target_os = "macos") {
            println!("    brew install --cask tailscale   (or get it from the App Store)\n");
        } else if cfg!(target_os = "windows") {
            println!("    winget install tailscale.tailscale\n");
        } else {
            println!("    Fedora:  sudo dnf install tailscale && sudo systemctl enable --now tailscaled && sudo tailscale up");
            println!("    Arch:    sudo pacman -S tailscale   && sudo systemctl enable --now tailscaled && sudo tailscale up\n");
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

/// Best-effort local address: ask the kernel which source IP it would use to
/// reach the internet. No packets are actually sent.
fn lan_ip() -> Option<IpAddr> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("1.1.1.1:80").ok()?;
    let ip = s.local_addr().ok()?.ip();
    if ip.is_loopback() {
        None
    } else {
        Some(ip)
    }
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

/// Room code doubles as the shared secret, so take it from the OS CSPRNG
/// rather than from the clock. 8 chars of a 32-symbol alphabet = 40 bits.
fn gen_code() -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut seed = [0u8; 8];
    let mut x = match getrandom::getrandom(&mut seed) {
        Ok(()) => u64::from_le_bytes(seed),
        // No CSPRNG is a bad sign, but a weak code beats refusing to start.
        Err(_) => std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x1234_5678)
            ^ ((std::process::id() as u64) << 32),
    };
    let mut out = String::with_capacity(8);
    for _ in 0..8 {
        // splitmix64, so each output char depends on all the seed bits.
        x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = x;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        out.push(ALPHABET[(z % ALPHABET.len() as u64) as usize] as char);
    }
    out
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

async fn ws_upgrade(ws: WebSocketUpgrade, State(app): State<App>) -> Response {
    ws.on_upgrade(move |socket| serve_peer(socket, app))
}

async fn serve_peer(socket: WebSocket, app: App) {
    let (mut sink, mut stream) = socket.split();

    // The connection is anonymous until a `hello` with the right code arrives.
    let hello = match stream.next().await {
        Some(Ok(Message::Text(t))) => serde_json::from_str::<ClientMsg>(&t).ok(),
        _ => None,
    };
    let (name, file) = match hello {
        Some(ClientMsg::Hello { code, name, file }) if ct_eq(&code.to_uppercase(), &app.code) => {
            (clean_name(&name), file)
        }
        _ => {
            let err = ServerMsg::Error { message: "wrong or missing room code".into() };
            let _ = sink.send(Message::Text(serde_json::to_string(&err).unwrap())).await;
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
