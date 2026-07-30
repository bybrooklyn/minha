# Changelog

All notable changes to Minha are recorded here. The entries describe repository
state, not a promise of provider entitlement,
platform qualification, or release availability.

## [Unreleased]

- Reset the pre-production runtime, protocol, office, and book contracts to v1;
  prototype databases are timestamp-archived before a clean v1 store is created.
- Added Economy, Balanced, and Turbo execution profiles, evidence-triggered
  delegation, focused single-lane implementation, deterministic office rollups,
  typed completion judgments, and two bounded repair/rejudge cycles.
- Added cooperative double-Esc pause, explicit Ctrl-C interruption, a command
  palette, modal questions, dark navy default styling, and native raster corners
  with quadrant/square fallback rendering.
- Added direct DeepSeek V4 Flash/Pro reasoning routes, exact balance polling with
  a persisted reserve baseline, Max-only Flash reasoning, and cache-aware cost
  telemetry without storing provider credentials.

- Replaced the universal 128k/72% context policy with provider/model capability
  resolution and a protected five-percent boundary; session budget presets are
  now advisory rather than false terminal limits.
- Added typed stop reasons, local intent routing, consumed-tool evidence
  condensation, concurrent independent reads/searches, and per-agent context
  accounting without prose-based lifecycle inference.
- Added a public provider boundary, mixed ChatGPT/DeepSeek routing, a streamed
  DeepSeek V4 adapter with thinking and fragmented tool calls, private atomic
  credentials, provider-aware compaction, dated cost/cache-savings telemetry,
  and one-time repository disclosure.
- Added schema-v8 per-agent TODOs, compact manager rollups, bounded typed hive
  deltas with durable unread cursors, and scoped reviewable memory with FTS,
  entity ranking, supersession, tombstones, and idle extraction queues.
- Reworked the TUI around semantic activity groups, per-agent context status,
  optimistic submission, restrained themes, reduced motion, and a
  grapheme-/display-width-aware composer with paste, undo/redo, word motion,
  history boundaries, and command/path completion.
- Restored Minha's navy canvas, added a centered reading rail and progressive
  Kitty/quadrant/square filled-surface rendering, prewrapped Markdown, hidden
  internal control payloads, native-render cursor preservation, and one shared
  Unicode editor layout for wrapping, visual movement, and cursor placement.
- Simplified clarification to one inline question with direct typed answers,
  stopped background agents from stealing transcript focus, and added schema-v9
  office rooms, ordered messages, independent read cursors, and compact typed
  coordination rows with optional direct agent targeting.
- Expanded the bundled design, systems, data, and management references with
  cited guidance for terminal rendering, context hygiene, provider adapters,
  durable agent memory, and typed multi-agent communication.

- Added the automatic Issue Clarifier with an explainable five-dimension
  ambiguity meter, bounded Luna/fallback question batches, optional
  high-impact Terra advice, explicit brief confirmation, CLI/TUI answer flows,
  status/transcript visibility, and schema-v7 persistence across resume and
  fork.
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
