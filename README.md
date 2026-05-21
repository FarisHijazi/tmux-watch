# tmux-watch

Tile matching tmux sessions — local **or remote** — into a single hub session that auto-refreshes as sessions come and go.

```
tmux-watch                                          # all local sessions under the current directory
tmux-watch ~/projects                               # explicit path
tmux-watch -d 2 ~/projects                          # …max 2 directory levels deep
tmux-watch ftower:~/projects                        # remote tmux server (over ssh)
tmux-watch ~/projects ftower:~/work hetzner-1:~/src # mixed local + multiple remotes
```

## Why

When ~10 tmux sessions are scattered across project directories — and across machines — switching between them via `tmux attach -t name` is friction. `tmux-watch` collects every matching session into one tiled hub, polls a few times per minute, and silently adds/kills panes as sessions appear or vanish.

Refresh is silent because by definition you are *not* in the hub when a new session is born — you are in the terminal where you created it. The hub reshuffles invisibly.

## Install

Requires [`uv`](https://github.com/astral-sh/uv) (or plain `pip`) and `tmux`. `uv` provisions Python automatically; no other Python is needed.

Four supported flows — pick whichever matches your setup:

### 1. Global CLI (recommended)

Installs `tmux-watch` into `~/.local/bin/` (uv's tool bin directory — make sure it's on your `PATH`), isolated in its own venv:

```bash
uv tool install git+https://github.com/FarisHijazi/tmux-watch.git

uv tool upgrade tmux-watch       # later, to update
uv tool uninstall tmux-watch     # to remove
```

`tmux-watch --help` should now work in any shell.

### 2. Inside an existing venv

Standard PEP 621 console-script entry point — any installer works:

```bash
uv pip install git+https://github.com/FarisHijazi/tmux-watch.git
# or:
pip install git+https://github.com/FarisHijazi/tmux-watch.git
```

`tmux-watch` lands in the active venv's `bin/`, available whenever that venv is active. Good for dev environments or pinning in a `requirements.txt`.

### 3. Editable / development install

```bash
git clone git@github.com:FarisHijazi/tmux-watch.git
cd tmux-watch
uv tool install --force --editable .    # global, reflects in-tree edits
# or, inside a venv:
uv pip install -e .
```

### 4. One-off run via `uvx` (no install)

```bash
uvx --from git+https://github.com/FarisHijazi/tmux-watch.git tmux-watch -n
```

**Caveat:** the background poller spawned by `tmux-watch` references the script path inside `uv`'s ephemeral cache. If `uv` cleans the cache, an already-running poller dies at its next tick. For long-lived hubs, prefer `uv tool install` (option 1).

### Short alias (optional)

The full command is intentionally verbose so it doesn't squat the namespace. For day-to-day use, add a shell alias:

```bash
echo "alias tw='tmux-watch'" >> ~/.bash_aliases   # or ~/.bashrc / ~/.zshrc
```

Then `tw ~/projects` etc. (Aliases only affect interactive shells; the binary itself stays `tmux-watch`.)

## CLI

```
tmux-watch [-d N | --max-depth N] [-n | --dry-run] [[host:]PATH ...]
tmux-watch purge-hubs
```

- **Paths** are positional and variadic — `du` / `find` / `tree` convention. **Default: `.`** (current directory). Pass multiple to span more roots.
- **`host:` prefix** is rsync/scp style: `ftower:~/projects`, `faris@buzastation:~/src`. No prefix = local.
- **`-d N`** caps how many directory components below `PATH` a session's cwd may be. **Default: unlimited** (du convention).
- **`-n` / `--dry-run`** prints the hub name and the matching sessions, then exits — no hub created, no poller spawned.
- **`purge-hubs`** kills all `hub/*` sessions on the tmux server and exits. Pollers self-terminate within one tick. Takes no arguments.

## How it works

1. `tmux-watch` resolves each `[host:]PATH` to an absolute path on its host (locally with `pwd -P`; remotely via `ssh host "cd PATH && pwd -P"`).
2. It computes a deterministic hub session name from the sorted, resolved args. **Same args ⇒ same hub.**
3. It lists local + remote tmux sessions whose first-pane cwd is under one of the paths (within `-d` depth) and tiles each one into a pane of the hub session.
4. It spawns a background poller via `tmux run-shell -b`. The poller is parented to the **tmux server** itself, so it survives closing your terminal, SSH session, or VSCode window — same survival guarantee as your tmux sessions. It dies only when the hub session is killed.
5. Every 3 seconds (default) the poller re-lists, diffs against the current panes, and reconciles: `split-window` for new sessions, `kill-pane` for vanished ones. Existing panes are untouched. If a remote host is unreachable that tick, its panes are left alone (no churn on transient failures).
6. Re-running `tmux-watch` with the same args triggers an immediate reconcile and exits (no second attach).

## Pane identity and labels

Each pane carries a `@tw-src` user-option storing `<host>\t<session>`, which is the diff key for reconcile. The pane title is purely cosmetic:

- Local pane: title is `<session>`
- Remote pane: title is `<host>:<session>`

Identity is bulletproof: inner programs (vim, ssh) overwriting the title can't break reconcile. Titles are visible because `pane-border-status` is set to `top` on the hub.

## Tunables

- `TW_POLL_INTERVAL` (seconds, default `3`) — poll cadence.

## Conventions

| Convention | Source |
|---|---|
| Variadic positional paths, default `.` | `du`, `find`, `tree`, `ls` |
| `[host:]PATH` syntax | `rsync`, `scp` |
| `-d N` / `--max-depth N`, default infinite | `du` |

## Requirements

- `tmux` 3.0+
- `uv` 0.4+ (or any installer that respects PEP 621 console scripts)
- `ssh` for remote hosts; configure your `~/.ssh/config` for hostnames / keys
- Key-based SSH auth to any remote hosts (`BatchMode=yes` is used for listing)

## Tips

- **SSH multiplexing.** If polling a remote host feels slow, add a `ControlMaster auto` block to `~/.ssh/config` — the poller will reuse a single connection per host.
- **Force a refresh.** Re-run `tmux-watch ...` with the same args, or kill and let auto-poll handle it.
- **Inspect a hub.** `tmux ls | grep ^hub/` shows all hubs; the slug after `__` is a hash of the args.

## License

MIT
