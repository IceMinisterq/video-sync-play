"""End-to-end test of the port-mapping code against a fake UPnP router.

A real router cannot be reached from CI, so this stands one up: a UDP
listener that answers SSDP on loopback, and an HTTP server that serves a
device description and handles the SOAP calls. The binary is pointed at it
with VSP_GATEWAY and must come out the other side printing a public link,
then withdraw the mapping on Ctrl-C.

    python3 tests/portmap.py [path-to-binary]

Needs nothing installed - stdlib only.
"""
import http.server, os, re, signal, socket, subprocess, sys, threading, time

BINARY = sys.argv[1] if len(sys.argv) > 1 else "target/release/video-sync-play"
HTTP_PORT, APP_PORT = 8977, 8978
EXTERNAL_IP, EXTERNAL_PORT = "203.0.113.7", 8978

calls = []          # (action, body) seen by the fake router
fails = []

DESC = f"""<?xml version="1.0"?>
<root xmlns="urn:schemas-upnp-org:device-1-0">
  <URLBase>http://127.0.0.1:{HTTP_PORT}/</URLBase>
  <device>
    <deviceType>urn:schemas-upnp-org:device:InternetGatewayDevice:1</deviceType>
    <serviceList>
      <service>
        <serviceType>urn:schemas-upnp-org:service:Layer3Forwarding:1</serviceType>
        <controlURL>/ctl/L3F</controlURL>
      </service>
    </serviceList>
    <deviceList><device><deviceList><device>
      <serviceList>
        <service>
          <serviceType>urn:schemas-upnp-org:service:WANPPPConnection:1</serviceType>
          <controlURL>/ctl/PPP</controlURL>
        </service>
      </serviceList>
    </device></deviceList></device></deviceList>
  </device>
</root>"""


def soap_ok(action, inner=""):
    return (f'<?xml version="1.0"?><s:Envelope '
            f'xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">'
            f'<s:Body><u:{action}Response '
            f'xmlns:u="urn:schemas-upnp-org:service:WANPPPConnection:1">'
            f'{inner}</u:{action}Response></s:Body></s:Envelope>')


