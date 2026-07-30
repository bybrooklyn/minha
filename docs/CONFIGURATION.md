# Configuration

Minha configuration is TOML. It is recursively merged in this order:

1. built-in defaults;
2. the optional user file at the platform config directory's `minha/config.toml`;
3. the optional project file `<repository>/minha.toml`.

Later tables override only the fields they mention. A relative `database_path` is resolved against the project root. The project file cannot add tools, bypass the provider's model catalog check, or grant remote/destructive authority without the corresponding runtime policy and user acknowledgement.

Use [`minha.toml.example`](../minha.toml.example) as the complete typed example. The sections below describe the behavior that matters operationally.

## Core paths and mode

| Field | Default | Meaning |
| --- | --- | --- |
| `database_path` | `.minha/minha.sqlite3` | SQLite run/state database; relative paths are project-relative |
| `mode` | `interactive` | Default runtime mode when a session does not select a more specific workflow |

SQLite creates parent directories and enables WAL. Minha is pre-production: opening a prototype database whose `user_version` is not 0 or 1 atomically renames it to a timestamped `prototype-vN-*.bak` and creates a clean v1 store. No prototype data is silently translated. Do not copy `.minha/minha.sqlite3` into worker lanes or commit it.

## `[models]`

Model fields are strings because the provider catalog is the authority. The defaults are:

```toml
[models]
planner = "gpt-5.6-luna"
worker_fast = "gpt-5.3-codex-spark"
lead = "gpt-5.6-luna"
complex_lead = "gpt-5.6-terra"
manager = "gpt-5.4-mini"
worker_medium = "gpt-5.4-mini"
worker_deep = "gpt-5.6-luna"
consult_ambiguous = "gpt-5.6-terra"
consult_high_risk = "gpt-5.6-sol"
reasoning_effort = "medium"
```

These names identify role defaults only. They do not grant access. Run `minha models --json` to inspect the account's discovered catalog. The runtime fails a selected workflow when a required exact slug is absent instead of silently downgrading it.

The typed role labels are Spark, Luna, Terra, and Sol. Routine work starts with Luna, important or failed work may escalate to Terra, and Sol is reserved for critical/high-risk work. `manager` remains accepted for configuration compatibility, but manager rollups are deterministic and do not spend a model turn.

## `[scheduler]`

```toml
[scheduler]
max_agents = 8
hard_max_agents = 16
usage_reserve_percent = 12.0
question_policy = "only_blocking"
```

`max_agents` and `hard_max_agents` are bounded to 1–16. Delegation is evidence-triggered: small/coherent tasks use one focused lead lane, while only independent, path-disjoint work expands. Balanced requires an estimated 25% speed benefit with at most 15% coordination overhead; Turbo can use `hard_max_agents` for truly disjoint work. `question_policy` accepts `agent_discretion`, `only_blocking`, or `never`.

The usage reserve is a stop threshold for provider-reported account windows. At 12%, a window reaching 88% used pauses further model work. It is not a billing limit and does not redeem credits or transfer quota between accounts.

## `[context]`

```toml
[context]
# context_limit = 200000 # optional user ceiling
fact_limit = 24
recent_turns = 8
```

The provider catalog is authoritative when it reports a context window. The versioned fallback is 128,000 for Spark, 272,000 for GPT-5.4/5.5/5.6 and auto-review, and 1,048,576 with 393,216 (384K) maximum output metadata for DeepSeek V4. Minha protects the final five percent from routine calls and forecasts the next input plus an output allowance before compacting. An explicit legacy `context_limit` remains accepted only as a ceiling. Consumed tool output is replaced in active context with a digest and bounded evidence excerpt while the full event remains in SQLite. DeepSeek fallback pricing is dated independently and is used only for status estimates; it does not impose a billing limit.

## `[permissions]`

```toml
[permissions]
remote_writes = "ask"
destructive = "ask"
```

Permission levels are `deny`, `ask`, and `allow`. For risky `exec` calls, `destructive` and `remote_writes` are routed independently: the destructive classification consults `permissions.destructive`, while remote-write classification consults `permissions.remote_writes`. The fixed executor and read-only roles still enforce their own concrete boundaries, and a command can be denied by read-only mode or by an always-unsafe rule. The fixed model surface remains fixed regardless of these values. Secret redaction and workspace containment are always-on behavior rather than configurable opt-outs.

