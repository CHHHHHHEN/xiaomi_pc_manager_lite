# opencode agent instructions

## build/lint commands

### Frontend (Svelte + Vite)
- `pnpm install` — install deps
- `pnpm run dev` — Vite dev server
- `pnpm run build` — Vite production build
- `pnpx svelte-check` — Svelte type/lint check

### Backend (Rust + Tauri)
- `pnpm run tauri dev` — run in dev mode (Vite + Tauri)
- `pnpm run tauri build` — production build
- `cargo build -p xiaomi-pc-manager-lite` — Rust only
- `cargo check -p xiaomi-pc-manager-lite` — Rust type check only
- `cargo clippy -p xiaomi-pc-manager-lite` — Rust lint

### Full verify before commit
```
cargo check -p xiaomi-pc-manager-lite && pnpx svelte-check && pnpm run build
```
