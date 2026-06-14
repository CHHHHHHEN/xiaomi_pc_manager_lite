# opencode agent instructions

## build/lint commands

### Frontend (Svelte + Vite)
- `npm install` — install deps
- `npm run dev` — Vite dev server
- `npm run build` — Vite production build
- `npx svelte-check` — Svelte type/lint check

### Backend (Rust + Tauri)
- `npm run tauri dev` — run in dev mode (Vite + Tauri)
- `npm run tauri build` — production build
- `cargo build -p xiaomi-pc-manager-lite` — Rust only
- `cargo check -p xiaomi-pc-manager-lite` — Rust type check only
- `cargo clippy -p xiaomi-pc-manager-lite` — Rust lint

### Full verify before commit
```
cargo check -p xiaomi-pc-manager-lite && npx svelte-check && npm run build
```
