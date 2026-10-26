# tmux-watch

Two implementations of the same CLI: Python (`src/tmux_watch/cli.py`, the
original) and a Rust port (`rust/`, merged to master 2026-10-26). Keep them behaviourally
identical: hub names, `@tw-args`/`@tw-src` formats and layouts must match so
either can reconcile the other's hubs. Verify with `python3 parity/tw_parity.py`
(scratch tmux socket, never the default server). Details and the cutover plan:
`docs/devlog/claude_2026-10-26-rust-rewrite.md` (plain path).
