# ecore-launcher TUI patch v1

Patch: `ecore-launcher-tui-v1`
Project: `minh23102011/e-core-launcher`
Goal: Add a keyboard-only full-control Ratatui dashboard without replacing the existing CLI/core safety model.

Baseline:
- Remote commit inspected: `85023ad682c257b6e2b8536cc00c5838fa8e25dd` (`phase6`)
- Expected local project: `~/Downloads/project`
- Source assumptions: Phase 6 CLI/core APIs match the inspected commit or retain the stable anchors validated by `apply.py`.

Files:
- added: `src/cli/tui/mod.rs`
- changed by `apply.py`: `Cargo.toml`, `src/cli/mod.rs`, `README.md`
- `Cargo.lock` is intentionally not bundled; Cargo should resolve/update it on the real project when the quality gate runs.

TUI behavior:
- command: `ecore-launcher tui`
- one-screen Applications + Details layout
- keyboard only
- add discovered apps
- enable/disable
- configure delay, nice, I/O class/priority, process-tree enforcement
- launch selected app through existing `run`
- launch selected app through existing `supervise`
- read-only doctor modal
- startup/autostart status + confirmed enable/disable/suppression actions
- remove registry entry with confirmation
- 5-second low-overhead core refresh; startup status refreshes explicitly
- small-terminal fallback below 72x20

Dependencies:
- adds `ratatui = 0.28.1` with only the `crossterm` backend feature
- Ratatui 0.28.1 declares Rust 1.74, so it is compatible with the project's declared Rust 1.75 floor at the direct-crate level.
- transitive dependency/MSRV behavior must still be verified by Cargo on the real repo.

Apply:
```fish
cd ~/Downloads/project
unzip -o ecore-launcher-tui-v1.zip -d ecore-launcher-tui-v1
python ecore-launcher-tui-v1/apply.py
```

Verified in this environment:
- `apply.py` parses with Python AST.
- `apply.py` was exercised against a synthetic ecore-launcher-shaped tree.
- applying twice is idempotent.
- stable-anchor patching for Cargo.toml, CLI wiring, README insertion, and payload installation succeeded.
- TUI source delimiter balance was statically checked.
- ZIP contents were checked for cache/build junk.

Not verified here:
- Rust compilation (this environment does not provide Cargo/rustfmt).
- full repository tests.
- runtime rendering in a real terminal.
- Rust 1.75 dependency resolution after Ratatui is added.

Quality gate on the real repo:
```fish
cd ~/Downloads/project
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo tree -d
```

If `cargo fmt --all -- --check` reports only formatting diffs, run:
```fish
cargo fmt --all
```
then rerun the full gate.

Runtime verification:
```fish
cd ~/Downloads/project
cargo run -- tui
```

Expected:
- alternate-screen TUI opens;
- left pane lists registry apps;
- right pane shows current policy/details;
- `?` shows keyboard help;
- `q` restores the terminal and exits.

Useful safe checks before mutating startup state:
- press `d` for Doctor;
- press `u` for Startup status;
- startup/autostart mutations require an explicit confirmation modal.

Git after the gate passes:
```fish
git status
git add Cargo.toml Cargo.lock README.md src/cli/mod.rs src/cli/tui/mod.rs
git commit -m "feat: add keyboard-only management TUI"
git push
```

Notes:
- TUI actions reuse the existing registry/startup/doctor APIs or spawn the current binary's existing `run`/`supervise` subcommands directly; no shell command string is used.
- The TUI does not add automatic process classification or weaken the existing opt-in/fail-closed E-core model.
- Run/supervise actions report that the launcher request process started; they do not falsely claim target application completion.
