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

Requires [`uv`](https://github.com/astral-sh/uv) and `tmux`. (`uv` provisions Python automatically; no other Python is needed.)

```bash
git clone git@github.com:FarisHijazi/tmux-watch.git ~/.tmux-watch
ln -s ~/.tmux-watch/tw ~/.local/bin/tw     # or any directory in your PATH
```

That's it. `tw --help` should work.

## CLI

```
tw [-d N | --max-depth N] [host:]PATH...
```

- **Paths** are required and variadic — `du` / `find` / `tree` convention.
- **`host:` prefix** is rsync/scp style: `ftower:~/projects`, `faris@buzastation:~/src`. No prefix = local.
- **`-d N`** caps how many directory components below `PATH` a session's cwd may be. Default = unlimited.

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
