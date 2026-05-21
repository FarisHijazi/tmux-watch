"""tmux-watch — tile matching tmux sessions into a single hub session.

Usage:
    tmux-watch [-d N | --max-depth N] [host:]PATH...

Re-running with the same args reconciles in place. A background poller
spawned via `tmux run-shell -b` keeps the hub in sync as sessions come
and go, parented to the tmux server so it survives terminal close.
"""

from __future__ import annotations

import argparse
import fcntl
import hashlib
import json
import os
import re
import shlex
import subprocess
import sys
import time
from contextlib import contextmanager
from dataclasses import dataclass
from pathlib import Path


PROG = "tmux-watch"
HUB_ARGS_KEY = "@tw-args"        # session option: JSON of (depth, specs)
PANE_SRC_KEY = "@tw-src"         # per-pane option: "<host>\t<session>"


# ---------- subprocess helpers ----------

def run(cmd: list[str]) -> subprocess.CompletedProcess:
    return subprocess.run(cmd, capture_output=True, text=True)


def tmux(*args: str) -> subprocess.CompletedProcess:
    return run(["tmux", *args])


def session_exists(name: str) -> bool:
    return tmux("has-session", "-t", name).returncode == 0


# ---------- specs ----------

@dataclass(frozen=True, order=True)
class Spec:
    host: str   # "" for local; ssh target otherwise
    path: str   # absolute path on `host`


def parse_spec(arg: str) -> tuple[str, str]:
    """rsync rule: split on first ':' only if it appears before any '/'.
    Local sources use a bare path; a `local:` prefix is rejected."""
    colon = arg.find(":")
    slash = arg.find("/")
    if colon != -1 and (slash == -1 or colon < slash):
        host, path = arg[:colon], arg[colon + 1:]
        if host == "local":
            sys.exit(
                f"{PROG}: 'local:' prefix is not a valid host — "
                f"drop it and use the bare path: {path}"
            )
        return host, path
    return "", arg


def resolve_spec(host: str, path: str) -> Spec:
    if not host:
        p = Path(path).expanduser()
        if not p.is_dir():
            sys.exit(f"{PROG}: not a directory: {path}")
        return Spec(host="", path=str(p.resolve()))

    r = run(
        ["ssh", "-o", "ConnectTimeout=5", "-o", "BatchMode=yes", host,
         f"cd {shlex.quote(path)} && pwd -P"]
    )
    if r.returncode != 0:
        sys.exit(f"{PROG}: {host}: cannot resolve {path}: "
                 f"{r.stderr.strip() or 'ssh failed'}")
    return Spec(host=host, path=r.stdout.strip())


def display_target(host: str, name_or_path: str) -> str:
    """`name` for local, `host:name` for remote."""
    return name_or_path if not host else f"{host}:{name_or_path}"


# ---------- session listing ----------

def _parse_panes_output(text: str) -> list[tuple[str, str]]:
    """tmux list-panes -F '#S\\t#{pane_current_path}' → dedupe by session."""
    seen: dict[str, str] = {}
    for line in text.splitlines():
        if "\t" not in line:
            continue
        s, p = line.split("\t", 1)
        seen.setdefault(s, p)
    return list(seen.items())


def list_local_sessions() -> list[tuple[str, str]]:
    r = tmux("list-panes", "-a", "-F", "#S\t#{pane_current_path}")
    if r.returncode != 0:
        return []
    return _parse_panes_output(r.stdout)


def list_remote_sessions(host: str) -> list[tuple[str, str]] | None:
    r = run(
        ["ssh", "-o", "ConnectTimeout=5", "-o", "BatchMode=yes", host,
         "tmux list-panes -a -F '#S\t#{pane_current_path}' 2>/dev/null"]
    )
    if r.returncode != 0:
        return None
    return _parse_panes_output(r.stdout)


def within(path: str, base: str, depth: int | None) -> bool:
    if path == base:
        return True
    if not path.startswith(base + "/"):
        return False
    if depth is None:
        return True
    rel = path[len(base) + 1:]
    return rel.count("/") + 1 <= depth


