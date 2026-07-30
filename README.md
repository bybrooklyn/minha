# Minha

Minha (pronounced *meen-yah*) is a Rust 2024 coding harness for direct ChatGPT Codex sessions and small, inspectable multi-agent workflows. It provides a terminal UI, a JSON/JSONL command boundary, a persistent SQLite run log, scoped repository instructions, a deliberately fixed model-facing tool surface, and a local scheduler for parallel work.

Minha talks to the ChatGPT Codex HTTP API directly. An installed `codex` executable is not required. It is a harness, not a claim that any particular account can use every model name shown below.

## Current contract

The following behavior is implemented in this repository:

- ChatGPT Codex device login, refresh-token renewal, and atomic private credential files.
- Named account profiles under `~/.minha/accounts`, with active/enable/disable/remove operations and deterministic worker-slot distribution across enabled profiles.
- Exact model-catalog discovery before a run. Configured model slugs are rejected when the account's catalog does not contain them; Minha does not silently substitute another model.
- The four named model roles `Spark`, `Luna`, `Terra`, and `Sol`; deterministic manager rollups do not spend a coordination-only model turn.
- A Ratatui TUI with live transcript reduction, task/agent/board/problem drawers, local status and diagnostic views, queued steering, interruption, session controls, and the slash commands documented below.
- A safety-focused Issue Clarifier for actionable requests whose goal and scope are both unsafe to guess: one blocking modal question at a time, optional notes, explicit brief confirmation, and no ambiguity meter for greetings or ordinary conversation.
- Persistent task graphs, path/resource leases, generation fencing, clean-repository worktrees, dirty/unborn snapshot lanes, recovery patches, integration, and a read-only completion judge.
- A fixed model surface with a compact coordinated `todo` delta tool in addition to workspace, quality, book, GitHub, and hive operations. Read-only roles lose mutation and, where policy requires, question capabilities.
- Model-aware 95% context boundaries, deterministic evidence condensation before compaction, provider prompt-cache keys, local result caching, account-window reserves, and per-agent context records.
- Versioned local books with registry metadata, trust/freshness checks, compact lexical retrieval, model-tier budgets, private drafts, and the built-in `caveman` and `talk` skills.
- Typed protocol events and SQLite persistence for sessions, messages, tasks, per-agent TODOs, agents, leases, board entries, semantic/episodic memory, usage, cache statistics, books, incidents, and compaction checkpoints.

The following are intentionally configurable or optional:

- Model slugs, reasoning effort, scheduler width, context thresholds, cache size/age, memory retrieval/generation, token-budget preset, bundled-book availability, permissions, mouse support, theme, reduced motion, and tool-detail density are configuration surfaces with active runtime paths.
- Luna handles routine work, Terra is preferred for important or failed work, and Sol is reserved for critical or high-risk work. DeepSeek Flash/Pro can participate directly when configured; live remaining balance is shown when the provider exposes it.
- Project and user skills, compatible instruction files, model account profiles, and upstream/provider behavior depend on the local environment.

The following are not claims made by the project:

- No model entitlement, plan tier, quota, unlimited-credit status, or provider availability is assumed.
- No remote write, push, merge, release, or credit redemption happens automatically. Remote GitHub mutation is possible only through permission-gated `exec`; MCP, plugin marketplaces, and arbitrary dynamic tool loading are not provided.
- Local tests do not prove interactive OAuth, live entitlement, provider compatibility, terminal rendering, or human approval of risky actions. See [Operations and qualification](docs/OPERATIONS.md).

## Quick start

Rust 1.97 or newer is required.

Install the latest checksum-verified GitHub release on macOS or x86-64 Linux:

```sh
curl --proto '=https' --tlsv1.2 -fsSL https://raw.githubusercontent.com/bybrooklyn/minha/main/install.sh | sh
```

The script downloads the target-specific binary and adjacent SHA-256 file, verifies the checksum, and installs to `~/.local/bin/minha`. Set `MINHA_INSTALL_DIR`, `MINHA_VERSION`, or `MINHA_REPO` to override those defaults. Review [`install.sh`](install.sh) before piping it to a shell. Until the first versioned GitHub release is published, build from source instead:

```sh
cargo build --workspace
cargo test --workspace
cargo run -p minha -- doctor
cargo run -p minha -- login
cargo run -p minha
```