class Router(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a): pass

    def _send(self, body, status=200):
        raw = body.encode()
        self.send_response(status)
        self.send_header("Content-Type", 'text/xml; charset="utf-8"')
        self.send_header("Content-Length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def do_GET(self):
        self._send(DESC)

    def do_POST(self):
        n = int(self.headers.get("Content-Length", 0))
        body = self.rfile.read(n).decode()
        action = (self.headers.get("SOAPAction") or "").strip('"').split("#")[-1]
        calls.append((action, body, dict(self.headers)))

        if action == "GetExternalIPAddress":
            self._send(soap_ok(action, f"<NewExternalIPAddress>{EXTERNAL_IP}</NewExternalIPAddress>"))
        elif action in ("AddPortMapping", "DeletePortMapping"):
            self._send(soap_ok(action))
        else:
            self._send("<fault/>", 500)


def ssdp_responder(stop):
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.bind(('127.0.0.1', 1900))
    s.settimeout(0.3)
    while not stop.is_set():
        try:
            data, addr = s.recvfrom(2048)
        except socket.timeout:
            continue
        if b"M-SEARCH" not in data:
            continue
        reply = (
            "HTTP/1.1 200 OK\r\n"
            "CACHE-CONTROL: max-age=120\r\n"
            "EXT:\r\n"
            f"LOCATION: http://127.0.0.1:{HTTP_PORT}/rootDesc.xml\r\n"
            "ST: urn:schemas-upnp-org:device:InternetGatewayDevice:1\r\n"
            "USN: uuid:fake::urn:schemas-upnp-org:device:InternetGatewayDevice:1\r\n"
            "\r\n"
        ).encode()
        s.sendto(reply, addr)
    s.close()


def check(label, got, want):
    ok = got == want
    print(f"  {'PASS' if ok else 'FAIL'}  {label}: got {got!r}, want {want!r}")
    if not ok: fails.append(label)


def check_true(label, cond, detail=""):
    print(f"  {'PASS' if cond else 'FAIL'}  {label}" + ("" if cond else f"  ({detail})"))
    if not cond: fails.append(label)


if not os.path.exists(BINARY):
    sys.exit(f"no binary at {BINARY} - run: cargo build --release")

stop = threading.Event()
threading.Thread(target=ssdp_responder, args=(stop,), daemon=True).start()
httpd = http.server.ThreadingHTTPServer(('127.0.0.1', HTTP_PORT), Router)
threading.Thread(target=httpd.serve_forever, daemon=True).start()
time.sleep(0.4)

env = dict(os.environ, VSP_GATEWAY="127.0.0.1")
proc = subprocess.Popen(
    [BINARY, "--public", "--port", str(APP_PORT), "--no-open", "--code", "PORTMAPTEST1"],
    stdout=subprocess.PIPE, stderr=subprocess.STDOUT, env=env, text=True)

out_lines = []
def drain():
    for line in proc.stdout:
        out_lines.append(line.rstrip())
threading.Thread(target=drain, daemon=True).start()

# Discovery sends two rounds then listens; give it room.
deadline = time.time() + 20
while time.time() < deadline and not any("203.0.113.7" in l for l in out_lines):
    time.sleep(0.3)
out = "\n".join(out_lines)

print("\n[discovery and mapping]")
adds = [c for c in calls if c[0] == "AddPortMapping"]
check_true("router was discovered and asked for a mapping", bool(adds),
           f"calls seen: {[c[0] for c in calls]}")
check_true("external address was checked before trusting the mapping",
           any(c[0] == "GetExternalIPAddress" for c in calls))
check_true("public link is advertised with the router's external address",
           f"http://{EXTERNAL_IP}:{EXTERNAL_PORT}/#PORTMAPTEST1" in out,
           out[:400])

print("\n[SOAP shape - the parts real routers are fussy about]")
if adds:
    action, body, headers = adds[0]
    order = re.findall(r"<New(\w+)>", body)
    check("the eight arguments are in the order the spec lists", order,
          ["RemoteHost", "ExternalPort", "Protocol", "InternalPort",
           "InternalClient", "Enabled", "PortMappingDescription", "LeaseDuration"])
    check_true("NewRemoteHost is present though empty",
               "<NewRemoteHost></NewRemoteHost>" in body)
    check_true("NewEnabled is 1, not true", "<NewEnabled>1</NewEnabled>" in body)
    check_true("the service type is the one the router advertised, verbatim",
               "urn:schemas-upnp-org:service:WANPPPConnection:1" in body
               and "WANPPPConnection:1#AddPortMapping" in headers.get("SOAPAction", ""),
               headers.get("SOAPAction"))
    check_true("SOAPAction is quoted", (headers.get("SOAPAction") or "").startswith('"'))
    check_true("Content-Type carries the charset",
               "charset" in (headers.get("Content-Type") or "").lower())
    check_true("a finite lease was asked for, so a crash cannot leave it open",
               "<NewLeaseDuration>3600</NewLeaseDuration>" in body)
    check_true("the control URL matched the WAN service, not Layer3Forwarding",
               headers.get("Host", "").endswith(str(HTTP_PORT)))
else:
    fails.append("no AddPortMapping")

print("\n[cleanup on Ctrl-C]")
proc.send_signal(signal.SIGINT)
try:
    proc.wait(timeout=10)
except subprocess.TimeoutExpired:
    proc.kill(); fails.append("did not exit on Ctrl-C")
time.sleep(0.3)
dels = [c for c in calls if c[0] == "DeletePortMapping"]
check_true("the forward is withdrawn on exit", bool(dels),
           f"calls seen: {[c[0] for c in calls]}")
if dels:
    check_true("it deletes the external port that was actually granted",
               f"<NewExternalPort>{EXTERNAL_PORT}</NewExternalPort>" in dels[0][1])

stop.set(); httpd.shutdown()
print("\n" + ("ALL PASS" if not fails else f"{len(fails)} FAILED: {fails}"))
sys.exit(1 if fails else 0)
