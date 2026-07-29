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

SQLite creates parent directories, enables WAL, and migrates forward. A database with a future schema is rejected. Do not copy `.minha/minha.sqlite3` into worker lanes or commit it unless the project explicitly wants local state tracked.

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

The typed role labels are Spark, Luna, Terra, and Sol. `manager` is a separate configurable coordination-only string. If the manager slug is absent or its optional review fails, the implementation path continues without manager directives. The manager does not inspect files or receive the worker tool surface.

## `[scheduler]`

```toml
[scheduler]
min_agents = 2
max_agents = 8
hard_max_agents = 16
usage_reserve_percent = 12.0
question_policy = "only_blocking"
```

`max_agents` and `hard_max_agents` are bounded to 1–16, with `min_agents <= max_agents <= hard_max_agents`. The current scheduler never expands beyond `max_agents`; `hard_max_agents` is a validated ceiling reserved for future/other scheduling policy. `question_policy` accepts `agent_discretion`, `only_blocking`, or `never`; the default persists only questions that block progress.

The usage reserve is a stop threshold for provider-reported account windows. At 12%, a window reaching 88% used pauses further model work. It is not a billing limit and does not redeem credits or transfer quota between accounts.

## `[context]`

```toml
[context]
compact_at_percent = 72.0
context_limit = 128000
reserve_tokens = 16384
fact_limit = 24
recent_turns = 8
```

The local estimator is conservative and model-independent. Compaction is predictive: it uses the estimated current context plus the next turn, triggers at the percentage or hard limit, summarizes older material, keeps recent turns, and leaves the reserve available. `fact_limit` is actively included in the compaction instructions, but the current runtime does not implement it as a separate persisted-fact extractor. These are not provider tokenizer guarantees.

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

The local result cache has exact, TTL, and never classes. Keys include a namespace, request bytes, and sorted observed-input digests. Exact entries are reusable while their key inputs match; TTL entries expire by age; never entries are not read or written. Durable cache entries and `cache_stats` live in SQLite schema v6; the stats retain hits, misses, writes, bypasses, byte counts, and saved input tokens across process restarts. `max_age_days` and `max_bytes` govern durable pruning. `hot_entries` bounds the connected in-memory LRU, which also has a 16 MiB process safety cap; every hot hit validates and touches the durable row before use. The current runtime uses local replay for deterministic, secret-safe compaction results and deliberately does not replay workspace-dependent coding answers. Provider prompt caching is separate and is reported through provider usage fields.

`/status` opens the local inspector/dashboard and refreshes those cache counters; it may also append a compact status card to the transcript. `/clean` removes expired entries, trims over-limit entries by least-recent use, and reclaims expired task leases. It does not delete transcripts, run history, profiles, source files, worktrees, or recovery patches.

## `[budgets]`

```toml
[budgets]
default = "balanced"
```

The built-in total-token presets are:

| Preset | Total session budget |
| --- | ---: |
| `economy` | 25,000 |
| `balanced` | 100,000 |
| `thorough` | 300,000 |
| `exhaustive` | 1,000,000 |

The selected preset is the enforced global session token budget. It does not change provider pricing or entitlement.

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
```

`mouse` controls terminal mouse capture. `tool_detail = "expanded"` opens new tool and diff cards; `compact` keeps them collapsed. Slash commands and keyboard controls remain available through the same TUI state reducer.

## Instruction and skill precedence

Instruction discovery is scoped from the repository root toward the target path. Within a directory, compatible `.claude/` and `.agents/` instruction files and `CLAUDE.md` are loaded before `AGENTS.md`; `AGENTS.md` wins same-scope conflicts. Canonicalized symlinks load once.

Recognized skill/agent locations include:

```text
project: .minha/skills, .codex/skills, .claude/skills, .agents/skills, skills
user:    ~/.codex/skills, ~/.claude/skills, ~/.agents/skills
agents:  .minha/agents, .agents, .agents/agents, .claude/agents
```

Skill metadata is discovered eagerly; the full `SKILL.md` body is loaded only when selected. Built-in `$caveman` and `$talk` are always available unless a same-name discovered skill wins de-duplication. See [Operations](OPERATIONS.md) for practical precedence checks.
