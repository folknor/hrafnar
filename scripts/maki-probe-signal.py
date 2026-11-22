#!/usr/bin/env python3
"""Probe how `maki -p --output-format stream-json` answers a signal mid-turn.

Starts a long turn, waits for its `init` event plus a delay, sends the signal,
prints what maki printed and its exit status, then resumes the session once to
show whether it survived. Re-run after a maki upgrade: the maki adapter's
interrupt handling and reference/maki.md rest on the result.

Python rather than sh on purpose: a non-interactive shell starts a background
job with SIGINT ignored, maki keeps an inherited ignored signal ignored, and
the probe then shows a turn that shrugs SIGINT off. hrafnar's keeper starts
maki with default dispositions, as this does.

Every wait is bounded, so a maki that never prints `init` or ignores the
signal is reported (and killed) rather than hanging the probe.

usage: scripts/maki-probe-signal.py INT|TERM [delay-secs] [maki args...]
Costs real model turns on whatever provider maki is configured with.
"""

import json
import queue
import signal
import subprocess
import sys
import threading
import time
import uuid

INIT_WAIT_SECS = 60
EXIT_WAIT_SECS = 30
RESUME_WAIT_SECS = 120


def start_maki(args):
    child = subprocess.Popen(
        ["maki", "-p", *args, "--output-format", "stream-json"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        restore_signals=True,
    )
    lines = queue.Queue()

    def pump():
        for line in child.stdout:
            lines.put(line.rstrip("\n"))
        lines.put(None)

    threading.Thread(target=pump, daemon=True).start()
    return child, lines


def drain(lines, until):
    """Every line until EOF or the deadline; the bool says whether EOF came."""
    out = []
    while True:
        try:
            line = lines.get(timeout=max(0.0, until - time.monotonic()))
        except queue.Empty:
            return out, False
        if line is None:
            return out, True
        out.append(line)


def finish(child, lines, wait_secs):
    """Wait for exit and EOF within `wait_secs`, killing the child past it."""
    deadline = time.monotonic() + wait_secs
    out, eof = drain(lines, deadline)
    try:
        child.wait(timeout=max(0.0, deadline - time.monotonic()))
        timed_out = not eof
    except subprocess.TimeoutExpired:
        timed_out = True
    if timed_out:
        child.kill()
        child.wait()
    return out, child.stderr.read(), timed_out


def summarize(line):
    """An event as printed, its result text cut short; None for assistant events."""
    try:
        event = json.loads(line)
    except ValueError:
        return line
    if event.get("type") == "assistant":
        return None
    if isinstance(event.get("result"), str) and len(event["result"]) > 200:
        event["result"] = event["result"][:200] + "..."
    return json.dumps(event)


def main():
    if len(sys.argv) < 2 or sys.argv[1] not in ("INT", "TERM"):
        sys.exit(__doc__)
    sig = getattr(signal, "SIG" + sys.argv[1])
    delay = float(sys.argv[2]) if len(sys.argv) > 2 else 3.0
    extra = sys.argv[3:]
    version = subprocess.run(["maki", "--version"], capture_output=True, text=True)
    print(f"maki: {version.stdout.strip()}")
    sid = str(uuid.uuid4())
    print(f"session: {sid}")
    child, lines = start_maki(["--session-id", sid, *extra])
    child.stdin.write("Write the numbers from 1 to 3000, one per line, with no other text.")
    child.stdin.close()

    seen = []
    deadline = time.monotonic() + INIT_WAIT_SECS
    init = False
    while not init:
        try:
            line = lines.get(timeout=max(0.0, deadline - time.monotonic()))
        except queue.Empty:
            break
        if line is None:
            break
        seen.append(line)
        init = '"subtype":"init"' in line
    if not init:
        out, stderr, _ = finish(child, lines, 0)
        print(f"no init within {INIT_WAIT_SECS}s; exit {child.returncode}")
        for line in seen + out:
            print(summarize(line))
        print(stderr, end="")
        sys.exit(1)

    time.sleep(delay)
    sent = time.monotonic()
    child.send_signal(sig)
    out, stderr, timed_out = finish(child, lines, EXIT_WAIT_SECS)
    elapsed = time.monotonic() - sent
    if timed_out:
        print(f"signal: SIG{sys.argv[1]} after init + {delay}s; still running "
              f"{EXIT_WAIT_SECS}s later, killed")
    else:
        print(f"signal: SIG{sys.argv[1]} after init + {delay}s; exited {elapsed:.1f}s later")
    print(f"exit: {child.returncode}")
    print("--- stdout (assistant events elided)")
    for line in seen + out:
        shown = summarize(line)
        if shown is not None:
            print(shown)
    print("--- stderr")
    print(stderr, end="")

    print("--- resume")
    resume, resume_lines = start_maki(["--resume", sid, *extra])
    resume.stdin.write("Reply with the single word: resumed")
    resume.stdin.close()
    out, stderr, timed_out = finish(resume, resume_lines, RESUME_WAIT_SECS)
    print(f"resume exit: {resume.returncode}" + (" (killed, timed out)" if timed_out else ""))
    for line in out:
        if '"type":"result"' in line:
            print(summarize(line))
    print(stderr, end="")


main()
