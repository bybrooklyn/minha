# Architecture

Minha is a local Rust application with a direct provider boundary and a persistent, typed runtime. The provider supplies model calls and account metadata; the local runtime owns workspace safety, scheduling, coordination, persistence, recovery, and presentation.

## Workspace crates

| Crate | Boundary |
| --- | --- |
| `minha-core` | OAuth/device flow, provider transport, exact model discovery, runtime actor, fixed tools, instruction/skill discovery, books, Git lanes/worktrees, SQLite store, usage/cache, office coordination, and completion judging |
| `minha-tui` | TUI input, slash-command handling, event reduction, overlays/drawers, transcript/status rendering, local commands, and login/diagnostic presentation |
| `minha-cli` | Clap command parsing, stable result envelopes/exit codes, JSON and JSONL output, login/profile operations, run/session inspection, and the TUI entry point |

The CLI and TUI call the same `Harness` and `Store`; the TUI is not a second scheduler.

## End-to-end run flow

```text
user goal / prompt / steering / interrupt
        |
        v
in-process Harness runtime actor
        |
        +--> typed RuntimeEvent stream + SQLite session log
        |
        +--> scoped instructions + lazy skill metadata/body loading
        |
        +--> enabled account profiles + token refresh
        |
        +--> provider model catalog (fresh, ETag, or bounded stale fallback)
        |
        +--> Luna lead/planner route and bounded branch plan
        |       |
        |       +--> optional Terra ambiguity consultation
        |       +--> optional Sol high-risk consultation
        |       +--> optional configured 5.4-mini coordination review
        |
        +--> persistent DAG, path/resource leases, and ready queue
        |
        +--> Spark workers in Git worktrees or snapshot lanes
        |       |
        |       +--> task evidence, board/hive messages, artifacts
        |       +--> retry generation or failure incident
        |       +--> recovery patch under .minha/recovery/<run>/
        |
        +--> Luna integration and local validation
        |
        +--> read-only Spark completion judge
        |
        +--> terminal state, transcript, metrics, and replayable events
```

Plan runs use one read-only planner lane. Review runs use a read-only Spark lane. Audit runs use up to the configured worker count of independent Spark lenses (correctness, tests, performance, security, and maintainability) followed by a lead synthesis. Implementation consults Terra or Sol only when the plan explicitly requests the matching consultation; neither is the default.

## Provider boundary

`provider.rs` sends typed JSON to the ChatGPT Codex `models` and `responses` endpoints and parses split SSE frames. Requests carry bearer authorization, the ChatGPT account header, a Minha originator, the exact configured model slug, low text verbosity, role labels, prompt-cache keys, and encrypted reasoning carryover where the provider supports it. Catalog GETs have bounded transient retries; model-turn POSTs deliberately do not retry because processing may have started before a transport or server error became visible. Every request has a finite total timeout.

Completed output items are retained so stateless tool loops can return function-call outputs. Text deltas can be broadcast to the TUI, while completed transcript items are persisted without one SQLite write per token. Provider failures are converted into bounded incident data with redacted diagnostics and request identifiers.

Model discovery is a preflight, not a promise. A catalog is reused for 15 minutes, refreshed with an ETag when possible, and may be used as a stale fallback for 24 hours after a refresh failure. The runtime still requires the exact configured slug for the selected route. It does not infer entitlement from a default name.

Enabled account clients are sorted with the active profile first. Worker and audit slots use `slot % enabled_clients.len()`. This spreads parallel calls predictably; it is not a quota-aware health balancer. An expiring record is refreshed before use and written back to its profile atomically.

## Fixed model-facing tools

The provider tool schema is assembled by `tool_definitions` and is intentionally closed:

| Tool | Capability and boundary |
| --- | --- |
| `read_files` | Batched workspace file/range reads with a shared read cap |
| `search` | `rg` search with bounded results |
| `apply_patch` | Unified patch application after path validation and `git apply --check`; absent from read-only roles |
| `exec` | argv-only child process execution, no shell interpolation, timeout and output caps |
| `ask_user` | Persisted blocking question; omitted for roles that cannot ask |
| `books` | Search/read/draft/feedback against the indexed local catalog |
| `github` | Bounded structured read-only queries through the authenticated `gh` CLI |
| `quality` | Detection and bounded conventional checks for Rust, Python, JavaScript, and Go |
| `hive` | Coordinated inbox/messages/board/artifact operations, only for coordinated agents |