def is_hub_session(name: str) -> bool:
    return name == "hub" or name.startswith("hub/")


def list_pairs(specs: list[Spec], depth: int | None) -> tuple[list[tuple[str, str]], set[str]]:
    by_host: dict[str, list[Spec]] = {}
    for s in specs:
        by_host.setdefault(s.host, []).append(s)

    pairs: list[tuple[str, str]] = []
    seen: set[tuple[str, str]] = set()
    unreachable: set[str] = set()

    for host, host_specs in by_host.items():
        sessions = list_local_sessions() if not host else list_remote_sessions(host)
        if sessions is None:
            unreachable.add(host)
            continue
        for sess_name, sess_path in sessions:
            if is_hub_session(sess_name):
                continue
            if any(within(sess_path, sp.path, depth) for sp in host_specs):
                key = (host, sess_name)
                if key not in seen:
                    seen.add(key)
                    pairs.append(key)
    return pairs, unreachable


# ---------- hub naming ----------

def hub_name(depth: int | None, specs: list[Spec]) -> str:
    sorted_specs = sorted(specs)
    key = json.dumps(
        {"depth": depth, "specs": [(s.host, s.path) for s in sorted_specs]},
        sort_keys=True,
    )
    digest = hashlib.sha1(key.encode()).hexdigest()[:6]
    first = sorted_specs[0]
    base = Path(first.path).name or "root"
    label = f"{first.host}-{base}" if first.host else base
    label = re.sub(r"[^A-Za-z0-9]+", "-", label).strip("-").lower()[:30]
    return f"hub/{label}__{digest}"


# ---------- pane operations ----------

def attach_cmd(host: str, session: str) -> str:
    if not host:
        return f"TMUX= tmux attach -t {shlex.quote(session)}"
    return f"ssh -t {shlex.quote(host)} tmux attach -t {shlex.quote(session)}"


def split_window(hub: str, command: str) -> str:
    r = tmux("split-window", "-t", hub, "-P", "-F", "#{pane_id}", command)
    return r.stdout.strip()


def kill_pane(pid: str) -> None:
    tmux("kill-pane", "-t", pid)


def set_pane_title(pid: str, title: str) -> None:
    tmux("select-pane", "-t", pid, "-T", title)


def set_pane_src(pid: str, host: str, session: str) -> None:
    """Bind identity to the pane via a user-option. Title is display-only."""
    tmux("set-option", "-p", "-t", pid, PANE_SRC_KEY, f"{host}\t{session}")


def install_pane(pid: str, host: str, sess: str) -> None:
    set_pane_src(pid, host, sess)
    set_pane_title(pid, display_target(host, sess))


def list_panes_with_identity(hub: str) -> dict[tuple[str, str], str]:
    """{(host, sess): pane_id} for panes that have @tw-src. Auto-migrates
    legacy panes by parsing their title once."""
    r = tmux(
        "list-panes", "-t", hub, "-F",
        "#{pane_id}\t#{pane_title}\t#{@tw-src}",
    )
    if r.returncode != 0:
        return {}
    out: dict[tuple[str, str], str] = {}
    for line in r.stdout.splitlines():
        parts = line.split("\t", 2)
        if len(parts) < 2:
            continue
        pid = parts[0]
        title = parts[1] if len(parts) > 1 else ""
        src = parts[2] if len(parts) > 2 else ""
        if src and "\t" in src:
            host, sess = src.split("\t", 1)
        else:
            # legacy: recover identity from title via rsync rule
            host, sess = _recover_identity_from_title(title)
            if not sess:
                continue
            set_pane_src(pid, host, sess)
        out[(host, sess)] = pid
    return out


def _recover_identity_from_title(title: str) -> tuple[str, str]:
    if not title:
        return "", ""
    if ":" in title:
        host, sess = title.split(":", 1)
        return host, sess
    return "", title


# ---------- hub state ----------

def store_hub_args(hub: str, depth: int | None, specs: list[Spec]) -> None:
    payload = json.dumps({
        "depth": depth,
        "specs": [{"host": s.host, "path": s.path} for s in specs],
    })
    tmux("set-option", "-t", hub, HUB_ARGS_KEY, payload)


