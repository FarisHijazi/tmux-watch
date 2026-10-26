# Rust rewrite of tmux-watch (branch `rust-rewrite`)

Crate: `rust/` (binary `tmux-watch`, deps: regex, serde_json, sha1_smol). A
line-for-line port of `src/tmux_watch/cli.py` 0.3.0: same flags (argparse
surface incl. `--long=value`, unique-prefix long options, `-nd2` bundling),
same hub names (identical SHA-1 of the same `json.dumps` key), same
`@tw-args` / `@tw-src` formats, same host-block layout strings, same lock files
(`/tmp/tmux-watch-<hub>.lock`, `flock`), same messages and exit codes.

Old and new interoperate: a hub built by one is reconciled correctly by the
other's `_reconcile` / poller.

Only known difference: a bad `--exclude` regex prints the Rust `regex` crate's
reason after the shared `bad --exclude regex '<pat>':` prefix, and that crate
has no look-around/backreferences (Python `re` has).

## Parity test

`parity/tw_parity.py` runs the installed Python `tmux-watch` and the Rust
build against one scratch tmux server (`tmux -L <socket> -f /dev/null` via a
PATH wrapper) with a fake `ssh` (host `fakebox` runs locally, `deadbox` fails):
22 CLI cases (dry-run output, errors, exit codes), a real attaching hub per
implementation (hub name, pane identities, start commands, layout geometry,
`@tw-args`, border options, `destroy-unattached`), destroy-with-last-client,
cross-implementation poller add/remove + `_reconcile`, and `purge-hubs`.

Result: **40 passed, 0 failed** (42/42 after the `--help` cases were added at cutover).

```
OLD=/tmp/tw-py/bin/tmux-watch python3 parity/tw_parity.py   # OLD: Python under 3.12, see the docstring
```

Startup: ~1 ms vs ~72 ms for `--help`.

## Cutover (done 2026-10-26)

Merged to master (fast-forward, parity re-run after merge). Then a follow-up
fix: `--help` was not byte-identical (a `\`-continued string ate the option
lines' indent, and argparse 3.12 prints `-d N, --max-depth N` and wraps the
`-x` line). Parity had no `--help` case; it now has two (42/42), and it
refuses an OLD that is not the Python script, since after cutover
`~/.local/bin/tmux-watch` is the Rust binary. argparse's layout varies by
Python version, so OLD must run under 3.12 (the uv tool's interpreter).

Every machine had `tmux-watch v0.3.0` as a uv tool from
`git+https://github.com/FarisHijazi/tmux-watch.git` (unpinned). Per machine:
`uv tool uninstall tmux-watch`, then either
`cargo install --git https://github.com/FarisHijazi/tmux-watch.git --root ~/.local tmux-watch`
(macOS arm64, native build) or a copy of a Debian 13 x86_64 build (glibc 2.41
everywhere). Smoke test per machine against a private server
(`TMUX_TMPDIR=$(mktemp -d)`): `--help`, `-n` dry-run, a real hub under
`script` (2 tiles, `@tw-src` set), `purge-hubs`.

Already-running hubs were left alone: a Python poller is one long-lived
process with no lazy imports, so deleting its venv does not stop it; it runs
until its hub is rebuilt.

**Rollback** (macOS and Debian alike; `--force` replaces the Rust binary,
verified in a scratch `UV_TOOL_DIR`):

```
uv tool install --force --python 3.12 --from git+https://github.com/FarisHijazi/tmux-watch.git@f4a716662703873fdb707daaa3dc5244a6b4fc9d tmux-watch
```