`github` constructs an allowlisted `gh` argv and requests JSON fields directly, reducing prompt volume and preventing that convenience path from mutating remote state. Full `gh` usage remains possible through `exec`; any remote mutation follows the dedicated policy below.

`quality` collapses tool discovery and standard checks into one call. It drains child output while retaining only configured byte caps, so noisy builds cannot allocate unbounded transcript memory. Missing optional security tools are reported as skipped rather than silently treated as successful checks.

`exec` has a separate preflight for destructive, remote, credential, history-changing, or deletion-like commands. The `destructive` and `remote_writes` tri-state policies are routed independently for risky `exec` calls; each can deny, ask, or allow its own dimension. User acknowledgement is one-use. Some command forms are always denied. This tool list is not extended by skills, configuration, `.agents/`, `.claude/`, or provider responses. There is no MCP client, plugin loader, or arbitrary runtime tool registration.

The older internal role/risk catalog in `tools.rs` is a policy primitive for Git/worktree/recovery operations. It is distinct from the concrete Responses schema in `executor.rs`; documentation should not merge those two names into one model-facing catalog.

## Runtime persistence

SQLite is the source of truth for execution state. WAL mode, foreign keys, and a busy timeout are enabled. The schema records:

- runs, messages, typed ordered events, mode, terminal state, summary, parent/fork relationship, and pending questions;
- workspaces, agents, task DAG nodes/dependencies, attempts, generations, and path/resource leases;
- revisioned board entries, hive messages, artifacts, and incident records with correlation IDs;
- per-turn input/output/cached/reasoning usage, account rate-limit snapshots, and model catalogs with ETags;
- local cache entries, durable schema-v6 `cache_stats`, indexed book metadata, and compaction checkpoints.

Schema migration intentionally discards the unqualified prototype-v2 run/message/event tables at the v3 boundary so stringly prototype data cannot masquerade as replayable state. Later migrations through the current schema are additive. A database newer than the supported schema is rejected rather than downgraded.

The TUI reduces `RuntimeEvent` envelopes both live and during replay. It does not reconstruct task state from model prose. JSON and JSONL output use the same typed state and event stream exposed by the CLI.

## Hivemind and office coordination

The planning graph is acyclic and versioned. A node becomes ready only when every prerequisite succeeded. The scheduler launches only ready tasks whose declared resource sets do not conflict, and its active worker limit is `max_agents`; `hard_max_agents` is currently a validated ceiling rather than an expansion mechanism.

The office layer provides typed records for agents, tasks, evidence, artifacts, progress, anonymous health metrics, incidents, and replacement handoffs. Hive messages are private-room scoped, schema-versioned, recipient-checked, and size-bounded. Board entries distinguish project-wide decisions/constraints from run-local findings and progress; project pins are user-controlled rather than a free model write.

Task leases protect both the graph and the physical workspace:

1. The scheduler assigns an agent and generation.
2. It acquires all declared resources with an expiry.
3. The worker operates only in its branch/lane and posts evidence/artifacts as needed.
4. Completion, retry, or failure releases the matching lease.
5. An expired or stale generation cannot release or mutate a newer lease.

Interrupted tasks return to pending with a new generation. A worker failure gets one bounded retry with a new fenced generation. Exhausted work is recorded as failed and remains available for explicit retry/recovery.

## Branch isolation and recovery

For a clean Git repository, a worker receives a generated worktree branch named `minha/<run>/<task>`. For a dirty or unborn repository, the runtime creates baseline and lane snapshots from the current source. Snapshot lanes omit `.git`, `.minha`, build products, caches, and dependency trees; this keeps user changes without copying local coordination state into every lane.

Workers do not commit. Before an integration application, textual and binary-capable patches are persisted under `.minha/recovery/<run>/`. The integrator inspects the primary checkout, applies recovery material through the fixed patch path, resolves missing work, and runs sufficient local checks. Minha never commits, merges, pushes, or discards changes automatically.

