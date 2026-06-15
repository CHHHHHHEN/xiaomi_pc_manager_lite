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

All commands run from project root (`D:\VSCodeProjects\xiaomi_pc_manager_lite`).

### Full verify before commit
```
cargo check && cargo clippy
```
