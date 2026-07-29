# Contributing

Minha is intentionally small at its model boundary and explicit about what is local infrastructure versus provider behavior. Contributions should preserve those boundaries.

## Scope and authority

Read the repository-root [`AGENTS.md`](../AGENTS.md) before changing files. At runtime, `AGENTS.md` wins same-scope conflicts with `CLAUDE.md`; compatible `.agents/` and `.claude/` files are supported, not a second authority system. Symlinked aliases are canonicalized and loaded once.

Do not add MCP, a plugin marketplace, arbitrary dynamic tool loading, or a hidden credit-redemption path. New model-facing tools must be justified against prompt cost, added to the fixed schema deliberately, role-filtered, bounded, and tested. A skill may improve instructions or workflow guidance but must not silently enlarge the tool catalog.

Preserve unrelated worktree changes. Minha does not auto-commit, merge, push, release, or discard user changes. Do not edit bundled books, `minha.toml.example`, source, or other owned surfaces when a task is scoped to documentation.

## Documentation rules

Documentation must distinguish:

- implemented behavior from configuration hooks and optional paths;
- configured model names from live provider entitlement;
- local tests from OAuth/provider, terminal, platform, accessibility, and human qualification;
- provider prompt caching from Minha's local result cache;
- task/lease/recovery state from model-generated prose;
- manifest signature metadata from cryptographic verification.

When adding a command, link to its implementation-facing explanation and document its failure/recovery behavior. Avoid promising a fallback that the runtime does not execute. Keep examples copyable and use `--json`/`--jsonl` when output shape matters.

Check relative links from the file being edited. Root links such as [`README.md`](../README.md) and [`SECURITY.md`](../SECURITY.md) resolve differently from links in `README.md`.

## Code and schema changes

Keep model-facing JSON schemas compact, versioned where persisted, and bounded by byte/count/timeout limits. Keep secret-bearing types' `Debug` output redacted. New persistent state needs a migration, replay behavior, failure semantics, and tests. New coordination messages need schema/version/recipient/size validation. New leases need expiry and generation fencing.

Provider changes must preserve exact model preflight and bounded redacted diagnostics. Auth changes must preserve atomic private writes, refresh-token handling, and explicit account identifiers. Cache changes must state whether a result is exact, TTL-bound, or never-cacheable and must reject secret-like inputs.

## Required checks

Before claiming completion, run from the workspace root:

```sh
cargo fmt --all -- --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo doc --workspace --no-deps
```

When available, also run `cargo audit`, `cargo deny check`, and `actionlint`. CI runs the dependency checks and treats rustdoc warnings as failures. Release changes additionally require the checklist in [Releasing Minha](RELEASING.md).

For runtime-facing changes, also exercise the relevant local boundary where available:

```sh
cargo run -p minha -- doctor
cargo run -p minha -- models --json
cargo run -p minha -- sessions --json
```

Do not claim that these commands performed interactive login, confirmed model entitlement, validated a remote provider, or qualified terminal rendering. Those require the evidence described in [Operations and qualification](OPERATIONS.md).

## Review checklist

- Is the change inside the requested ownership boundary?
- Are unrelated dirty files preserved?
- Does the model-facing surface remain closed and minimal?
- Does a new tool save more prompt/tool-loop cost than its schema adds, and is its output bounded?
- Are read-only roles actually denied mutation rather than merely told not to mutate?
- Are paths, output, messages, cache inputs, and incidents bounded?
- Are auth/account/credit operations explicit, redacted, and non-destructive by default?
- Does SQLite remain the authoritative source for replayable state?
- Are leases fenced by holder/generation and reclaimed on expiry?
- Are recovery patches written before integration?
- Do docs name what is implemented, optional, configurable, or unqualified?
- Do all required checks pass with warnings denied?
