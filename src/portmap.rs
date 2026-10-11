//! Asking the router to forward our port, so the link works from outside.
//!
//! Two protocols, tried in order, both spoken directly over std sockets so the
//! binary stays a single file with nothing to install:
//!
//!   * UPnP IGD - SSDP discovery, then SOAP over HTTP to the WAN service.
//!   * NAT-PMP / PCP - a few fixed-layout UDP packets to the default gateway.
//!
//! Everything here is best effort. A great many routers have UPnP switched
//! off, and that is a normal outcome rather than an error: the caller falls
//! back to the LAN link and the Tailscale advice.
//!
//! The whole attempt is bounded by `BUDGET` so startup cannot hang on a
//! router that listens but never answers.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream, UdpSocket};
use std::time::{Duration, Instant};

/// Upper bound on the whole discovery-and-map attempt.
const BUDGET: Duration = Duration::from_secs(6);
/// Devices stagger SSDP replies over MX seconds, so we must listen at least
/// that long. MX 2 with a 3s read window is what miniupnpc and friends use.
const MX: u32 = 2;
const SSDP_LISTEN: Duration = Duration::from_secs(3);
const SSDP_MULTICAST: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 250);
const SSDP_PORT: u16 = 1900;
/// A finite lease means a crash cannot leave the port open forever. Renewed
/// from a timer at half the granted value.
pub const LEASE_SECS: u32 = 3600;
/// Nothing a home router sends us should be anywhere near this large.
const MAX_BODY: usize = 256 * 1024;

const SEARCH_TARGETS: &[&str] = &[
    "urn:schemas-upnp-org:device:InternetGatewayDevice:1",
    "urn:schemas-upnp-org:service:WANIPConnection:1",
    "urn:schemas-upnp-org:service:WANPPPConnection:1",
    "upnp:rootdevice",
];

/// A live port forward. Dropping this does *not* release it; call `release`.
#[derive(Clone, Debug)]
pub struct Mapping {
    /// Address the outside world can reach, once the forward is in place.
    pub external: SocketAddrV4,
    pub internal_port: u16,
    /// Seconds the router agreed to, which may be less than we asked for.
    pub lease: u32,
    pub how: Method,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Method {
    /// The exact serviceType string and absolute control URL the router gave
    /// us. Both are kept verbatim: hardcoding WANIPConnection:1 breaks every
    /// PPPoE gateway, which advertises WANPPPConnection instead.
    Upnp { service: String, control: String, internal_ip: Ipv4Addr },
    NatPmp { gateway: Ipv4Addr },
    /// PCP carries a client-chosen nonce that identifies the mapping; losing
    /// it means we could never renew or delete our own forward.
    Pcp { gateway: Ipv4Addr, nonce: [u8; 12], client: Ipv4Addr },
}

impl Mapping {
    pub fn protocol(&self) -> &'static str {
        match self.how {
            Method::Upnp { .. } => "UPnP",
            Method::NatPmp { .. } => "NAT-PMP",
            Method::Pcp { .. } => "PCP",
        }
    }
}

/// Ask the router to forward `port`. Returns `Err` with a short, honest
/// reason the caller can print.
pub fn map(port: u16) -> Result<Mapping, String> {
    let deadline = Instant::now() + BUDGET;

    let gateway = default_gateway();

    // UPnP first: it reports the external address, so we can tell the user
    // whether the forward is actually reachable rather than just accepted.
    let upnp_err = match gateway.map(|gw| upnp_map(port, gw, deadline)).unwrap_or(Err(
        "could not work out this machine's default gateway".to_string(),
    )) {
        Ok(m) => return Ok(m),
        Err(e) => e,
    };

    if let Some(gw) = gateway {
        if let Ok(m) = pmp_map(port, gw, deadline) {
            return Ok(m);
        }
    }

    Err(upnp_err)
}

impl Mapping {
    /// Hand the port back. Best effort and quick: this runs while the user is
    /// waiting for the process to exit.
    pub fn release(&self) {
        match &self.how {
            Method::Upnp { service, control, .. } => {
                let body = format!(
                    "<NewRemoteHost></NewRemoteHost>\
                     <NewExternalPort>{}</NewExternalPort>\
                     <NewProtocol>TCP</NewProtocol>",
                    self.external.port()
                );
                let _ = soap(control, service, "DeletePortMapping", &body, Duration::from_millis(800));
            }
            Method::NatPmp { gateway } => {
                let _ = pmp_request(*gateway, &natpmp_packet(self.internal_port, 0, 0), 1, Duration::from_millis(400));
            }
            Method::Pcp { gateway, nonce, client } => {
                let pkt = pcp_packet(*nonce, *client, self.internal_port, self.external.port(), 0);
                let _ = pmp_request(*gateway, &pkt, 1, Duration::from_millis(400));
            }
        }
    }

    /// Re-assert the mapping before the lease runs out. Returns false when the
    /// router has forgotten us, which is the caller's cue to stop promising a
    /// working public link.
    pub fn renew(&self) -> bool {
        let deadline = Instant::now() + Duration::from_secs(4);
        match &self.how {
            Method::Upnp { service, control, internal_ip } => add_port_mapping(
                control,
                service,
                *internal_ip,
                self.internal_port,
                self.external.port(),
                LEASE_SECS,
                deadline,
            )
            .is_ok(),
            Method::NatPmp { gateway } => {
                pmp_map_with(*gateway, self.internal_port, self.external.port(), deadline).is_ok()
            }
            Method::Pcp { gateway, nonce, client } => {
                let pkt =
                    pcp_packet(*nonce, *client, self.internal_port, self.external.port(), LEASE_SECS);
                // Any datagram coming back is not success: an error response
                // is a datagram too. Check the result code and the lifetime.
                match pmp_request(*gateway, &pkt, 2, Duration::from_millis(600)) {
                    Ok(reply) => matches!(parse_pcp(&reply), Some((_, _, lease)) if lease > 0),
                    Err(_) => false,
                }
            }
        }
    }
}