def read_hub_args(hub: str) -> tuple[int | None, list[Spec]]:
    r = tmux("show-options", "-v", "-t", hub, HUB_ARGS_KEY)
    if r.returncode != 0 or not r.stdout.strip():
        sys.exit(f"{PROG}: no {HUB_ARGS_KEY} on session {hub}")
    data = json.loads(r.stdout.strip())
    specs = [Spec(host=s["host"], path=s["path"]) for s in data["specs"]]
    return data["depth"], specs


# ---------- locking ----------

@contextmanager
def file_lock(hub: str):
    path = f"/tmp/tmux-watch-{hub.replace('/', '_')}.lock"
    with open(path, "w") as f:
        fcntl.flock(f.fileno(), fcntl.LOCK_EX)
        try:
            yield
        finally:
            fcntl.flock(f.fileno(), fcntl.LOCK_UN)


# ---------- reconcile ----------

def reconcile(hub: str) -> tuple[int, int]:
    """Diff src vs panes; add missing, kill gone (skip unreachable hosts).
    Identity is (host, session) via @tw-src — title chars don't matter."""
    depth, specs = read_hub_args(hub)
    pairs, unreachable = list_pairs(specs, depth)
    src: set[tuple[str, str]] = set(pairs)
    panes = list_panes_with_identity(hub)

    added = 0
    for host, sess in src - panes.keys():
        pid = split_window(hub, attach_cmd(host, sess))
        if pid:
            install_pane(pid, host, sess)
            added += 1

    removed = 0
    for (host, sess), pid in list(panes.items()):
        if (host, sess) in src:
            continue
        if host in unreachable:
            continue
        kill_pane(pid)
        removed += 1

    if added or removed:
        tmux("select-layout", "-t", hub, "tiled")
    return added, removed


# ---------- hub creation ----------

def spawn_poller(hub: str) -> None:
    script = os.path.realpath(sys.argv[0])
    tmux("run-shell", "-b", f"exec {shlex.quote(script)} _poll {shlex.quote(hub)}")


def create_hub(hub: str, depth: int | None, specs: list[Spec],
               pairs: list[tuple[str, str]]) -> None:
    first_host, first_sess = pairs[0]

    r = tmux("new-session", "-d", "-s", hub, attach_cmd(first_host, first_sess))
    if r.returncode != 0:
        sys.exit(f"{PROG}: failed to create hub: {r.stderr.strip()}")

    panes = tmux("list-panes", "-t", hub, "-F", "#{pane_id}")
    first_pid = panes.stdout.strip().splitlines()[0]
    install_pane(first_pid, first_host, first_sess)

    for host, sess in pairs[1:]:
        pid = split_window(hub, attach_cmd(host, sess))
        if pid:
            install_pane(pid, host, sess)

    tmux("set-option", "-t", hub, "pane-border-status", "top")
    tmux("set-option", "-t", hub, "pane-border-format", " #{pane_title} ")
    tmux("select-layout", "-t", hub, "tiled")
    store_hub_args(hub, depth, specs)
    spawn_poller(hub)


# ---------- legacy hub detection ----------

def legacy_hubs() -> list[str]:
    """Hub sessions without @tw-args — pre-Python or hand-created."""
    r = tmux("list-sessions", "-F", "#S")
    if r.returncode != 0:
        return []
    out: list[str] = []
    for s in r.stdout.splitlines():
        if not is_hub_session(s):
            continue
        a = tmux("show-options", "-v", "-t", s, HUB_ARGS_KEY)
        if a.returncode != 0 or not a.stdout.strip():
            out.append(s)
    return out


def warn_legacy_hubs() -> None:
    stale = legacy_hubs()
    if not stale:
        return
    print(f"{PROG}: warning: {len(stale)} legacy hub(s) without poller — "
          f"run `{PROG} purge-hubs` to clean them up:", file=sys.stderr)
    for h in stale:
        print(f"  {h}", file=sys.stderr)


# ---------- entry points ----------