`login` starts the official Codex device flow. Minha prints a verification URL and one-time code, polls until authorization completes, extracts the account identifier from the token claims for routing, and stores the record privately. The TUI's `/login` command starts the same flow. The provider credentials are not stored in SQLite.

`minha provider add deepseek` uses a no-echo prompt and atomically stores the API key in a mode-0600 user configuration file on Unix. `provider list`, `provider test deepseek`, and `provider remove deepseek` never expose the key or write it to SQLite; the test also reports the provider's exact current balance when available. Enabling the provider is the explicit data-routing choice. Enabled ChatGPT and DeepSeek clients participate in the same provider-neutral runtime. Routing chooses from discovered capabilities and task complexity, uses Flash Max for bounded work, and escalates failed or complex work to Pro Max. `/status` prices observed cache-hit, cache-miss, and output tokens from Minha's dated fallback metadata and reports estimated cache savings; prices remain provider-controlled and should be refreshed against the [official table](https://api-docs.deepseek.com/quick_start/pricing).

The normal command-line workflows are:

```sh
minha run "fix issue #42"
minha run "it doesn't work"
minha plan "replace the parser"
minha audit "audit the repository"
minha review "review the current diff"
minha models --json
minha status --json
minha usage --json
minha sessions --json
minha resume --prompt "continue with the failing tests"
minha run --jsonl "inspect and fix the parser"
minha update --check
minha update
```

Without a subcommand, `minha` opens the TUI. The CLI also provides `login`, `logout`, `answer`, `pickup`, `events`, `show`, `fork`, `rename`, `archive`, `diff`, `doctor`, `version`, and `update`; run `minha --help` for the parser-generated synopsis.

`minha update` asks the installed, authenticated `gh` CLI for the latest release in `bybrooklyn/minha`, selects the exact target-named binary and checksum, verifies SHA-256, and replaces the current executable atomically on Unix. `--check` performs no write, and `--repo OWNER/REPOSITORY` selects an alternate trusted release source. On Windows, the verified binary is staged beside the running executable and the result reports the manual replacement path; it does not claim the running process was replaced.

## Login, profiles, and account rotation

### Device login

The device flow is designed for a terminal without a browser callback. Run:

```sh
minha login
```

Open the printed URL, enter the printed code, and leave the process running while it polls. The default record is compatible with the named `default` profile. Login requires a successful account identifier; a token without one is rejected rather than used ambiguously.

The authorization record is written atomically with mode `0600` on Unix. The default compatibility file is `~/.minha/auth.json`. Named profiles use:

```text
~/.minha/accounts/profiles.json   profile index and active name
~/.minha/accounts/<name>.json     one private OAuth record per profile
```

JWT claims are decoded only to display/select the email, account identifier, and expiry. Minha does not authenticate JWT signatures locally; the token endpoint and provider remain the authority.

### Named profiles

Log in to multiple accounts with names containing only ASCII letters, digits, `-`, and `_`:

```sh
minha login --profile personal --label "Personal"
minha login --profile work --label "Work"
minha login list
minha login status
minha login use work
minha login disable personal
minha login enable personal
minha login remove work
```

`login use` changes which enabled profile is tried first. `disable` removes a profile from the active client pool without deleting its file; `remove` deletes that profile's credential file and index entry. `logout` removes the active profile when one exists.

For a run, enabled profile records are loaded and expired records are refreshed when a refresh token is available. The active profile is ordered first. Audit lenses and worker slots then select clients by slot modulo the enabled-client list, so parallel work is spread predictably. This is deterministic slot rotation, not quota-aware balancing or guaranteed failover: a provider can still rate-limit an account, and a failed refresh of the first profile can prevent the run from starting. Use `/models`, `minha login status`, and the Problems drawer to distinguish authentication, entitlement, and provider failures.

## Models and role routing

Minha discovers the provider model catalog before every new or resumed run. The names below are the repository defaults and role labels, not entitlement promises.

| Role | Default slug | Implemented use | Availability rule |
| --- | --- | --- | --- |
| Spark | `gpt-5.3-codex-spark` | Fast workers, audit lenses, review, completion judge | Preferred when discovered; DeepSeek Flash can serve bounded worker, audit, and review routes |
| Luna | `gpt-5.6-luna` | Planner, lead, integration, normal session continuation | Preferred for balanced/lead work; exact configured slug must be available for the selected route |
| Terra | `gpt-5.6-terra` | Optional ambiguity consultation and configured complex lead | Used only when selected/configured and discovered; no automatic entitlement assumption |
| Sol | `gpt-5.6-sol` | Optional high-risk consultation and quality route | Used only when selected/configured and discovered; no automatic entitlement assumption |
| Manager | `gpt-5.4-mini` | Optional coordination review of task ownership, dependencies, and convergence | Configurable string, not one of the four typed model roles; skipped if unavailable or if its turn errors |
| DeepSeek Flash / Pro | `deepseek/deepseek-v4-flash`, `deepseek/deepseek-v4-pro` | Classification, bounded workers/audits, complex leads, retries, integration, and compaction | Available only after a private key is configured; exact provider-qualified slugs are checked during preflight |

`minha models --json` reports the live catalog. The cached catalog is reused for up to 15 minutes and may be used as a stale fallback for up to 24 hours when refresh fails; it does not turn an unavailable configured slug into an available one. Change role slugs in `minha.toml`; see [Configuration](docs/CONFIGURATION.md).

Routing is bounded and local. Clear chat, continuation, planning, implementation, audit, and review intent does not spend a tool-enabled classifier turn. Ambiguous intent uses a bounded no-tool classifier. Available providers may mix within one run: audit lenses alternate eligible Spark/Flash workers, bounded tasks may use Flash, complex or failed tasks may use Pro, and every assignment is emitted as typed runtime state.

## Issue Clarifier

Minha routes intent before deciding whether clarification is needed. Greetings, chat, planning, audits, and reviews proceed normally. Only an actionable mutation request pauses when guessing its scope, safety constraints, or expected result could materially change the work. The internal preflight still tracks goal, reproduction, scope, constraints, and success criteria, but the normal TUI presents one useful question rather than an intake dashboard.

The interaction is intentionally forgiving:

- choose one of two or three plain-language options, choose **Not sure**, or select **Other**;
- start typing to supply a free-text answer, then press `Enter` to submit it;
- use `/best` to delegate unresolved details to Minha's safest repository-supported judgment;
- use `/summary` to review the best current brief, or `/cancel` to preserve the session without starting work;
- confirm the observed behavior, expected result, evidence, scope, constraints, success criteria, and assumptions before agents begin editing;
- choose **Edit** or **Keep clarifying** when the brief does not sound right.

The deterministic internal state decides whether clarification is needed and when a brief is ready. Luna may improve wording and choices, but malformed or unavailable output falls back to local questions. Internal model tags and JSON remain in durable events and never appear in the conversational transcript.

CLI question IDs are durable and batchable:

```sh
minha answer --answer goal-1=wrong --answer scope-1=tui
minha answer "The problem happens after I reopen the session"
minha answer confirm
```

The positional form answers the first pending question and remains useful for one-question recovery. Repeat `--answer ID=VALUE` to answer a displayed batch without losing field identity. `minha status`, `minha show`, JSON output, transcript export, replay, and forks retain the clarification snapshot and confirmed brief. Workspace-local text and log paths may be used as bounded read-only evidence; screenshot paths are recorded as evidence only and are not treated as image understanding.

## TUI

The TUI uses Minha's navy canvas, a centered readable conversation rail, and borderless transcript content. Focused controls use filled surfaces with an automatic Kitty raster, two-color quadrant, then square fallback chain. It shows optimistic queued/working state immediately and uses a Unicode-aware movable composer cursor.

Useful keys:

| Key | Action |
| --- | --- |
| `Enter` | Send a prompt, accept the highlighted clarification choice, answer a blocking question, or queue steering while work runs |
| `Shift+Enter` / `Ctrl+J` | Insert a newline |
| `Ctrl+R` | Recall input history |
| `Ctrl+P` | Open the command palette |
| `Tab` | Complete slash commands or paths |
| `Shift+Tab` | Cycle Activity, Work, Board, Problems, and closed drawer states |
| Arrow / `Alt+Arrow` | Move by grapheme, display-width-correct line, or word |
| `Ctrl+Z` / `Ctrl+Y` | Undo / redo composer edits |
| `Enter` on an agent | Open its transcript |
| `Ctrl+O` | Expand/collapse the nearest semantic activity group or diff |
| `Ctrl+T` | Toggle task detail |
| `Esc` | Close an overlay; press twice during work to pause cooperatively at a safe boundary |
| `Ctrl+C` | Explicitly interrupt active work |
| Mouse wheel/click | Scroll normally, place the composer cursor, operate drawers, or answer a modal question when `tui.mouse = true` |

Use `/to AGENT_ID` to target the next composer submission to one active agent; `/to` without an ID clears the target. Coordination remains compact in the transcript and its durable room/message state is inspectable through Activity and Work.

### Slash commands

Commands are local TUI controls. They are not additional model tools.

| Command | Implemented behavior |
| --- | --- |
| `/new` | Clear the current TUI session state and start a new local session view |
| `/resume` | Open the persisted-session picker |
| `/retry` | Retry the active run in the same durable session, preserving task graph and recovery state |
| `/retry --fresh` | Start a new run with the same goal; the previous transcript remains in session history |
| `/fork` | Fork the active persisted run and load the child |
| `/plan`, `/implement`, `/audit`, `/review` | Select a mode; with text, start that mode immediately |
| `/diff` | Show the current repository diff |
| `/agents`, `/activity` | Open the Activity/Hive drawer |
| `/work` | Open the task/work drawer |
| `/board` | Open the shared board drawer and refresh board entries |
| `/problems` | Open recorded incidents and retry guidance |
| `/note TEXT` | Add a session note |
| `/pin [ID]`, `/resolve [ID]` | Pin or resolve the selected/identified board entry where permitted |
| `/compact` | Queue predictive context compaction at the next model boundary |
| `/model` | Show the configured lead and worker labels |
| `/skills` | List discovered skill metadata |
| `/ask TEXT` | Answer a small local status question from current TUI state |
| `/usage` | Show current session input/output tokens and context estimate |
| `/status` | Open the real local status inspector/dashboard, refresh session/context/tasks/agents/board/cache/usage data, and also append a compact status card to the transcript |
| `/context` | Open the context/usage overlay |
| `/memory QUERY`, `/memory pin ID`, `/memory delete ID` | Search, pin, or tombstone reviewable durable memory |
| `/memories [enabled|use|generate] [on|off]` | Inspect or change project memory controls |
| `/doctor` | Run local workspace, database, book, cache, and model-catalog diagnostics |
| `/check`, `/lint`, `/test`, `/docs`, `/security` | Run the detected workspace's bounded built-in quality action and persist the tool result when a session is active |
| `/quality [ACTION]` | Detect the Rust/Python/JavaScript/Go quality suite, or run `check`, `lint`, `test`, `docs`, `security`, `all`, or `format` |
| `/gh [ACTION] [NUMBER]` | Read compact structured GitHub repository, issue, PR, check, run, workflow, or release data through authenticated `gh` |
| `/clean` | Prune expired/over-limit cache entries and reclaim expired task leases |
| `/books` | Open the indexed-books overlay |
| `/login` | Start the device login overlay |
| `/transcript [PATH]` | Export the displayed transcript to the default or requested path |
| `/help` | Open command help |
| `/auto` | Return to automatic mode selection |
| `/quit` or `/exit` | Leave the TUI |

While a run is active, ordinary text is queued as steering and independent tasks continue. A persisted `ask_user` question pauses only the asking task and its dependents; `/answer` is available through the CLI for non-TUI recovery. Issue Clarifier is a pre-work gate, so `Esc` clears its current text instead of silently dismissing the required brief confirmation.

## Hivemind execution

An implementation run is a durable workflow:

```text
goal or steering
  -> local intent route
  -> if goal and scope are unsafe to guess: one modal question -> confirmed brief
  -> focused Luna lane, or an evidence-justified bounded DAG
  -> optional Terra/Sol consultation
  -> deterministic manager/task/TODO rollup
  -> ready, path-disjoint tasks
  -> Spark workers in branches or snapshot lanes
  -> recovery patch persistence
  -> Luna integration and checks
  -> read-only typed completion judge (up to two repair/rejudge cycles)
  -> typed events, transcript, and SQLite state
```

The scheduler defaults to one focused lane and expands only for independent work. Economy stays single-lane, Balanced requires a meaningful speedup with bounded coordination cost, and Turbo may use `hard_max_agents` for truly disjoint work. Dependencies must converge before a dependent task becomes ready. Resource leases carry a generation and can be released only by the matching task/agent/generation.

Clean repositories use generated worktrees named like `minha/<run>/<task>`. Dirty or unborn repositories use isolated filesystem snapshot lanes so current user changes are not replaced by a clean checkout assumption. `.git`, `.minha`, build products, caches, and dependency trees are excluded from snapshot copies. Workers do not commit. Their text and patches are written below `.minha/recovery/<run>/` before primary-checkout application.

Minha does not auto-commit, merge, push, or discard user changes. Resume, retry, fork, archive, transcript export, task state, board notes, leases, and recovery patches remain inspectable. The SQLite run log is authoritative; a Markdown plan or model prose is not runtime state.

The office/hive layer carries compact typed messages for task, evidence, integration, incident, progress, health, and replacement handoff records. Private rooms restrict recipients and messages are schema-versioned and size-bounded. The model sees coordinated `hive` only for the agents that need it.

## Fixed tools and no MCP

The model-facing tool catalog is intentionally closed:

1. `read_files` — batched workspace line-range reads with byte caps.
2. `search` — bounded `rg` search.
3. `apply_patch` — workspace-contained unified patches, checked before mutation; omitted from read-only roles.
4. `exec` — argv-only execution without a shell, with timeout/output bounds and command policy checks.
5. `ask_user` — persisted blocking questions, omitted when the role cannot ask.
6. `books` — bounded search/read/draft/feedback access to the local book catalog.
7. `github` — structured, bounded, read-only repository/issue/PR/check/run/workflow/release queries through authenticated `gh`.
8. `quality` — detects and runs conventional bounded checks for Rust, Python, JavaScript, and Go in one compact call.
9. `hive` — coordinated inbox, typed messages, board entries, and artifact references for coordinated agents.

The structured `github` tool is read-only. Agents can use the full installed `gh` CLI through `exec`, but remote mutations are classified separately and follow `permissions.remote_writes`; the default is a one-use question. Shell interpolation, pipelines, redirection, arbitrary tool names, and remote/destructive actions are not implicit capabilities. Some operations are always denied. Minha has no MCP client, MCP server, plugin marketplace, or arbitrary dynamic tool loading.

## Token economy, cache, and safety

Minha records provider-reported input, output, cached-input, cache-write, and reasoning-output tokens per turn. It estimates context per agent, forecasts the next request, protects the final five percent, and compacts only when deterministic evidence condensation cannot make the bounded call fit. Consumed read/search/command/patch results become typed digest-backed evidence summaries while raw events remain in SQLite. Auto-compaction writes a durable checkpoint chain and rejects empty checkpoints. `/compact` queues this operation; it does not immediately rewrite the transcript.

Durable memory is separate from compaction. Clean schema v1 retains user, project, and run scopes with FTS/entity retrieval, validity, confidence, salience, provenance, pinning, supersession, tombstones, and access counts. Completed or conclusively blocked runs queue secret-filtered episodic extraction for idle preflight processing. Generated memory is advisory: repository instructions and current evidence remain authoritative. Per-agent TODO deltas survive retries and compaction; the live rollup is deterministic. Use `minha memory`, `minha memories`, `/memory`, and `/memories` to inspect and control memory.

The local cache has three explicit classes:

- `exact`: reusable indefinitely when the versioned request and observed-input manifest match.
- `ttl`: reusable until the configured age expires.
- `never`: never read or written; the bypass is counted.

Observed inputs are sorted and hashed into cache keys. Secret-like filenames and common credential markers are rejected before they become cache inputs. A bounded in-memory LRU serves repeated process-local hits; every hot hit is checked against SQLite before use, then updates the same durable schema-v1 counters. The runtime locally replays only deterministic, secret-safe compaction results; it does not replay coding-agent answers that may depend on mutable workspace state. Provider caching and Minha's local result cache are separate mechanisms.

Account rate-limit windows are read-only provider data. When the configured reserve is reached, Minha persists `usage_paused`, does not execute a pending tool call, and makes no further model call for that run. Use `minha pickup [RUN]` after the window resets. Minha never redeems or purchases credits.

## Books and skills

The bundled registry is schema version 1 with ten packs and at least 100 promoted, current entries. Each entry carries an ID/version, taxonomy, tags, source metadata, trust state, freshness metadata, citations, and ordered index/compact/detailed token budgets. The bundled manifest includes registry/key/signature/digest fields and is checked for structural consistency; the current local code treats those signature fields as registry metadata, not as a cryptographic verification implementation.

Retrieval is lexical and bounded. It searches searchable trust states (`verified`, `promoted`, and `stale`), scores query/path/language/tag matches, and returns compact metadata/facts before a detailed read. Model tiers use approximately 4k/16k/32k input-token retrieval budgets for start/Spark/larger work, with output limits of 1k/4k/8k. Draft books are private and not searchable until verification; stale books remain visible with reduced trust and must not be treated as current without review.

Skill discovery reads metadata first and loads a full `SKILL.md` only after selection. Project and user locations include `.minha/skills`, `.codex/skills`, `.claude/skills`, `.agents/skills`, and `skills`; built-ins include `$caveman` and `$talk`. `$caveman` supports lite/full/ultra and wenyan variants and uses compact `claim -> evidence -> next` reports internally. A skill does not enlarge the fixed tool catalog.

## Configuration

Minha starts with built-in defaults, recursively overlays the user file at the platform config directory's `minha/config.toml` (on macOS this is normally `~/Library/Application Support/minha/config.toml`), then recursively overlays `<project>/minha.toml`. Relative `database_path` values resolve from the project root. See [`minha.toml.example`](minha.toml.example) and the field-by-field [Configuration guide](docs/CONFIGURATION.md).

The most important defaults are:

```text
database:            .minha/minha.sqlite3
workers:             2 minimum, 8 normal maximum, 16 hard maximum
usage reserve:       12 percent
context:             provider/model discovered; routine work ends at 95 percent
cache:               enabled; 512 MiB; 30 days; 128 hot entries
memory:              enabled; retrieve and generate; 5 injected results maximum
budget:              balanced (100,000 soft optimization target)
books:               embedded; per-book bounded index/compact/detailed reads
permissions:         remote writes ask; destructive actions ask; always-on secret checks
tui:                 auto theme; motion enabled; NO_COLOR honored
```

Every field shown in [`minha.toml.example`](minha.toml.example) has an active runtime path. Configuration cannot create new model tools or grant provider entitlements.

## Troubleshooting and qualification

Start with:

```sh
minha doctor
minha login status
minha models --json
minha status --json
minha usage --json
```

Common recovery actions:

- `auth.login_required` or `auth.account_id_missing`: log in again, then inspect profile status; do not copy tokens into issue reports.
- `model.unavailable`: compare exact configured slugs with `minha models --json`; this is an entitlement/catalog issue, not permission to substitute silently.
- `provider.rate_limited`: wait for the read-only reset window, enable another authorized profile if available, or use `minha pickup` after pause.
- `provider.transport` or `provider.unavailable`: preserve the run and use `/retry` or `minha resume`; the durable task graph remains available.
- `state.persistence`: stop mutating the workspace, run `/doctor`, and preserve `.minha/minha.sqlite3` and the incident correlation ID.
- `config.invalid`: repair `minha.toml` against [`minha.toml.example`](minha.toml.example); validation rejects impossible scheduler, context, cache, budget, and book limits.
- `tool.permission_denied`: review read-only role and risky-command policy; a denied action is not evidence that the command was attempted.

The [operations guide](docs/OPERATIONS.md) documents session recovery, cache cleanup, incidents, logs, account rotation, and the boundary between local evidence and live qualification. The [security boundary](SECURITY.md) is the security-specific policy document.

## Architecture and contribution

The workspace contains:

- `minha-core`: authentication, provider transport, model discovery, runtime orchestration, fixed tools, instructions/skills, books, worktrees, SQLite state, usage, cache, office coordination, and judging.
- `minha-tui`: Ratatui state reduction, overlays, drawers, command handling, and rendering.
- `minha-cli`: the human and machine-readable CLI boundary.

Read [Architecture](docs/ARCHITECTURE.md) for data flow and persistence, [Configuration](docs/CONFIGURATION.md) for policy fields, [Operations](docs/OPERATIONS.md) for running and recovery, and [Contributing](docs/CONTRIBUTING.md) for documentation/code boundaries and required checks.

Before claiming completion, run:

```sh
cargo fmt --all -- --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

Preserve unrelated worktree changes. Do not push, merge, release, or redeem account credits without explicit authority.

## Provenance and license

The protocol adapter was independently implemented against the published Apache-2.0 OpenAI Codex source pinned in [`vendor/codex/UPSTREAM.toml`](vendor/codex/UPSTREAM.toml). No upstream source file is copied verbatim. See [`NOTICE`](NOTICE) and [`vendor/codex/PATCHES.md`](vendor/codex/PATCHES.md).

Minha is licensed under Apache-2.0.
