# Minha

Minha (pronounced *meen-yah*) is a Rust 2024 coding harness for direct ChatGPT Codex sessions and small, inspectable multi-agent workflows. It provides a terminal UI, a JSON/JSONL command boundary, a persistent SQLite run log, scoped repository instructions, a deliberately fixed model-facing tool surface, and a local scheduler for parallel work.

Minha talks to the ChatGPT Codex HTTP API directly. An installed `codex` executable is not required. It is a harness, not a claim that any particular account can use every model name shown below.

## Current contract

The following behavior is implemented in this repository:

- ChatGPT Codex device login, refresh-token renewal, and atomic private credential files.
- Named account profiles under `~/.minha/accounts`, with active/enable/disable/remove operations and deterministic worker-slot distribution across enabled profiles.
- Exact model-catalog discovery before a run. Configured model slugs are rejected when the account's catalog does not contain them; Minha does not silently substitute another model.
- The four named model roles `Spark`, `Luna`, `Terra`, and `Sol`, plus an optional configurable coordination-manager slug whose default is `gpt-5.4-mini`.
- A Ratatui TUI with live transcript reduction, task/agent/board/problem drawers, local status and diagnostic views, queued steering, interruption, session controls, and the slash commands documented below.
- Persistent task graphs, path/resource leases, generation fencing, clean-repository worktrees, dirty/unborn snapshot lanes, recovery patches, integration, and a read-only completion judge.
- A nine-tool model surface: `read_files`, `search`, `apply_patch`, `exec`, `ask_user`, `books`, structured read-only `github`, bundled `quality`, and coordinated `hive`. Read-only roles lose mutation and, where policy requires, question capabilities.
- Bounded context estimation and predictive compaction, provider prompt-cache keys, a local model-result cache with exact/TTL/never classes, cache metrics, account-window reserves, and per-turn token records.
- Versioned local books with registry metadata, trust/freshness checks, compact lexical retrieval, model-tier budgets, private drafts, and the built-in `caveman` and `talk` skills.
- Typed protocol events and SQLite persistence for sessions, messages, tasks, agents, leases, board entries, usage, cache statistics, books, incidents, and compaction checkpoints.

The following are intentionally configurable or optional:

- Model slugs, reasoning effort, scheduler width, context thresholds, cache size/age, token-budget preset, bundled-book availability, permissions, mouse support, and tool-detail density are configuration surfaces with active runtime paths.
- Terra and Sol consultation are requested by a Luna plan only for material ambiguity or high-risk decisions. The manager is used only when its configured slug is present in the discovered catalog; a manager failure is non-fatal.
- Project and user skills, compatible instruction files, model account profiles, and upstream/provider behavior depend on the local environment.

The following are not claims made by the project:

- No model entitlement, plan tier, quota, unlimited-credit status, or provider availability is assumed.
- No remote write, push, merge, release, or credit redemption happens automatically. Remote GitHub mutation is possible only through permission-gated `exec`; MCP, plugin marketplaces, and arbitrary dynamic tool loading are not provided.
- Local tests do not prove interactive OAuth, live entitlement, provider compatibility, terminal rendering, or human approval of risky actions. See [Operations and qualification](docs/OPERATIONS.md).

## Quick start

Rust 1.97 or newer is required.

```sh
cargo build --workspace
cargo test --workspace
cargo run -p minha -- doctor
cargo run -p minha -- login
cargo run -p minha
```

`login` starts the official Codex device flow. Minha prints a verification URL and one-time code, polls until authorization completes, extracts the account identifier from the token claims for routing, and stores the record privately. The TUI's `/login` command starts the same flow. The provider credentials are not stored in SQLite.

The normal command-line workflows are:

```sh
minha run "fix issue #42"
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
| Spark | `gpt-5.3-codex-spark` | Fast workers, audit lenses, review, completion judge | Required for worker, audit, and review paths; exact slug must be in the discovered catalog |
| Luna | `gpt-5.6-luna` | Planner, lead, integration, normal session continuation | Preferred for balanced/lead work; exact configured slug must be available for the selected route |
| Terra | `gpt-5.6-terra` | Optional ambiguity consultation and configured complex lead | Used only when selected/configured and discovered; no automatic entitlement assumption |
| Sol | `gpt-5.6-sol` | Optional high-risk consultation and quality route | Used only when selected/configured and discovered; no automatic entitlement assumption |
| Manager | `gpt-5.4-mini` | Optional coordination review of task ownership, dependencies, and convergence | Configurable string, not one of the four typed model roles; skipped if unavailable or if its turn errors |

`minha models --json` reports the live catalog. The cached catalog is reused for up to 15 minutes and may be used as a stale fallback for up to 24 hours when refresh fails; it does not turn an unavailable configured slug into an available one. Change role slugs in `minha.toml`; see [Configuration](docs/CONFIGURATION.md).

Routing is bounded and local: fast work prefers Spark, balanced work Luna, reasoning Terra, and quality Sol. A missing preferred candidate can route within the configured candidate set, but the provider preflight still checks the exact model strings required by the selected workflow. There is no silent cross-provider fallback.

## TUI

The TUI keeps model prose open and puts user, tool, error, status, and diagnostic content in bounded cards. It shows only agents and tasks observed in the runtime event stream; it does not invent workers for visual effect.

Useful keys:

| Key | Action |
| --- | --- |
| `Enter` | Send a prompt, answer a blocking question, or queue steering while work runs |
| `Shift+Enter` / `Ctrl+J` | Insert a newline |
| `Ctrl+R` | Recall input history |
| `Tab` | Cycle Activity/Hive, Work, Board, Problems, and closed drawer states |
| `Enter` on an agent | Open its transcript |
| `Ctrl+O` | Expand/collapse tool and diff detail |
| `Ctrl+T` | Toggle task detail |
| `Esc` / `Ctrl+C` | Close an overlay or interrupt active work |
| Mouse wheel/click | Scroll and select when `tui.mouse = true` (active) |

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

While a run is active, ordinary text is queued as steering and independent tasks continue. A persisted `ask_user` question pauses only the asking task and its dependents; `/answer` is available through the CLI for non-TUI recovery.

## Hivemind execution

An implementation run is a durable workflow:

```text
goal or steering
  -> Luna plan and bounded DAG
  -> optional Terra/Sol consultation
  -> optional 5.4-mini manager review
  -> ready, path-disjoint tasks
  -> Spark workers in branches or snapshot lanes
  -> recovery patch persistence
  -> Luna integration and checks
  -> read-only Spark completion judge
  -> typed events, transcript, and SQLite state
