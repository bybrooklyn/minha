# Changelog

All notable changes to Minha are recorded here. The repository is private and
the entries describe repository state, not a promise of provider entitlement,
platform qualification, or release availability.

## [Unreleased]

- Added Rust CI for formatting, workspace tests, Clippy, rustdoc warnings, and
  Linux, Windows, and macOS test runners.
- Added RustSec advisory, dependency provenance, dependency-ban, and license
  checks.
- Added tag-driven release automation for the `minha` binary and SHA-256 asset
  files.
- Added Dependabot configuration for Cargo and GitHub Actions updates.
- Added checksum-verified `minha update` support backed by authenticated GitHub
  Releases, with check-only and repository-override modes.
- Added compact structured read-only GitHub queries and built-in quality suites
  for Rust, Python, JavaScript, and Go, including matching TUI slash commands.
- Added local HTTP/SSE provider integration tests covering headers, tool calls,
  usage/rate-limit parsing, retries, bounded errors, and secret redaction.
- Bounded retained subprocess output while continuing to drain child pipes.
- Fixed branch/snapshot recovery patches so filtered worktree metadata and a
  dropped final newline can no longer make valid worker changes fail `git
  apply`; added a complete offline planner-to-workers-to-integrator-to-judge
  regression test.
- Prevented a model judge's verified verdict from promoting a run while the
  persisted task graph still contains failed, pending, or blocked tasks.
- Confined read-only argv path arguments to the workspace and denied Git/rg
  escape hooks; bound one-use risky-command approvals to the exact argv.
- Added finite provider request timeouts and stopped implicit Responses POST
  retries while retaining bounded model-catalog GET retries.
- Preserved a single-document `--json` login contract and made `--jsonl`
  authorization progress explicit.
- Required every release tag to pass the full repository and dependency gates
  before cross-platform binaries can be published.

## [0.1.0] - 2026-07-29

- Initial private repository baseline.

[Unreleased]: https://github.com/bybrooklyn/minha/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/bybrooklyn/minha/releases/tag/v0.1.0