// ------------------------------------------------------------------- UPnP

fn upnp_map(port: u16, gateway: Ipv4Addr, deadline: Instant) -> Result<Mapping, String> {
    let locations = ssdp_discover(gateway, deadline);
    if locations.is_empty() {
        return Err("no UPnP router answered".into());
    }

    let mut last = String::from("router refused the forward");
    for location in locations {
        // The budget covers the whole attempt, not just the search: a router
        // that accepts connections and never answers must not hold up
        // startup indefinitely.
        if Instant::now() >= deadline {
            break;
        }
        let Some(url) = Url::parse(&location) else { continue };
        // A hostile machine on the LAN can answer SSDP too. Only ever talk
        // SOAP to a private address, so a forged LOCATION cannot turn this
        // into a request against an arbitrary host.
        if !url.host.parse::<Ipv4Addr>().map(is_lan_device).unwrap_or(false) {
            continue;
        }
        let Ok(desc) = http_get(&url, remaining(deadline, Duration::from_secs(2))) else {
            continue;
        };

        for (service, control) in wan_services(&desc, &url) {
            if Instant::now() >= deadline {
                break;
            }
            // The address the router sees us from is the one it will accept as
            // NewInternalClient; deriving it this way is right even when a VPN
            // owns the default route.
            let Some(internal_ip) = local_addr_toward(url.host.parse().unwrap_or(gateway)) else {
                continue;
            };

            let external_ip = match get_external_ip(&control, &service, deadline) {
                Ok(ip) => ip,
                Err(e) => {
                    last = e;
                    continue;
                }
            };
            // A forward behind carrier-grade NAT or a second router is
            // accepted and then useless. Say so instead of printing a link
            // that cannot work.
            if unreachable_external(external_ip) {
                return Err(double_nat(external_ip));
            }

            let mut external_port = port;
            for attempt in 0..8 {
                if Instant::now() >= deadline {
                    break;
                }
                match add_port_mapping(
                    &control,
                    &service,
                    internal_ip,
                    port,
                    external_port,
                    LEASE_SECS,
                    deadline,
                ) {
                    Ok(lease) => {
                        return Ok(Mapping {
                            external: SocketAddrV4::new(external_ip, external_port),
                            internal_port: port,
                            lease,
                            how: Method::Upnp { service, control, internal_ip },
                        })
                    }
                    // 718 means that external port belongs to a different
                    // machine on the LAN; step to a fresh one in the dynamic
                    // range rather than giving up.
                    Err(e) if e.contains("718") => {
                        external_port = 49152 + (attempt * 137) % 16000;
                    }
                    Err(e) => {
                        last = e;
                        break;
                    }
                }
            }
        }
    }
    Err(last)
}

/// Send M-SEARCH to the multicast group and straight at the gateway, then
/// collect every LOCATION we are told about.
fn ssdp_discover(gateway: Ipv4Addr, deadline: Instant) -> Vec<String> {
    // Binding to the interface address rather than 0.0.0.0 is what makes the
    // datagram leave the right NIC; a wildcard bind picks one interface by
    // routing metric, which on a machine with Docker or a VPN is the wrong one.
    let local = local_addr_toward(gateway).unwrap_or(Ipv4Addr::UNSPECIFIED);
    let Ok(sock) = UdpSocket::bind(SocketAddrV4::new(local, 0)) else { return Vec::new() };
    let _ = sock.set_multicast_ttl_v4(2);
    let _ = sock.set_read_timeout(Some(Duration::from_millis(250)));

    let group = SocketAddrV4::new(SSDP_MULTICAST, SSDP_PORT);
    let direct = SocketAddrV4::new(gateway, SSDP_PORT);

    // UDP is lossy and the spec says to repeat; two rounds is plenty.
    for _round in 0..2 {
        for target in SEARCH_TARGETS {
            let msg = msearch(target);
            let _ = sock.send_to(&msg, group);
            let _ = sock.send_to(&msg, direct);
        }
        std::thread::sleep(Duration::from_millis(120));
    }

    let stop = (Instant::now() + SSDP_LISTEN).min(deadline);
    let mut found: Vec<String> = Vec::new();
    let mut buf = [0u8; 2048];
    while Instant::now() < stop {
        let Ok((n, _from)) = sock.recv_from(&mut buf) else { continue };
        let text = String::from_utf8_lossy(&buf[..n]);
        if let Some(loc) = header(&text, "LOCATION") {
            if !found.iter().any(|f| f == &loc) {
                found.push(loc);
            }
        }
    }
    found
}

fn msearch(target: &str) -> Vec<u8> {
    // Every line CRLF, and the trailing blank line is mandatory. MAN keeps its
    // literal double quotes, and a device must discard a search with no MX.
    format!(
        "M-SEARCH * HTTP/1.1\r\n\
         HOST: {SSDP_MULTICAST}:{SSDP_PORT}\r\n\
         MAN: \"ssdp:discover\"\r\n\
         MX: {MX}\r\n\
         ST: {target}\r\n\
         \r\n"
    )
    .into_bytes()
}

