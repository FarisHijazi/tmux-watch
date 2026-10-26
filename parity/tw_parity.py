#!/usr/bin/env python3
"""Parity harness: Python tmux-watch (OLD) vs Rust tmux-watch (NEW).

Everything runs against a scratch tmux server (`tmux -L <socket> -f /dev/null`, a PATH
wrapper) — never the default server, never the user's hooks/plugins. A fake `ssh` on PATH runs the remote
command locally for host `fakebox` and fails for `deadbox`, so remote specs are
exercised without a real host.

OLD must be the Python implementation run by Python 3.12 (the released uv
tool's interpreter; argparse's --help layout differs between versions):

    uv venv -p 3.12 /tmp/tw-py && uv pip install --python /tmp/tw-py .
    OLD=/tmp/tw-py/bin/tmux-watch python3 parity/tw_parity.py
"""

import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

OLD = os.path.realpath(os.path.expanduser(os.environ.get("OLD", "/tmp/tw-py/bin/tmux-watch")))
if not Path(OLD).is_file() or Path(OLD).read_bytes()[:2] != b"#!":
    sys.exit(f"OLD={OLD} is not the Python tmux-watch script (see the docstring)")
NEW = os.path.realpath(os.environ.get("NEW", str(Path(__file__).resolve().parent.parent / "rust/target/release/tmux-watch")))
REAL_TMUX = shutil.which("tmux")
SOCKET = f"tw-parity-{os.getpid()}"
ROOT = Path(tempfile.mkdtemp(prefix="tw-parity-")).resolve()
BIN = ROOT / "bin"
WORK = ROOT / "work"
results = {"pass": 0, "fail": 0}


def setup():
    BIN.mkdir()
    (BIN / "tmux").write_text(f'#!/bin/sh\nexec {REAL_TMUX} -L {SOCKET} -f /dev/null "$@"\n')
    (BIN / "ssh").write_text(
        "#!/bin/sh\n"
        "# fake ssh: drop options, run the command locally for fakebox\n"
        "while [ $# -gt 0 ]; do case $1 in -o) shift 2;; -t) shift;; *) break;; esac; done\n"
        "host=$1; shift\n"
        'if [ "$host" != fakebox ]; then echo "ssh: Could not resolve hostname $host" >&2; exit 255; fi\n'
        'unset TMUX; exec sh -c "$*"\n'
    )
    for f in BIN.iterdir():
        f.chmod(0o755)
    for d in ["a/b/c", "a/x", "other"]:
        (WORK / d).mkdir(parents=True)
    os.environ["PATH"] = f"{BIN}:{os.environ['PATH']}"
    os.environ.pop("TMUX", None)
    os.environ["TW_POLL_INTERVAL"] = "1"
    os.environ["HOME_ORIG"] = os.environ.get("HOME", "")
    # sessions: (name, cwd)
    for name, cwd in [("alpha", "a"), ("beta", "a/b"), ("gamma", "a/b/c"), ("hq-worker", "a/x"),
                      ("other", "other"), ("hub/legacy", "a")]:
        tmux("new-session", "-d", "-s", name, "-c", str(WORK / cwd), "-x", "200", "-y", "50")


def tmux(*args):
    return subprocess.run(["tmux", *args], capture_output=True, text=True)


def run(binary, args, cwd=None):
    r = subprocess.run([binary, *args], capture_output=True, text=True, cwd=cwd or WORK, timeout=60)
    return r.returncode, r.stdout, r.stderr


def check(name, old, new):
    if old == new:
        results["pass"] += 1
        print(f"PASS {name}")
    else:
        results["fail"] += 1
        print(f"FAIL {name}\n  old={old!r}\n  new={new!r}")


def norm_err(text):
    # Compare the error/warning lines. A bad regex's reason comes from the regex
    # engine (Python re vs Rust regex), so only the message prefix must match.
    lines = [line for line in text.splitlines() if "error:" in line or "tmux-watch:" in line or line.startswith("  ")]
    return [l.split("':", 1)[0] if "bad --exclude regex" in l else l for l in lines
            if not (l.startswith("    ") or l.startswith("error: "))]


def cli_cases():
    a = str(WORK / "a")
    cases = [
        ("dry-run default cwd", ["-n"], WORK / "a"),
        ("dry-run explicit", ["-n", a], None),
        ("dry-run depth 1", ["-n", "-d", "1", a], None),
        ("dry-run depth=0 long", ["-n", "--max-depth=0", a], None),
        ("dry-run bundled -nd1", ["-nd1", a], None),
        ("dry-run exclude", ["-n", "-x", "^hq-", a], None),
        ("dry-run 2 excludes + prefix opt", ["--dry", "--exclude", "^hq-", "-xbeta", a], None),
        ("dry-run tilde+relative", ["-n", "a/b"], None),
        ("dry-run two specs", ["-n", str(WORK / "other"), a], None),
        ("dry-run remote fakebox", ["-n", f"fakebox:{a}", str(WORK / 'other')], None),
        ("dry-run no matches", ["-n", "-x", ".", a], None),
        ("bad regex", ["-n", "-x", "(", a], None),
        ("local: prefix rejected", ["-n", "local:/tmp"], None),
        ("not a directory", ["-n", str(WORK / "nope")], None),
        ("unreachable host resolve", ["-n", "deadbox:/tmp"], None),
        ("bad depth", ["-n", "-d", "abc", a], None),
        ("missing depth value", ["-n", "-d"], None),
        ("help", ["--help"], None),
        ("help short", ["-h"], None),
        ("unknown option", ["--frobnicate"], None),
        ("purge-hubs extra arg", ["purge-hubs", "x"], None),
        ("_poll missing hub", ["_poll"], None),
        ("_reconcile missing args on hub", ["_reconcile", "hub/legacy"], None),
        ("no matching sessions (real run)", ["-x", ".", a], None),
    ]
    for name, args, cwd in cases:
        o, n = run(OLD, args, cwd), run(NEW, args, cwd)
        check(name, (o[0], o[1], norm_err(o[2])), (n[0], n[1], norm_err(n[2])))