def cmd_main(depth: int | None, paths: list[str], dry_run: bool = False) -> int:
    if not paths:
        paths = ["."]

    warn_legacy_hubs()
    specs = [resolve_spec(*parse_spec(p)) for p in paths]
    hub = hub_name(depth, specs)

    if dry_run:
        pairs, unreachable = list_pairs(specs, depth)
        if unreachable:
            print(f"{PROG}: warning: unreachable hosts skipped: "
                  f"{', '.join(sorted(unreachable))}", file=sys.stderr)
        print(f"hub: {hub}")
        print(f"depth: {'unlimited' if depth is None else depth}")
        print("specs:")
        for s in specs:
            print(f"  {display_target(s.host, s.path)}")
        print(f"would attach ({len(pairs)} session{'s' if len(pairs) != 1 else ''}):")
        for host, sess in pairs:
            print(f"  {display_target(host, sess)}")
        return 0

    if not session_exists(hub):
        pairs, unreachable = list_pairs(specs, depth)
        if not pairs:
            sys.exit(f"{PROG}: no matching tmux sessions")
        if unreachable:
            print(f"{PROG}: warning: unreachable hosts skipped: "
                  f"{', '.join(sorted(unreachable))}", file=sys.stderr)
        create_hub(hub, depth, specs, pairs)
        os.execvp("tmux", ["tmux", "attach", "-t", hub])

    with file_lock(hub):
        added, removed = reconcile(hub)
    n = len(list_panes_with_identity(hub))
    print(f"{PROG}: reconciled {hub}: +{added} -{removed} ({n} panes)")
    return 0


def cmd_reconcile(hub: str) -> int:
    with file_lock(hub):
        added, removed = reconcile(hub)
    print(f"{PROG}: reconciled {hub}: +{added} -{removed}")
    return 0


def cmd_purge_hubs() -> int:
    r = tmux("list-sessions", "-F", "#S")
    if r.returncode != 0:
        print(f"{PROG}: no tmux server running")
        return 0
    hubs = [s for s in r.stdout.splitlines() if is_hub_session(s)]
    if not hubs:
        print(f"{PROG}: no hub sessions to purge")
        return 0
    for h in hubs:
        tmux("kill-session", "-t", h)
        print(f"killed {h}")
    print(f"{PROG}: purged {len(hubs)} hub session{'s' if len(hubs) != 1 else ''} "
          f"(pollers self-exit within one tick)")
    return 0


def cmd_poll(hub: str) -> int:
    interval = float(os.environ.get("TW_POLL_INTERVAL", "3"))
    while session_exists(hub):
        time.sleep(interval)
        try:
            with file_lock(hub):
                reconcile(hub)
        except SystemExit:
            return 0
        except Exception:
            pass
    return 0


def main() -> int:
    argv = sys.argv[1:]
    if argv and argv[0] in ("_poll", "_reconcile"):
        if len(argv) < 2:
            sys.exit(f"{PROG}: {argv[0]} requires HUB argument")
        return cmd_poll(argv[1]) if argv[0] == "_poll" else cmd_reconcile(argv[1])
    if argv and argv[0] == "purge-hubs":
        if len(argv) > 1:
            sys.exit(f"{PROG}: purge-hubs takes no arguments")
        return cmd_purge_hubs()

    p = argparse.ArgumentParser(
        prog=PROG,
        description="Tile matching tmux sessions into a single hub session.",
        epilog="subcommands:\n  purge-hubs    kill all hub/* sessions and exit",
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    p.add_argument("-d", "--max-depth", dest="depth", type=int, default=None,
                   metavar="N", help="max directory depth below each PATH")
    p.add_argument("-n", "--dry-run", action="store_true",
                   help="list sessions that would be attached, then exit")
    p.add_argument("paths", nargs="*", metavar="[host:]PATH",
                   help="directories to watch (default: .); "
                        "host: prefix for remote (rsync style)")
    args = p.parse_args(argv)
    return cmd_main(args.depth, args.paths, dry_run=args.dry_run)


if __name__ == "__main__":
    sys.exit(main())
