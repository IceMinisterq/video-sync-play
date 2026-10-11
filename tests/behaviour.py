"""Behaviour tests for the video-sync-play room state machine.

Starts the binary on a spare port, drives it with real WebSocket peers, and
checks the rules that matter: the room runs only when nobody is paused, a
peer that vanishes must not leave the other racing ahead, the clock never
runs past the last frame, and the room code is required.

    python3 tests/behaviour.py [path-to-binary]

Needs nothing installed - stdlib only.
"""
import os, subprocess, sys, threading, time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from ws import WS, summary

PORT, CODE = 8911, "SUITEXYZ"
BINARY = sys.argv[1] if len(sys.argv) > 1 else "target/release/video-sync-play"
FILE = {"name": "test.mp4", "size": 5388318, "duration": 120.0}
fails = []


def join(name, code=CODE, file=FILE):
    w = WS(port=PORT)
    w.send({"t": "hello", "code": code, "name": name, "file": file})
    return w


def keepalive(peers, stop):
    """Real clients ping every 2s; without this the watchdog gates the room."""
    while not stop.is_set():
        for p in peers:
            if p.alive:
                try: p.ping()
                except Exception: pass
        time.sleep(1.0)


def check(label, got, want):
    ok = got == want
    print(f"  {'PASS' if ok else 'FAIL'}  {label}: got {got!r}, want {want!r}")
    if not ok: fails.append(label)


def case(n): print(f"\n[{n}]")


if not os.path.exists(BINARY):
    sys.exit(f"no binary at {BINARY} - run: cargo build --release")

server = subprocess.Popen(
    [BINARY, "--port", str(PORT), "--code", CODE, "--no-open"],
    stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
time.sleep(1.5)
try:


    # ---------------------------------------------------------------- 1. gating
    case("gate: room runs only when nobody is paused")
    a, b = join("alice"), join("bob")
    stop = threading.Event()
    threading.Thread(target=keepalive, args=([a, b], stop), daemon=True).start()
    time.sleep(0.4)
    check("both fresh -> held", a.last_state()['playing'], False)
    a.send({"t": "resume", "position": 0}); time.sleep(0.4)
    check("one wants play -> still held", a.last_state()['playing'], False)
    b.send({"t": "resume", "position": 0})
    a.wait_state(lambda s: s['playing'], 2)
    check("both want play -> running", a.last_state()['playing'], True)

    # ------------------------------------------------------- 2. either can pause
    case("pause: either peer stops the room")
    time.sleep(1.5)
    b.send({"t": "pause", "position": 1.5})
    a.wait_state(lambda s: not s['playing'], 2)
    st = a.last_state()
    check("bob pauses -> held", st['playing'], False)
    check("froze near bob's playhead", 1.0 < st['position'] < 2.5, True)
    a.send({"t": "resume", "position": st['position']}); time.sleep(0.4)
    check("only alice resumes -> still held", a.last_state()['playing'], False)
    b.send({"t": "resume", "position": st['position']})
    a.wait_state(lambda s: s['playing'], 2)
    check("bob resumes too -> running", a.last_state()['playing'], True)

    # ----------------------------------------------------------------- 3. seek
    case("seek: moves the whole room, with a buffering grace period")
    b.send({"t": "seek", "position": 60.0})
    a.wait_state(lambda s: abs(s['position'] - 60.0) < 0.01, 2)
    st = a.last_state()
    check("room jumped to 60s", round(st['position'], 2), 60.0)
    check("start deferred for buffering", st['anchor'] - st['now'] > 0, True)

    # ---------------------------------------------------------------- 4. stall
    case("stall: a buffering peer holds everyone")
    b.send({"t": "stall", "stalled": True})
    a.wait_state(lambda s: not s['playing'], 2)
    check("bob buffering -> held", a.last_state()['playing'], False)
    b.send({"t": "stall", "stalled": False})
    a.wait_state(lambda s: s['playing'], 2)
    check("bob recovered -> running", a.last_state()['playing'], True)

    # ----------------------------------------------------------- 5. disconnect
    case("disconnect: a vanished peer must not leave the other racing ahead")
    time.sleep(1.0)
    b.close()
    a.wait_state(lambda s: not s['playing'], 3)
    st = a.last_state()
    check("room froze", st['playing'], False)
    check("bob removed", [p['name'] for p in st['peers']], ['alice'])
    check("alice force-paused (must re-consent)", st['peers'][0]['paused'], True)
    pos = st['position']
    time.sleep(2.0)
    check("playhead did not advance", round(a.last_state()['position'], 2), round(pos, 2))
    check("lone survivor may continue on purpose",
          (a.send({"t": "resume", "position": pos}),
           a.wait_state(lambda s: s['playing'], 2)['playing'])[1], True)
    stop.set(); a.close()

    # ------------------------------------------------------ 6. watchdog / auth
    case("watchdog: a silent peer gates the room")
    c, d = join("carol"), join("dave")
    time.sleep(0.3)
    c.send({"t": "resume", "position": 0}); d.send({"t": "resume", "position": 0})
    c.wait_state(lambda s: s['playing'], 2)
    check("running", c.last_state()['playing'], True)
    # Neither pings now: both go silent after ~6s.
    c.wait_state(lambda s: not s['playing'], 9)
    st = c.last_state()
    check("silence -> held", st['playing'], False)
    check("marked not-responding", any(p['silent'] for p in st['peers']), True)
    c.close(); d.close()

    case("end of film: the clock must not run past the last frame")
    e, f = join("eve"), join("frank")
    stop2 = threading.Event()
    threading.Thread(target=keepalive, args=([e, f], stop2), daemon=True).start()
    time.sleep(0.3)
    e.send({"t": "seek", "position": 500.0}); time.sleep(0.4)
    check("seek beyond the end is clamped", e.last_state()['position'], 120.0)
    e.send({"t": "seek", "position": 119.0})
    e.send({"t": "resume", "position": 119.0}); f.send({"t": "resume", "position": 119.0})
    e.wait_state(lambda s: s['playing'], 2)
    time.sleep(3.0)
    f.send({"t": "pause", "position": 120.0}); time.sleep(0.4)
    check("playhead stopped at the duration", e.last_state()['position'] <= 120.0, True)
    stop2.set(); e.close(); f.close()

    case("mismatch: a different file is flagged, not silently tolerated")
    g = join("gina")
    h = join("hank", file={"name": "other.mkv", "size": 999, "duration": 93.0})
    i = join("ivan")
    time.sleep(0.5)
    st = g.last_state()
    flagged = sorted(p['name'] for p in st['peers'] if p['mismatch'])
    check("odd one out flagged", flagged, ['hank'])
    g.close(); h.close(); i.close()

    case("auth: the room code is required")
    bad = WS(port=PORT)
    bad.send({"t": "hello", "code": "WRONGCDE", "name": "mallory", "file": FILE})
    time.sleep(0.5)
    errs = [m for m in bad.inbox if m.get('t') == 'error']
    check("rejected with an error", bool(errs), True)
    check("no room state leaked", bad.last_state(), None)
    bad.close()

finally:
    server.terminate()
    server.wait(timeout=5)

print("\n" + ("ALL PASS" if not fails else f"{len(fails)} FAILED: {fails}"))
sys.exit(1 if fails else 0)