```

The scheduler runs no more than the configured `max_agents` workers; `hard_max_agents` is a validated ceiling, not a second expansion target in the current scheduler. Dependencies must converge before a dependent task becomes ready. Resource leases are acquired for declared paths/resources, carry a generation, expire, and are released only by the matching task/agent/generation. A stale worker cannot write through a newer lease.

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

Minha records provider-reported input, output, cached-input, cache-write, and reasoning-output tokens per turn. It estimates context with a conservative local heuristic, predicts whether the next turn crosses the configured threshold, and asks the active model for a durable summary while retaining recent turns. `fact_limit` is included in the compaction instructions; it is not a separate persisted-fact extractor. Auto-compaction writes a durable checkpoint chain: each checkpoint links to its predecessor, and the preceding persisted messages are marked compacted when the observed input manifest is secret-safe. If observed inputs look secret-like, Minha does not create that manifest-backed checkpoint/cache input. `/compact` queues this operation; it does not immediately rewrite the transcript.

The local cache has three explicit classes:

- `exact`: reusable indefinitely when the versioned request and observed-input manifest match.
- `ttl`: reusable until the configured age expires.
- `never`: never read or written; the bypass is counted.

Observed inputs are sorted and hashed into cache keys. Secret-like filenames and common credential markers are rejected before they become cache inputs. A bounded in-memory LRU serves repeated process-local hits; every hot hit is checked against SQLite before use, then updates the same durable counters. Cache entries are stored in SQLite schema v6, with durable `cache_stats` for hits/misses/writes/bypasses/bytes/saved-input-token metrics, and pruned by age and size. The runtime locally replays only deterministic, secret-safe compaction results; it does not replay coding-agent answers that may depend on mutable workspace state. Provider prompt-cache keys keep stable instruction prefixes reusable across compatible requests; provider caching and Minha's local result cache are separate mechanisms.

Account rate-limit windows are read-only provider data. When the configured reserve is reached, Minha persists `usage_paused`, does not execute a pending tool call, and makes no further model call for that run. Use `minha pickup [RUN]` after the window resets. Minha never redeems or purchases credits.

## Books and skills

The bundled registry is schema version 2 with ten packs and at least 100 promoted, current entries. Each entry carries an ID/version, taxonomy, tags, source metadata, trust state, freshness metadata, citations, and ordered index/compact/detailed token budgets. The bundled manifest includes registry/key/signature/digest fields and is checked for structural consistency; the current local code treats those signature fields as registry metadata, not as a cryptographic verification implementation.

Retrieval is lexical and bounded. It searches searchable trust states (`verified`, `promoted`, and `stale`), scores query/path/language/tag matches, and returns compact metadata/facts before a detailed read. Model tiers use approximately 4k/16k/32k input-token retrieval budgets for start/Spark/larger work, with output limits of 1k/4k/8k. Draft books are private and not searchable until verification; stale books remain visible with reduced trust and must not be treated as current without review.

Skill discovery reads metadata first and loads a full `SKILL.md` only after selection. Project and user locations include `.minha/skills`, `.codex/skills`, `.claude/skills`, `.agents/skills`, and `skills`; built-ins include `$caveman` and `$talk`. `$caveman` supports lite/full/ultra and wenyan variants and uses compact `claim -> evidence -> next` reports internally. A skill does not enlarge the fixed tool catalog.

## Configuration

Minha starts with built-in defaults, recursively overlays the user file at the platform config directory's `minha/config.toml` (on macOS this is normally `~/Library/Application Support/minha/config.toml`), then recursively overlays `<project>/minha.toml`. Relative `database_path` values resolve from the project root. See [`minha.toml.example`](minha.toml.example) and the field-by-field [Configuration guide](docs/CONFIGURATION.md).

The most important defaults are:

```text
database:            .minha/minha.sqlite3
workers:             2 minimum, 8 normal maximum, 16 hard maximum
usage reserve:       12 percent
context:             128,000 tokens; compact at 72 percent; reserve 16,384
cache:               enabled; 512 MiB; 30 days; 128 hot entries
budget:              balanced (100,000 total session tokens)
books:               embedded; per-book bounded index/compact/detailed reads
permissions:         remote writes ask; destructive actions ask; always-on secret checks
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
