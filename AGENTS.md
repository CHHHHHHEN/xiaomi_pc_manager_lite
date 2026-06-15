# opencode agent instructions

## build/lint commands

### Build (pure Rust)
- `cargo build` — debug build
- `cargo build --release` — release build
- `cargo check` — type check only
- `cargo clippy` — lint

### Run
- `cargo run` — run from terminal
- The binary is at `target/debug/xiaomi-pc-manager-lite.exe`

### Full verify before commit
```
cargo check && cargo clippy
```
