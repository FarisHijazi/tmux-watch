# tmux-watch

Tile matching tmux sessions — local **or remote** — into a single hub session that auto-refreshes as sessions come and go.

```
tw ~/projects                                  # all local sessions under ~/projects
tw -d 2 ~/projects                             # …max 2 directory levels deep
tw ftower:~/projects                           # remote tmux server (over ssh)
tw ~/projects ftower:~/work hetzner-1:~/src    # mixed local + multiple remotes
```

## Why

When ~10 tmux sessions are scattered across project directories — and across machines — switching between them via `tmux attach -t name` is friction. `tw` collects every matching session into one tiled hub, polls a few times per minute, and silently adds/kills panes as sessions appear or vanish.

Refresh is silent because by definition you are *not* in the hub when a new session is born — you are in the terminal where you created it. The hub reshuffles invisibly.

## Install

Requires [`uv`](https://github.com/astral-sh/uv) (or plain `pip`) and `tmux`. `uv` provisions Python automatically; no other Python is needed.

Three supported flows — pick whichever matches your setup:

### 1. Global CLI (recommended)

Installs `tw` into `~/.local/bin/tw` (uv's tool bin directory — make sure it's on your `PATH`), isolated in its own venv:

```bash
uv tool install git+https://github.com/FarisHijazi/tmux-watch.git

uv tool upgrade tmux-watch       # later, to update
uv tool uninstall tmux-watch     # to remove
```

`tw --help` should now work in any shell.

### 2. Inside an existing venv

Standard PEP 621 console-script entry point — any installer works:

```bash
uv pip install git+https://github.com/FarisHijazi/tmux-watch.git
# or:
pip install git+https://github.com/FarisHijazi/tmux-watch.git
```

`tw` lands in the active venv's `bin/`, available whenever that venv is active. Good for dev environments or pinning in a `requirements.txt`.

### 3. Editable / development install

```bash
git clone git@github.com:FarisHijazi/tmux-watch.git
cd tmux-watch
uv tool install --force --editable .    # global, reflects in-tree edits
# or, inside a venv:
uv pip install -e .
```

### 4. One-off run via `uvx` (no install)

For a quick try without committing to an install:

```bash
uvx --from git+https://github.com/FarisHijazi/tmux-watch.git tw -n ~
```

`--from` is required because the package is `tmux-watch` but the command is `tw`.

**Caveat:** the background poller spawned by `tw` references the script path inside `uv`'s ephemeral cache. That path is stable across runs while the cache lives, but if `uv` cleans the cache (`uv cache clean`) or the cached env is evicted, an *already-running* poller will die at its next tick. For long-lived hubs, prefer `uv tool install` (option 1).

## CLI

```
tw [-d N | --max-depth N] [-n | --dry-run] [host:]PATH...
tw purge-hubs
```

- **Paths** are required and variadic — `du` / `find` / `tree` convention.
- **`host:` prefix** is rsync/scp style: `ftower:~/projects`, `faris@buzastation:~/src`. No prefix = local.
- **`-d N`** caps how many directory components below `PATH` a session's cwd may be. Default = unlimited.
- **`-n` / `--dry-run`** prints the hub name and the matching sessions, then exits — no hub created, no poller spawned. Useful for sanity-checking what a given invocation would tile.
- **`purge-hubs`** kills all `hub/*` sessions on the tmux server and exits. Pollers self-terminate within one tick. Takes no arguments.

## How it works

1. `tw` resolves each `[host:]PATH` to an absolute path on its host (local: `pwd -P`; remote: `ssh host "cd PATH && pwd -P"`).
2. It computes a deterministic hub session name from the sorted, resolved args. **Same args ⇒ same hub.**
3. It lists local + remote tmux sessions whose first-pane cwd is under one of the paths (within `-d` depth) and tiles each one into a pane of the hub session.
4. It spawns a background poller via `tmux run-shell -b`. The poller is parented to the **tmux server** itself, so it survives closing your terminal, SSH session, or VSCode window — same survival guarantee as your tmux sessions. It dies only when the hub session is killed.
5. Every 3 seconds the poller re-lists, diffs against the current panes, and reconciles: `split-window` for new sessions, `kill-pane` for vanished ones. Existing panes are untouched. If a remote host is unreachable that tick, its panes are left alone (no churn on transient failures).
6. Re-running `tw` with the same args triggers an immediate reconcile and exits (no second attach).

## Pane labels

Every pane is titled `host:session` (visible in `pane-border-status top`), and the title is re-asserted each tick so escape sequences from inner programs (vim, ssh) can't permanently overwrite it.

## Tunables

- `TW_POLL_INTERVAL` (seconds, default `3`) — poll cadence.

## Conventions

| Convention | Source |
|---|---|
| Variadic positional paths | `du`, `find`, `tree`, `ls` |
| `[host:]PATH` syntax | `rsync`, `scp` |
| `-d N` / `--max-depth N` | `du` |

## Requirements

- `tmux` 3.0+
- `uv` 0.4+
- `ssh` (for remote hosts; configure your `~/.ssh/config` for hostnames / keys)
- Key-based SSH auth to any remote hosts (`BatchMode=yes` is used for listing)

## Tips

- **SSH multiplexing.** If polling a remote host feels slow, add a `ControlMaster auto` block to `~/.ssh/config` — the poller will reuse a single connection per host.
- **Force a refresh.** Re-run `tw ...` with the same args, or kill and let auto-poll handle it.
- **Inspect a hub.** `tmux ls | grep ^hub/` shows all hubs; the slug after `__` is a hash of the args.

## License

MIT