/// Case-insensitive header lookup in an HTTP-ish response.
fn header(text: &str, name: &str) -> Option<String> {
    for line in text.lines() {
        // Skip anything without a colon rather than giving up: the first line
        // is the status line, so bailing there would find no headers at all.
        let Some((k, v)) = line.split_once(':') else { continue };
        if k.trim().eq_ignore_ascii_case(name) {
            return Some(v.trim().to_string());
        }
    }
    None
}

/// Every WAN connection service in a device description, as
/// (serviceType, absolute control URL). Matching on serviceType matters: the
/// first `<controlURL>` in the document usually belongs to Layer3Forwarding,
/// which accepts the POST and cannot forward ports.
fn wan_services(xml: &str, location: &Url) -> Vec<(String, String)> {
    let base = tag(xml, "URLBase").unwrap_or_else(|| location.origin());
    let mut out = Vec::new();

    for chunk in xml.split("<service>").skip(1) {
        let Some(service) = tag(chunk, "serviceType") else { continue };
        let is_wan = service.starts_with("urn:schemas-upnp-org:service:WANIPConnection:")
            || service.starts_with("urn:schemas-upnp-org:service:WANPPPConnection:");
        if !is_wan {
            continue;
        }
        let Some(control) = tag(chunk, "controlURL") else { continue };
        out.push((service, join_url(&base, &control)));
    }
    out
}

/// Innermost text of the first `<name>` element, ignoring any namespace prefix.
fn tag(xml: &str, name: &str) -> Option<String> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(unescape(xml[start..end].trim()))
}

fn join_url(base: &str, path: &str) -> String {
    if path.starts_with("http://") || path.starts_with("https://") {
        return path.to_string();
    }
    let base = base.trim_end_matches('/');
    if path.starts_with('/') {
        format!("{base}{path}")
    } else {
        format!("{base}/{path}")
    }
}

/// Time left before the deadline, never more than `cap` and never zero.
fn remaining(deadline: Instant, cap: Duration) -> Duration {
    deadline.saturating_duration_since(Instant::now()).min(cap).max(Duration::from_millis(250))
}

fn get_external_ip(control: &str, service: &str, deadline: Instant) -> Result<Ipv4Addr, String> {
    let body = soap(
        control,
        service,
        "GetExternalIPAddress",
        "",
        remaining(deadline, Duration::from_secs(3)),
    )?;
    let raw = tag(&body, "NewExternalIPAddress")
        .ok_or_else(|| "router gave no external address".to_string())?;
    raw.parse()
        .map_err(|_| format!("router reported an external address we cannot read: {raw}"))
}

/// Returns the granted lease in seconds.
fn add_port_mapping(
    control: &str,
    service: &str,
    internal: Ipv4Addr,
    internal_port: u16,
    external_port: u16,
    lease: u32,
    deadline: Instant,
) -> Result<u32, String> {
    // The eight arguments must appear in this order, and NewRemoteHost must be
    // present even though it is empty: several consumer firmwares parse them
    // positionally and answer 402 Invalid Args otherwise.
    let args = |lease: u32| {
        format!(
            "<NewRemoteHost></NewRemoteHost>\
             <NewExternalPort>{external_port}</NewExternalPort>\
             <NewProtocol>TCP</NewProtocol>\
             <NewInternalPort>{internal_port}</NewInternalPort>\
             <NewInternalClient>{internal}</NewInternalClient>\
             <NewEnabled>1</NewEnabled>\
             <NewPortMappingDescription>{}</NewPortMappingDescription>\
             <NewLeaseDuration>{lease}</NewLeaseDuration>",
            escape("video sync play")
        )
    };

    let budget = remaining(deadline, Duration::from_secs(4));
    match soap(control, service, "AddPortMapping", &args(lease), budget) {
        Ok(_) => Ok(lease),
        // 725 OnlyPermanentLeasesSupported, and some firmwares answer 402 to
        // any non-zero lease. A permanent mapping is the only option left, so
        // releasing it on exit becomes the thing that matters.
        Err(e) if e.contains("725") || e.contains("402") => {
            soap(control, service, "AddPortMapping", &args(0), budget)?;
            Ok(0)
        }
        Err(e) => Err(e),
    }
}

/// Anything taken from the router's own XML and spliced into a request line
/// or header must not be able to end that line. A hostile machine on the LAN
/// can answer SSDP, so this is attacker-controlled input.
fn is_header_safe(s: &str) -> bool {
    !s.chars().any(|c| c.is_control())
}

