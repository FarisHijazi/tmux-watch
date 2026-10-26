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

Result: **40 passed, 0 failed**.

```
python3 parity/tw_parity.py   # OLD defaults to ~/.local/bin/tmux-watch
```

Startup: ~1 ms vs ~72 ms for `--help`.

## Cutover plan (needs approval, not done)

1. `cargo install --path rust --root ~/.local` on each machine (macOS builds natively).
2. Point the `tw` alias at it: it calls `tmux-watch`, so installing to
   `~/.local/bin/tmux-watch` replaces the uv tool's symlink. Run `uv tool uninstall tmux-watch` first.
3. Running hubs keep their Python pollers until rebuilt; formats are shared.
4. Rollback: `uv tool install --force --from git+https://github.com/FarisHijazi/tmux-watch.git tmux-watch`.
