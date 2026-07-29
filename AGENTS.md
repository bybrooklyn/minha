# Minha contributor instructions

- This repository is a Rust 2024 workspace and requires Rust 1.97 or newer.
- Keep model-facing schemas compact and versioned. New tools must justify their prompt cost.
- Do not add MCP, plugin marketplaces, or arbitrary dynamic tool loading.
- Judges are read-only. Reset-credit redemption is never available to agents.
- `AGENTS.md` wins same-scope conflicts with `CLAUDE.md`; symlinked copies load once.
- Run `cargo fmt --all -- --check`, `cargo test --workspace`, and `cargo clippy --workspace --all-targets -- -D warnings` before claiming completion.
- Preserve unrelated worktree changes and never push, merge, release, or redeem account credits without explicit user authority.