fn soap(
    control: &str,
    service: &str,
    action: &str,
    args: &str,
    timeout: Duration,
) -> Result<String, String> {
    if !is_header_safe(control) || !is_header_safe(service) {
        return Err("router sent a malformed service description".into());
    }
    let url = Url::parse(control).ok_or_else(|| "bad control URL".to_string())?;
    if !url.host.parse::<Ipv4Addr>().map(is_lan_device).unwrap_or(false) {
        return Err("router pointed its control URL off the local network".into());
    }

    let body = format!(
        "<?xml version=\"1.0\"?>\
         <s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
         s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\">\
         <s:Body><u:{action} xmlns:u=\"{service}\">{args}</u:{action}></s:Body>\
         </s:Envelope>"
    );

    let request = format!(
        "POST {} HTTP/1.1\r\n\
         Host: {}:{}\r\n\
         Content-Type: text/xml; charset=\"utf-8\"\r\n\
         SOAPAction: \"{service}#{action}\"\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n{body}",
        url.path,
        url.host,
        url.port,
        body.len()
    );

    // One write, not two: an old Linksys only honoured SOAP requests that
    // arrived in a single packet, and nothing is lost by obliging.
    let response = http_round_trip(&url, request.as_bytes(), timeout)?;
    let (status, payload) = split_response(&response)?;

    if status == 200 {
        return Ok(payload);
    }
    // The fault carries the useful part. Prefixes are arbitrary per the XML
    // namespace rules, so match on the local name only.
    let code = tag(&payload, "errorCode").unwrap_or_else(|| status.to_string());
    let desc = tag(&payload, "errorDescription").unwrap_or_default();
    Err(format!("router refused {action} (error {code} {desc})").trim_end().to_string())
}

fn http_get(url: &Url, timeout: Duration) -> Result<String, String> {
    if !is_header_safe(&url.path) || !is_header_safe(&url.host) {
        return Err("router sent a malformed location".into());
    }
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {}:{}\r\nConnection: close\r\n\r\n",
        url.path, url.host, url.port
    );
    let response = http_round_trip(url, request.as_bytes(), timeout)?;
    Ok(split_response(&response)?.1)
}

fn http_round_trip(url: &Url, request: &[u8], timeout: Duration) -> Result<Vec<u8>, String> {
    let addr: SocketAddr = format!("{}:{}", url.host, url.port)
        .parse()
        .map_err(|_| "bad router address".to_string())?;
    let mut stream =
        TcpStream::connect_timeout(&addr, timeout).map_err(|e| format!("router unreachable: {e}"))?;
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(timeout));
    stream.write_all(request).map_err(|e| format!("router closed the connection: {e}"))?;

    // Read to EOF rather than trusting Content-Length: some gateways send
    // none, and others always answer chunked.
    let mut raw = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                raw.extend_from_slice(&buf[..n]);
                if raw.len() > MAX_BODY {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    Ok(raw)
}

/// Split an HTTP response into (status, body), undoing chunked framing.
///
/// Deliberately works on bytes: a router is free to send a body that is not
/// valid UTF-8, and chunk lengths count bytes. Converting first and slicing
/// afterwards would land in the middle of a replacement character and panic.
fn split_response(raw: &[u8]) -> Result<(u16, String), String> {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| "router sent a truncated reply".to_string())?;
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let body = &raw[split + 4..];

    let status = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse::<u16>().ok())
        .ok_or_else(|| "router sent no status line".to_string())?;

    let chunked = header(&head, "Transfer-Encoding")
        .map(|v| v.to_ascii_lowercase().contains("chunked"))
        .unwrap_or(false);

    let body = if chunked { dechunk(body) } else { body.to_vec() };
    Ok((status, String::from_utf8_lossy(&body).into_owned()))
}

fn dechunk(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut rest = body;
    loop {
        let Some(eol) = rest.windows(2).position(|w| w == b"\r\n") else { break };
        let size_line = String::from_utf8_lossy(&rest[..eol]);
        let Ok(size) =
            usize::from_str_radix(size_line.split(';').next().unwrap_or("").trim(), 16)
        else {
            break;
        };
        let after = &rest[eol + 2..];
        if size == 0 {
            break;
        }
        // A truncated body (we hit MAX_BODY, or the router died mid-chunk):
        // keep what arrived rather than reading past the end.
        if size > after.len() {
            out.extend_from_slice(after);
            break;
        }
        out.extend_from_slice(&after[..size]);
        rest = &after[size..];
        if rest.starts_with(b"\r\n") {
            rest = &rest[2..];
        }
    }
    out
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&amp;", "&")
}

/// The pieces of a URL we need. Enough for router description and control
/// URLs; not a general parser.
#[derive(Debug, PartialEq)]
struct Url {
    host: String,
    port: u16,
    path: String,
}

impl Url {
    fn parse(raw: &str) -> Option<Url> {
        let rest = raw.strip_prefix("http://")?;
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p.parse().ok()?),
            None => (authority.to_string(), 80),
        };
        if host.is_empty() {
            return None;
        }
        Some(Url { host, port, path: path.to_string() })
    }

    fn origin(&self) -> String {
        format!("http://{}:{}", self.host, self.port)
    }
}

// -------------------------------------------------------- NAT-PMP and PCP

fn pmp_map(port: u16, gateway: Ipv4Addr, deadline: Instant) -> Result<Mapping, String> {
    pmp_map_with(gateway, port, port, deadline)
}