def hub_state(hub):
    # pane_title is skipped: the nested client's set-titles overwrites it at a racy moment
    panes = tmux("list-panes", "-t", hub, "-F", "#{@tw-src}|#{pane_start_command}").stdout
    layout = tmux("display-message", "-p", "-t", hub, "#{window_layout}").stdout.strip()
    return {
        "panes": sorted(panes.splitlines()),
        # pane ids differ between runs; geometry must not (checksum covers ids, drop it)
        "layout": re.sub(r"(\d+x\d+,\d+,\d+),\d+", r"\1,ID", layout.split(",", 1)[1]),
        "order": [l.split("|")[0] for l in panes.splitlines()],
        "args": tmux("show-options", "-v", "-t", hub, "@tw-args").stdout.strip(),
        "border": tmux("show-options", "-v", "-t", hub, "pane-border-status").stdout.strip(),
        "format": tmux("show-options", "-v", "-t", hub, "pane-border-format").stdout.strip(),
        "destroy": tmux("show-options", "-v", "-t", hub, "destroy-unattached").stdout.strip(),
    }


def wait(pred, timeout=8.0):
    end = time.time() + timeout
    while time.time() < end:
        if pred():
            return True
        time.sleep(0.2)
    return False


def build_hub(binary, args, runner):
    """Run a real (attaching) tw inside a scratch pane, so the exec'd attach has a tty."""
    cmd = " ".join(f"'{a}'" for a in [binary, *args])
    tmux("new-session", "-d", "-s", runner, "-x", "220", "-y", "60", f"env -u TMUX {cmd}; sleep 30")
    hub = run(binary, ["-n", *args])[1].splitlines()[0].split(": ", 1)[1]
    wait(lambda: tmux("show-options", "-v", "-t", hub, "destroy-unattached").stdout.strip() == "on")
    time.sleep(1.5)  # first poller tick (0.5s) settles
    return hub


def hub_cases():
    args = ["-x", "^hq-", str(WORK / "a"), f"fakebox:{WORK / 'other'}"]
    states = {}
    for label, binary in [("old", OLD), ("new", NEW)]:
        hub = build_hub(binary, args, f"runner-{label}")
        states[label] = (hub, hub_state(hub))
        tmux("kill-session", "-t", f"runner-{label}")  # last client gone -> hub destroyed
        wait(lambda: tmux("has-session", "-t", hub).returncode != 0)
        check(f"hub destroyed with last client ({label})", True, tmux("has-session", "-t", hub).returncode != 0)
    check("hub name", states["old"][0], states["new"][0])
    for key in states["old"][1]:
        check(f"hub state: {key}", states["old"][1][key], states["new"][1][key])

    # cross-compat + poller: hub built by A, new session appears, B's poller/reconcile handles it
    for builder, reconciler in [(OLD, NEW), (NEW, OLD)]:
        tag = "old->new" if builder == OLD else "new->old"
        hub = build_hub(builder, args, "runner-x")
        before = len(hub_state(hub)["panes"])
        tmux("new-session", "-d", "-s", "delta", "-c", str(WORK / "a/b"))
        added = wait(lambda: len(hub_state(hub)["panes"]) == before + 1)
        check(f"poller adds new session ({tag[:3]} poller)", True, added)
        tmux("kill-session", "-t", "delta")
        removed = wait(lambda: len(hub_state(hub)["panes"]) == before)
        check(f"poller removes gone session ({tag[:3]} poller)", True, removed)
        # explicit _reconcile by the OTHER implementation is a no-op on a settled hub
        rc = run(reconciler, ["_reconcile", hub])
        check(f"_reconcile foreign hub ({tag})", (0, f"tmux-watch: reconciled {hub}: +0 -0\n"), rc[:2])
        tmux("kill-session", "-t", "runner-x")
        wait(lambda: tmux("has-session", "-t", hub).returncode != 0)

    # purge-hubs: same output on the same server state
    out = {}
    tmux("kill-session", "-t", "hub/legacy")
    for label, binary in [("old", OLD), ("new", NEW)]:
        tmux("new-session", "-d", "-s", "hub/p1")
        tmux("new-session", "-d", "-s", "hub/p2")
        out[label] = run(binary, ["purge-hubs"])
    check("purge-hubs", out["old"], out["new"])
    check("purge-hubs (none left)", run(OLD, ["purge-hubs"]), run(NEW, ["purge-hubs"]))


def main():
    setup()
    try:
        cli_cases()
        hub_cases()
    finally:
        subprocess.run([REAL_TMUX, "-L", SOCKET, "kill-server"], capture_output=True)
        shutil.rmtree(ROOT, ignore_errors=True)
    print(f"\n{results['pass']} passed, {results['fail']} failed")
    return 1 if results["fail"] else 0


if __name__ == "__main__":
    sys.exit(main())