Git worktree lanes are copied into metadata-free snapshots before diffing, so linked-worktree `.git` pointers never enter recovery material. Normalized patches preserve the terminating newline required by `git apply`. A completion judge can inspect and challenge the result, but its prose cannot promote a run to `succeeded` while the persisted task graph contains unresolved states.

## Context and cache economy

Repository instructions form a stable prompt prefix. The body of every compatibility file shares a hard budget. Skill metadata is listed eagerly; full bodies are loaded only after selection. Search, reads, logs, board queries, and tool output are bounded before entering model context. Independent worker histories stay independent; the parent receives bounded summaries.

Context uses a conservative, model-independent estimate rather than claiming tokenizer precision. Predictive compaction triggers at the configured percentage or before the hard limit, asks the active lead for a durable summary, retains recent turns, records the before-estimate, and continues. `fact_limit` is included in the compaction instructions, but is not a separate persisted-fact extraction pipeline. When the observed input manifest is secret-safe, the store writes a durable checkpoint chain with a predecessor link and marks the prior persisted messages compacted in the same transaction. If observed inputs are secret-like, no manifest-backed checkpoint/cache input is created. A forced `/compact` request is queued for a model boundary.

The local cache and provider prompt cache are different:

- Local result cache keys include a namespace, request bytes, and a sorted observed-input manifest. `exact` entries have no policy expiry, `ttl` entries expire by age, and `never` entries are bypassed. Secret-like filenames and common credential markers are rejected before hashing.
- Provider prompt-cache keys identify reusable stable prompt prefixes. Provider-reported cached-input and cache-write counts are recorded but are not confused with local result-cache hits.
- SQLite schema v6 durably stores cache statistics including hits, misses, writes, bypasses, bytes read/written, and estimated saved input tokens. `/clean` removes expired entries and trims by least-recent use to the configured size.

Usage reserves are enforced before the next model or tool boundary. If an account rate-limit window reaches the configured reserve, the run enters `usage_paused` without executing a pending tool call. `minha pickup` resumes after reset. Credits and account headers are observational only. The configured consultation/recovery percentages are validated policy metadata; the enforced local budget is the selected global session token budget.

## Books and skills

The bundled book registry is versioned separately from the runtime schema. Current bundled content is book schema v2, ten packs, and at least 100 entries. Manifest fields include registry identity, key ID, content digest, signature metadata, pack IDs, entry paths, trust, freshness, and token budgets. Structural validation checks those fields and every bundled book's citations and sections. The current code does not perform cryptographic signature verification; the manifest signature is metadata at this boundary.

Only `verified`, `promoted`, and `stale` books are searchable. Draft and unverified content is hidden from retrieval. Lexical search scores terms, path, language, tags, and trust rank. Retrieval starts with bounded metadata/abstract/facts and can expand to compact or detailed text according to the model tier. A draft can be verified and promoted only after structural validation and current freshness; version drift demotes content to stale.

Instruction discovery walks from project root to target, loading compatible `.claude/` and `.agents/` entries and then `CLAUDE.md` before `AGENTS.md` at each scope. Canonicalized symlink aliases are loaded once. `AGENTS.md` has final authority at the same scope. Skill discovery similarly de-duplicates canonical paths and names across project/user locations before adding built-in `caveman` and `talk`.

## Observability and incidents

Every model turn and major runtime boundary emits a typed event. Failures also become incident records with a stable category/code, severity, retryability, action list, and correlation ID. The TUI Problems drawer renders those records; JSON `status`, `usage`, `events`, and `show` expose machine-readable evidence.

The TUI `/status` command opens a real `status · inspector` dashboard. It refreshes local session/usage, context, cache, office, account-profile, book, and problem data and can additionally append a compact status card to the transcript; the card is a secondary summary, not the inspector itself.

Important incident categories include authentication, model availability, provider rate limits, provider transport, permission denial, state persistence, configuration, interruption, and generic runtime failure. Retryability is explicit data, not an instruction to blindly repeat a failed operation.

## Security boundaries

Credential records use secret-safe `Debug` output, atomic private writes, and refresh-token retention. Provider details, command output, cache inputs, and incidents are redacted or bounded where the runtime can identify common secret forms. No local JWT signature validation is claimed. Read [the repository security policy](../SECURITY.md) for the full security reporting and boundary statement.