fn pmp_map_with(
    gateway: Ipv4Addr,
    internal_port: u16,
    suggested: u16,
    _deadline: Instant,
) -> Result<Mapping, String> {
    let client = local_addr_toward(gateway).ok_or_else(|| "no route to the gateway".to_string())?;

    // PCP first; a PCP gateway answers version 2, and a NAT-PMP-only one
    // replies UNSUPP_VERSION, at which point we speak the older protocol.
    let mut nonce = [0u8; 12];
    if getrandom::getrandom(&mut nonce).is_ok() {
        let pkt = pcp_packet(nonce, client, internal_port, suggested, LEASE_SECS);
        if let Ok(reply) = pmp_request(gateway, &pkt, 3, Duration::from_millis(500)) {
            if let Some((ip, port, lease)) = parse_pcp(&reply) {
                if unreachable_external(ip) {
                    // Do not leave a forward behind that cannot work.
                    let undo = pcp_packet(nonce, client, internal_port, port, 0);
                    let _ = pmp_request(gateway, &undo, 1, Duration::from_millis(300));
                    return Err(double_nat(ip));
                }
                return Ok(Mapping {
                    external: SocketAddrV4::new(ip, port),
                    internal_port,
                    lease,
                    how: Method::Pcp { gateway, nonce, client },
                });
            }
        }
    }

    let pkt = natpmp_packet(internal_port, suggested, LEASE_SECS);
    let reply = pmp_request(gateway, &pkt, 3, Duration::from_millis(500))
        .map_err(|_| "the router does not speak NAT-PMP either".to_string())?;
    let (port, lease) =
        parse_natpmp_map(&reply).ok_or_else(|| "the router refused a NAT-PMP mapping".to_string())?;

    // NAT-PMP keeps the external address in a separate call.
    let ip = pmp_request(gateway, &[0u8, 0u8], 2, Duration::from_millis(400))
        .ok()
        .and_then(|r| parse_natpmp_addr(&r))
        .ok_or_else(|| "the router would not say what its public address is".to_string())?;

    if unreachable_external(ip) {
        let _ = pmp_request(gateway, &natpmp_packet(internal_port, 0, 0), 1, Duration::from_millis(300));
        return Err(double_nat(ip));
    }

    Ok(Mapping {
        external: SocketAddrV4::new(ip, port),
        internal_port,
        lease,
        how: Method::NatPmp { gateway },
    })
}

/// NAT-PMP map request: version 0, opcode 2 (TCP), reserved, ports, lifetime.
fn natpmp_packet(internal: u16, suggested: u16, lifetime: u32) -> Vec<u8> {
    let mut p = Vec::with_capacity(12);
    p.push(0); // version
    p.push(2); // opcode 2 = map TCP
    p.extend_from_slice(&0u16.to_be_bytes()); // reserved
    p.extend_from_slice(&internal.to_be_bytes());
    p.extend_from_slice(&suggested.to_be_bytes());
    p.extend_from_slice(&lifetime.to_be_bytes());
    p
}

/// PCP MAP request (RFC 6887): 24-byte header then the 36-byte MAP body.
fn pcp_packet(
    nonce: [u8; 12],
    client: Ipv4Addr,
    internal: u16,
    suggested: u16,
    lifetime: u32,
) -> Vec<u8> {
    let mut p = Vec::with_capacity(60);
    p.push(2); // version
    p.push(1); // opcode 1 = MAP, request (R bit clear)
    p.extend_from_slice(&0u16.to_be_bytes()); // reserved
    p.extend_from_slice(&lifetime.to_be_bytes());
    p.extend_from_slice(&v4_mapped(client)); // client address, IPv4-mapped
    p.extend_from_slice(&nonce);
    p.push(6); // protocol 6 = TCP
    p.extend_from_slice(&[0, 0, 0]); // reserved
    p.extend_from_slice(&internal.to_be_bytes());
    p.extend_from_slice(&suggested.to_be_bytes());
    p.extend_from_slice(&v4_mapped(Ipv4Addr::UNSPECIFIED)); // any external address
    p
}

fn v4_mapped(ip: Ipv4Addr) -> [u8; 16] {
    let mut out = [0u8; 16];
    out[10] = 0xff;
    out[11] = 0xff;
    out[12..16].copy_from_slice(&ip.octets());
    out
}

/// (external ip, external port, granted lifetime) from a PCP MAP response.
fn parse_pcp(r: &[u8]) -> Option<(Ipv4Addr, u16, u32)> {
    // Order matters: a NAT-PMP "unsupported version" reply is only 8 bytes, so
    // check the version before applying PCP's 24-byte floor.
    if r.len() < 4 || r[0] != 2 {
        return None;
    }
    if r.len() < 60 || r[1] != 0x81 || r[3] != 0 {
        return None;
    }
    let lifetime = u32::from_be_bytes([r[4], r[5], r[6], r[7]]);
    let port = u16::from_be_bytes([r[42], r[43]]);
    let ip = Ipv4Addr::new(r[56], r[57], r[58], r[59]);
    Some((ip, port, lifetime))
}

/// (external port, granted lifetime) from a NAT-PMP map response.
fn parse_natpmp_map(r: &[u8]) -> Option<(u16, u32)> {
    if r.len() < 16 || r[0] != 0 || r[1] != 130 {
        return None;
    }
    if u16::from_be_bytes([r[2], r[3]]) != 0 {
        return None;
    }
    Some((u16::from_be_bytes([r[10], r[11]]), u32::from_be_bytes([r[12], r[13], r[14], r[15]])))
}

fn parse_natpmp_addr(r: &[u8]) -> Option<Ipv4Addr> {
    if r.len() < 12 || r[0] != 0 || r[1] != 128 {
        return None;
    }
    if u16::from_be_bytes([r[2], r[3]]) != 0 {
        return None;
    }
    Some(Ipv4Addr::new(r[8], r[9], r[10], r[11]))
}