## `[cache]`

```toml
[cache]
enabled = true
max_bytes = 536870912
max_age_days = 30
hot_entries = 128
```

The local result cache has exact, TTL, and never classes. Keys include a namespace, request bytes, and sorted observed-input digests. Exact entries are reusable while their key inputs match; TTL entries expire by age; never entries are not read or written. Schema v1 retains cache hits, misses, writes, bypasses, byte counts, and saved input tokens across process restarts. `max_age_days` and `max_bytes` govern durable pruning. The current runtime replays only deterministic, secret-safe compaction results and never automatically replays workspace-dependent coding answers. Provider prefix caching is separate.

`/status` opens the local inspector/dashboard and refreshes those cache counters; it may also append a compact status card to the transcript. `/clean` removes expired entries, trims over-limit entries by least-recent use, and reclaims expired task leases. It does not delete transcripts, run history, profiles, source files, worktrees, or recovery patches.

## `[memory]`

```toml
[memory]
enabled = true
use_memory = true
generate = true
retrieval_limit = 5
```

Memory is local SQLite state, not an external service. `enabled` is the master configuration boundary; `use_memory` permits bounded retrieval into non-classifier agents; `generate` queues secret-filtered episodic extraction after meaningful completed or conclusively blocked runs. `retrieval_limit` is validated from 1 through 20. Project-local controls exposed by `minha memories` and `/memories` can turn use or generation off but cannot override a disabled configuration boundary. Generated memory remains advisory beneath checked-in instructions and current repository evidence.

## `[budgets]`

```toml
[budgets]
default = "balanced"
deepseek_soft_reserve_percent = 10.0
deepseek_hard_reserve_percent = 2.0
```

The built-in execution profiles are composable policy presets, not terminal token budgets:

| Profile | Soft optimization target | Default concurrency policy |
| --- | ---: | --- |
| `economy` | 25,000 | one lane, capability floor, judge required |
| `balanced` | 100,000 | up to 8 evidence-justified lanes |
| `turbo` | 1,000,000 | up to 16 truly disjoint lanes; run-scoped local YOLO |

Crossing a target changes routing and parallelism but never terminates useful work. Local YOLO does not authorize destructive commands, credential access, Git history changes, remote writes, pushes, releases, credit operations, or hard-spend bypasses. DeepSeek's live `/user/balance` value is displayed when available; the persisted high-water baseline applies the soft/hard reserve thresholds across restarts.

## `[books]`

```toml
[books]
enabled = true
```

Only the embedded registry is active in the current runtime; no external registry is fetched. Every book carries its own validated index, compact, and detailed token budgets, and the fixed `books` tool enforces a 32,000-token absolute ceiling.

## `[tui]`

```toml
[tui]
mouse = true
tool_detail = "compact"
theme = "dark"
surface_renderer = "auto"
reduced_motion = false
```

`mouse` controls terminal mouse capture. `tool_detail = "expanded"` opens new activity and diff detail; `compact` keeps it collapsed. `theme` accepts `auto`, `dark`, `light`, `ansi16`, `high_contrast`, or `no_color`; `NO_COLOR` always selects no-color rendering. `surface_renderer` accepts `auto`, `kitty`, `quadrant`, or `square`; `auto` tries supported Kitty/Ghostty raster corners before the portable quadrant and square fallbacks. `reduced_motion` lowers active redraws to one elapsed-time update per second and uses a static status marker.

## Instruction and skill precedence

Instruction discovery is scoped from the repository root toward the target path. Within a directory, compatible `.claude/` and `.agents/` instruction files and `CLAUDE.md` are loaded before `AGENTS.md`; `AGENTS.md` wins same-scope conflicts. Canonicalized symlinks load once.

Recognized skill/agent locations include:

```text
project: .minha/skills, .codex/skills, .claude/skills, .agents/skills, skills
user:    ~/.codex/skills, ~/.claude/skills, ~/.agents/skills
agents:  .minha/agents, .agents, .agents/agents, .claude/agents
```

Skill metadata is discovered eagerly; the full `SKILL.md` body is loaded only when selected. Built-in `$caveman` and `$talk` are always available unless a same-name discovered skill wins de-duplication. See [Operations](OPERATIONS.md) for practical precedence checks.