/// One request with a short retry ladder. The RFC's own schedule runs to two
/// minutes before giving up, which is far too long to sit in front of someone
/// waiting for the program to start.
fn pmp_request(
    gateway: Ipv4Addr,
    packet: &[u8],
    tries: u32,
    first_wait: Duration,
) -> Result<Vec<u8>, String> {
    let sock = UdpSocket::bind("0.0.0.0:0").map_err(|e| e.to_string())?;
    sock.connect(SocketAddrV4::new(gateway, 5351)).map_err(|e| e.to_string())?;

    let mut wait = first_wait;
    for _ in 0..tries {
        sock.send(packet).map_err(|e| e.to_string())?;
        let _ = sock.set_read_timeout(Some(wait));
        let mut buf = [0u8; 1100];
        match sock.recv(&mut buf) {
            Ok(n) => return Ok(buf[..n].to_vec()),
            // A refused connection means nothing is listening; retrying is
            // just waiting for nothing.
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
                return Err("nothing listening".into())
            }
            Err(_) => wait *= 2,
        }
    }
    Err("no reply".into())
}

// ------------------------------------------------------------- addresses

/// Which of our own addresses the kernel would use to reach `peer`. No packet
/// is sent; this just asks the routing table.
pub fn local_addr_toward(peer: Ipv4Addr) -> Option<Ipv4Addr> {
    let sock = UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect(SocketAddrV4::new(peer, 9)).ok()?;
    match sock.local_addr().ok()? {
        SocketAddr::V4(a) => Some(*a.ip()),
        SocketAddr::V6(_) => None,
    }
}

/// The default gateway, read from the routing table.
pub fn default_gateway() -> Option<Ipv4Addr> {
    // Looked up once: on Windows this shells out to PowerShell, which is slow
    // enough that doing it twice is visible at startup.
    static CACHED: std::sync::OnceLock<Option<Ipv4Addr>> = std::sync::OnceLock::new();
    *CACHED.get_or_init(probe_gateway)
}

fn probe_gateway() -> Option<Ipv4Addr> {
    // Escape hatch for odd setups, and the seam the tests point at a fake
    // router.
    if let Some(forced) = std::env::var_os("VSP_GATEWAY") {
        return forced.to_str()?.trim().parse().ok();
    }

    #[cfg(target_os = "linux")]
    {
        // /proc/net/route, little-endian hex. The default route is the one
        // with a zero destination and a zero mask.
        let text = std::fs::read_to_string("/proc/net/route").ok()?;
        let mut best: Option<(u32, Ipv4Addr)> = None;
        for line in text.lines().skip(1) {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() < 11 || f[1] != "00000000" || f[7] != "00000000" {
                continue;
            }
            let Ok(raw) = u32::from_str_radix(f[2], 16) else { continue };
            // An on-link default route (a VPN or PPPoE link) has no next hop
            // and stores 0.0.0.0 here. Taking it literally would send every
            // probe to this machine; report no gateway so the caller falls
            // back, which is what the other platforms already do.
            if raw == 0 {
                continue;
            }
            let metric: u32 = f[6].parse().unwrap_or(u32::MAX);
            let ip = Ipv4Addr::from(raw.to_le_bytes());
            if best.map(|(m, _)| metric < m).unwrap_or(true) {
                best = Some((metric, ip));
            }
        }
        return best.map(|(_, ip)| ip);
    }

    #[cfg(not(target_os = "linux"))]
    {
        gateway_from_command()
    }
}

#[cfg(not(target_os = "linux"))]
fn gateway_from_command() -> Option<Ipv4Addr> {
    use std::process::Command;

    let out = if cfg!(target_os = "windows") {
        Command::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                "(Get-NetRoute -DestinationPrefix '0.0.0.0/0' | Sort-Object RouteMetric | \
                 Select-Object -First 1).NextHop",
            ])
            .output()
            .ok()?
    } else {
        // macOS and the BSDs.
        Command::new("route").args(["-n", "get", "default"]).output().ok()?
    };

    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("gateway:") {
            if let Ok(ip) = rest.trim().parse() {
                return Some(ip);
            }
        }
        // PowerShell prints the address on its own.
        if let Ok(ip) = line.parse::<Ipv4Addr>() {
            if !ip.is_unspecified() {
                return Some(ip);
            }
        }
    }
    None
}

/// An address the internet cannot route back to: another NAT sits above the
/// router, so the forward is accepted and still useless.
pub fn unreachable_external(ip: Ipv4Addr) -> bool {
    is_private(ip) || is_cgnat(ip) || ip.is_unspecified() || ip.is_loopback()
}

fn double_nat(ip: Ipv4Addr) -> String {
    format!(
        "your router forwarded the port, but its own address ({ip}) is not a public one - \
         there is a second router or carrier-grade NAT above it, so the internet cannot \
         reach you this way"
    )
}

pub fn is_private(ip: Ipv4Addr) -> bool {
    ip.is_private() || ip.is_link_local() || ip.is_loopback()
}

/// May we send router requests to this address?
///
/// Deliberately narrower than `is_private`: loopback is *not* a router, and
/// allowing it would let a forged SSDP reply aim our requests at services
/// running on this very machine. The one exception is an explicitly
/// configured gateway, which is how the tests point this at a fake router.
pub fn is_lan_device(ip: Ipv4Addr) -> bool {
    if ip.is_private() || ip.is_link_local() {
        return true;
    }
    matches!(std::env::var("VSP_GATEWAY"), Ok(v) if v.trim().parse() == Ok(ip))
}

/// 100.64.0.0/10, the range carriers use for their own NAT. Seeing one as our
/// "public" address means there is another NAT above the router.
pub fn is_cgnat(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    o[0] == 100 && (64..128).contains(&o[1])
}

// ------------------------------------------------------------------ tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn msearch_is_exactly_what_the_spec_asks_for() {
        let m = String::from_utf8(msearch("upnp:rootdevice")).unwrap();
        assert_eq!(
            m,
            "M-SEARCH * HTTP/1.1\r\n\
             HOST: 239.255.255.250:1900\r\n\
             MAN: \"ssdp:discover\"\r\n\
             MX: 2\r\n\
             ST: upnp:rootdevice\r\n\
             \r\n"
        );
        // A device must discard a search with no MX, and the quotes around
        // ssdp:discover are part of the value.
        assert!(m.contains("MAN: \"ssdp:discover\""));
        assert!(m.ends_with("\r\n\r\n"));
    }

    #[test]
    fn location_header_is_found_case_insensitively() {
        let reply = "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age=120\r\n\
                     location: http://192.168.1.1:5000/desc.xml\r\n\r\n";
        assert_eq!(header(reply, "LOCATION").as_deref(), Some("http://192.168.1.1:5000/desc.xml"));
    }

    #[test]
    fn urls_parse_and_resolve() {
        let u = Url::parse("http://192.168.1.254:5000/rootDesc.xml").unwrap();
        assert_eq!(u.host, "192.168.1.254");
        assert_eq!(u.port, 5000);
        assert_eq!(u.path, "/rootDesc.xml");
        assert_eq!(u.origin(), "http://192.168.1.254:5000");

        // No port means 80, and no path means root.
        let d = Url::parse("http://10.0.0.1").unwrap();
        assert_eq!((d.port, d.path.as_str()), (80, "/"));

        assert_eq!(join_url("http://h:5000", "/ctl/IPConn"), "http://h:5000/ctl/IPConn");
        assert_eq!(join_url("http://h:5000/", "ctl"), "http://h:5000/ctl");
        assert_eq!(join_url("http://h:5000", "http://other/ctl"), "http://other/ctl");
        assert!(Url::parse("ftp://nope/").is_none());
    }

    #[test]
    fn picks_the_wan_service_not_the_first_control_url() {
        // Layer3Forwarding comes first in a real description and would happily
        // accept the POST while being unable to forward anything.
        let xml = r#"<root><URLBase>http://192.168.1.254:5000/</URLBase><device>
          <serviceList><service>
            <serviceType>urn:schemas-upnp-org:service:Layer3Forwarding:1</serviceType>
            <controlURL>/ctl/L3F</controlURL></service></serviceList>
          <deviceList><device><deviceList><device><serviceList>
            <service>
              <serviceType>urn:schemas-upnp-org:service:WANPPPConnection:1</serviceType>
              <controlURL>/ctl/PPPConn</controlURL></service>
          </serviceList></device></deviceList></device></deviceList></device></root>"#;
        let loc = Url::parse("http://192.168.1.254:5000/rootDesc.xml").unwrap();
        let found = wan_services(xml, &loc);
        assert_eq!(
            found,
            vec![(
                "urn:schemas-upnp-org:service:WANPPPConnection:1".to_string(),
                "http://192.168.1.254:5000/ctl/PPPConn".to_string()
            )]
        );
    }

    #[test]
    fn url_base_is_preferred_over_the_location_origin() {
        let xml = "<root><URLBase>http://10.1.1.1:80/</URLBase><service>\
                   <serviceType>urn:schemas-upnp-org:service:WANIPConnection:2</serviceType>\
                   <controlURL>/x</controlURL></service></root>";
        let loc = Url::parse("http://192.168.0.1:49152/d.xml").unwrap();
        assert_eq!(wan_services(xml, &loc)[0].1, "http://10.1.1.1:80/x");
    }

    #[test]
    fn soap_faults_report_the_upnp_error_code() {
        let fault = "<s:Envelope><s:Body><s:Fault><detail><UPnPError>\
                     <errorCode>725</errorCode>\
                     <errorDescription>OnlyPermanentLeasesSupported</errorDescription>\
                     </UPnPError></detail></s:Fault></s:Body></s:Envelope>";
        assert_eq!(tag(fault, "errorCode").as_deref(), Some("725"));
        assert_eq!(tag(fault, "errorDescription").as_deref(), Some("OnlyPermanentLeasesSupported"));
    }

    #[test]
    fn chunked_bodies_are_reassembled() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n\
                    4\r\n<abc\r\n3\r\ndef\r\n0\r\n\r\n";
        assert_eq!(split_response(raw).unwrap(), (200, "<abcdef".to_string()));
    }

    #[test]
    fn plain_bodies_survive_untouched() {
        let raw = b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 5\r\n\r\nhello";
        assert_eq!(split_response(raw).unwrap(), (500, "hello".to_string()));
    }

    #[test]
    fn a_chunk_split_inside_a_character_does_not_panic() {
        // The router is free to end a chunk in the middle of a multi-byte
        // character. Converting to text before slicing used to panic here.
        let mut raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
        raw.extend_from_slice(b"1\r\n\xc3\r\n1\r\n\xa9\r\n0\r\n\r\n");
        let (status, body) = split_response(&raw).unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, "\u{e9}", "the two halves rejoin into one character");
    }

    #[test]
    fn a_truncated_chunk_is_kept_rather_than_read_past() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nff\r\nshort";
        assert_eq!(split_response(raw).unwrap(), (200, "short".to_string()));
    }

    #[test]
    fn header_injection_from_the_router_is_refused() {
        // A hostile machine on the LAN can answer SSDP, so the serviceType
        // and control URL are attacker-controlled.
        assert!(!is_header_safe("urn:x\r\nX-Injected: 1"));
        assert!(!is_header_safe("/ctl HTTP/1.1\r\nHost: 127.0.0.1"));
        assert!(is_header_safe("urn:schemas-upnp-org:service:WANIPConnection:1"));
        assert!(is_header_safe("/ctl/IPConn"));
    }

    #[test]
    fn only_real_lan_addresses_are_treated_as_routers() {
        assert!(is_lan_device(Ipv4Addr::new(192, 168, 1, 1)));
        assert!(is_lan_device(Ipv4Addr::new(10, 0, 0, 1)));
        assert!(is_lan_device(Ipv4Addr::new(169, 254, 1, 1)));
        // Loopback is not a router: allowing it would let a forged SSDP
        // reply aim our requests at this machine's own services.
        assert!(!is_lan_device(Ipv4Addr::new(127, 0, 0, 1)));
        assert!(!is_lan_device(Ipv4Addr::new(8, 8, 8, 8)));
    }

    #[test]
    fn descriptions_are_xml_escaped() {
        assert_eq!(escape("a & b <c>"), "a &amp; b &lt;c&gt;");
        assert_eq!(unescape("a &amp; b &lt;c&gt;"), "a & b <c>");
    }

    #[test]
    fn natpmp_request_layout() {
        let p = natpmp_packet(8787, 8787, 3600);
        assert_eq!(p.len(), 12);
        assert_eq!(p[0], 0, "version 0");
        assert_eq!(p[1], 2, "opcode 2 = map TCP");
        assert_eq!(&p[4..6], &8787u16.to_be_bytes());
        assert_eq!(&p[8..12], &3600u32.to_be_bytes());

        // A release is the same packet with a zero lifetime.
        assert_eq!(&natpmp_packet(8787, 0, 0)[8..12], &[0, 0, 0, 0]);
    }

    #[test]
    fn natpmp_responses_parse_and_are_validated() {
        let mut r = vec![0u8, 130, 0, 0];
        r.extend_from_slice(&1234u32.to_be_bytes()); // epoch
        r.extend_from_slice(&8787u16.to_be_bytes()); // internal
        r.extend_from_slice(&49200u16.to_be_bytes()); // external, not what we asked
        r.extend_from_slice(&1800u32.to_be_bytes()); // granted, less than requested
        assert_eq!(parse_natpmp_map(&r), Some((49200, 1800)));

        // A non-zero result code is a refusal, not a mapping.
        let mut bad = r.clone();
        bad[2] = 0;
        bad[3] = 2;
        assert_eq!(parse_natpmp_map(&bad), None);
        assert_eq!(parse_natpmp_map(&r[..8]), None, "short packet");
    }

    #[test]
    fn pcp_request_layout() {
        let nonce = [7u8; 12];
        let p = pcp_packet(nonce, Ipv4Addr::new(192, 168, 1, 70), 8787, 8787, 3600);
        assert_eq!(p.len(), 60);
        assert_eq!(p[0], 2, "version 2");
        assert_eq!(p[1], 1, "MAP opcode, request");
        assert_eq!(&p[4..8], &3600u32.to_be_bytes());
        // Client address is IPv4-mapped IPv6.
        assert_eq!(&p[8..24], &v4_mapped(Ipv4Addr::new(192, 168, 1, 70)));
        assert_eq!(&p[24..36], &nonce);
        assert_eq!(p[36], 6, "TCP");
        assert_eq!(&p[40..42], &8787u16.to_be_bytes());
    }

    #[test]
    fn pcp_rejects_a_natpmp_unsupported_version_reply() {
        // 8 bytes, version 0: shorter than PCP's minimum, so the version test
        // has to come first or this panics on a slice out of range.
        let legacy = vec![0u8, 128, 0, 1, 0, 0, 0, 0];
        assert_eq!(parse_pcp(&legacy), None);
        assert_eq!(parse_pcp(&[]), None);
    }

    #[test]
    fn pcp_success_is_parsed() {
        let mut r = vec![0u8; 60];
        r[0] = 2;
        r[1] = 0x81; // MAP response
        r[3] = 0; // success
        r[4..8].copy_from_slice(&1800u32.to_be_bytes());
        r[42..44].copy_from_slice(&49200u16.to_be_bytes());
        r[56..60].copy_from_slice(&[203, 0, 113, 5]);
        assert_eq!(parse_pcp(&r), Some((Ipv4Addr::new(203, 0, 113, 5), 49200, 1800)));

        r[3] = 2; // NOT_AUTHORIZED
        assert_eq!(parse_pcp(&r), None);
    }

    #[test]
    fn unreachable_external_addresses_are_recognised() {
        assert!(is_cgnat(Ipv4Addr::new(100, 64, 0, 1)));
        assert!(is_cgnat(Ipv4Addr::new(100, 127, 255, 254)));
        assert!(!is_cgnat(Ipv4Addr::new(100, 128, 0, 1)));
        assert!(!is_cgnat(Ipv4Addr::new(100, 63, 255, 255)));
        assert!(is_private(Ipv4Addr::new(192, 168, 1, 1)));
        assert!(is_private(Ipv4Addr::new(10, 0, 0, 1)));
        assert!(is_private(Ipv4Addr::new(172, 16, 0, 1)));
        assert!(!is_private(Ipv4Addr::new(8, 8, 8, 8)));
    }
}
