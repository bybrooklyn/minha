# Operations and qualification

This guide describes how to operate the local harness, recover durable runs, and separate evidence from claims that require a live account or human review.

## Preflight

From the repository root:

```sh
minha doctor
minha login status
minha models --json
minha sessions --json
```

`doctor` checks the local Git repository, `rg`, merged configuration, SQLite store, and authentication state. The TUI `/doctor` view additionally reports workspace, database, schema/journal, book index, cache, and model-catalog diagnostics. A healthy local doctor result means the local prerequisites are present; it does not prove model entitlement or provider health. Every field in [`minha.toml.example`](../minha.toml.example) has an active runtime path, including context retention, memory controls, destructive/remote-write policy routing, cache bounds, theme, reduced motion, mouse, and tool-detail density.

`/status` is the TUI's real local inspector/dashboard, not just a transcript annotation. It refreshes session and lifetime usage, per-agent context/forecast/reserve data, cache entries/bytes/hits/misses/saved tokens, TODO freshness, office agents/tasks, provider state, account profiles, memory controls, indexed books, queued steering, and recorded problems. The CLI `status --json` exposes the same typed event-derived context and provider records.

For a clean local validation pass:

```sh
cargo fmt --all -- --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

The TUI provides the same common checks without asking a model to rediscover commands: `/check`, `/lint`, `/test`, `/docs`, `/security`, or `/quality all`. `/quality` detects Rust, Python, JavaScript, and Go roots; Rust linting also runs `actionlint` when a workflow directory and executable are present. Optional tools such as `cargo-audit`, `cargo-deny`, `actionlint`, or `pip-audit` are identified as skipped when unavailable; a skipped check is not a passing check.

## GitHub and self-update operations

Install and authenticate the official GitHub CLI before using GitHub-aware features:

```sh
gh auth status
minha update --check
```

The TUI `/gh` command is intentionally read-only and returns selected JSON fields instead of full human-formatted pages. Examples include `/gh repo`, `/gh issues`, `/gh pr 42`, `/gh checks 42`, `/gh runs`, and `/gh release`. The fixed `github` tool follows the same allowlist. An agent can request other `gh` commands through argv-only `exec`, but a detected remote mutation follows `permissions.remote_writes` and defaults to one-use approval.

`minha update` queries the latest release from `bybrooklyn/minha`. Use `--repo OWNER/REPOSITORY` only for a source you trust. The updater requires an exact binary name for the current target and its adjacent `.sha256` file, checks bounded file sizes, verifies the digest, and atomically replaces the executable on Unix. On Windows it leaves a verified staged executable beside the running binary and reports the path for replacement after exit. The command does not create releases, tags, or repository writes.

Common update failures are actionable: authenticate `gh` when release access is denied, verify DNS/network access when the CLI cannot reach GitHub, wait for a published asset matching the current target, or reject and report any checksum mismatch. Never bypass checksum verification.

## Login and profile operations

Direct DeepSeek credentials are managed separately from ChatGPT OAuth:

```text
minha provider add deepseek
minha provider list
minha provider test deepseek
minha provider remove deepseek
```

The add command uses a no-echo prompt and an atomic private file (mode 0600 on Unix). The test performs authenticated model-catalog and balance GETs, not a generation. Provider keys are never persisted in project TOML, SQLite, transcripts, events, board entries, or recovery artifacts. Explicitly configuring DeepSeek is the data-routing consent boundary.

`minha status` reports observed DeepSeek cost, cache-hit savings, and a conservative projection that prices each active agent's current forecast as a cache miss with its full output allowance. The model catalog exposes the dated pricing source and maximum-output metadata. These are estimates from the [official DeepSeek pricing table](https://api-docs.deepseek.com/quick_start/pricing), not provider invoices or hard spend controls.

Inspect and control durable memory without a model call:

```text
minha memories
minha memories --use false --generate false
minha memory search "parser constraint"
minha memory inspect MEMORY_ID
minha memory pin MEMORY_ID
minha memory correct MEMORY_ID "corrected fact"
minha memory delete MEMORY_ID
```

The equivalent TUI controls are `/memories`, `/memories enabled|use|generate on|off`, `/memory QUERY`, `/memory pin ID`, and `/memory delete ID`. Deletion creates a tombstone; correction creates a superseding record. Generated records never override current `AGENTS.md`, `CLAUDE.md`, checked-in documentation, or live repository evidence.

Start a device login and keep the terminal open while the provider polls:

```sh
minha login --profile personal --label "Personal"
```

With `--json`, the verification URL/code is written to stderr so stdout remains exactly one final JSON document. With `--jsonl`, the awaiting-authorization envelope is emitted as the first line and the final result as the last line. This keeps machine parsers unambiguous while still making the interactive device code visible.

Inspect and select profiles:

```sh
minha login list
minha login status
minha login use personal
minha login disable work
minha login enable work
```

Credentials live in `~/.minha/accounts/<name>.json` with a private `profiles.json` index. The active profile is tried first. Enabled profiles are loaded and expired records are refreshed before provider use. Parallel worker/audit slots distribute deterministically across enabled clients by modulo slot selection. This is rotation for parallelism, not a health-checked quota failover system. If the active profile cannot refresh before any client is usable, fix or remove that profile before retrying.

Do not paste `auth.json`, profile JSON, access tokens, refresh tokens, ID tokens, account headers, or raw provider responses into issues, transcripts, board entries, or recovery artifacts. `Debug` implementations redact secrets, but copied raw files remain sensitive.

## Running workflows

```sh
minha run "implement the requested change"
minha plan "design the migration without editing"
minha audit "find concrete regressions"
minha review "review the current diff"
```

Use `--json` for one stable result envelope or `--jsonl` for a stream of typed runtime events:

```sh
minha run --jsonl "run the smallest sufficient test pass"
minha status --json
minha events --json <RUN_UUID>
minha show --json <RUN_UUID>
```

Exit states distinguish success, pending/running, blocked or needing input, usage-paused, authentication unavailable, model unavailable, and failure. Preserve the run ID and any incident correlation ID when handing off a problem.

## Operating Issue Clarifier

An actionable but unsafe-to-guess report such as `minha run --implement "fix it"` enters `needs_input` before any workspace edit. Greetings and chat never do. In the TUI, one question is visible at a time: use Up/Down or the mouse to highlight an option, Enter to accept it, or start typing a direct free-text answer. Selecting **Other** moves the user to the same composer; **Not sure** delegates the detail without inventing evidence.

The CLI prints each durable question ID and option value. Answer one pending question positionally or preserve a whole batch's field identities:

```sh
minha answer "It happens after reopening the TUI" --run <RUN_UUID>
minha answer --run <RUN_UUID> \
  --answer goal-1=wrong \
  --answer reproduction-1=specific \
  --answer scope-1=tui
```

When the brief is ready, inspect it and reply with `minha answer confirm`, `minha answer edit`, `minha answer "keep clarifying"`, or `minha answer cancel`. Work does not start before confirmation. The TUI uses the same inline single-question picker for this decision.

Clarification survives process exit because the current meter, question batch, and brief are stored with the run. Use `minha status --json`, `minha show --json`, or TUI `/status` to inspect it. Forking copies the snapshot into the child. A confirmed brief also becomes a project decision; cancelling preserves the session but starts no agents. Transcript export includes the meter dimensions and brief.

Workspace-local text or log paths in the report may be inspected through bounded read-only tools when doing so can replace a question. Do not point intake at credentials or secret-bearing logs. Screenshot paths are retained only as evidence references; the current Issue Clarifier does not decode or inspect the image. If visual interpretation is essential, describe the visible symptom in text.

For token diagnosis, a clear request has no clarification turn. Vague requests use a compact Luna role without skill/agent bodies and with at most three questions per batch. Optional Terra advice is reserved for persistent high-impact ambiguity after two rounds. Provider usage for successful intake calls appears in `/status`; deterministic fallback questions consume no model tokens.

## Sessions and recovery

The SQLite database at `.minha/minha.sqlite3` is authoritative for run state. Useful commands are:

```sh
minha sessions --json
minha resume <RUN_UUID>
minha resume <RUN_UUID> --prompt "continue after fixing the failing test"
minha answer "use the SQLite path" --run <RUN_UUID>
minha pickup <RUN_UUID>
minha fork <RUN_UUID>
minha rename "parser migration" --run <RUN_UUID>
minha archive <RUN_UUID>
```

Use `resume --prompt` for normal continuation, `answer` for a persisted blocking question, and `pickup` for a run paused by the account reserve. `pickup` does not answer a question and returns a blocked result when input is still required.

In the TUI:

- `/retry` re-enters the same durable run, including its task graph and recovery state.
- `/retry --fresh` starts a new run with the prior goal and leaves the old transcript in history. It is a new run, not a history rewrite and not the internal cache-bypass control.
- `/fork` creates a child session with preserved history and a separate continuation.
- `/transcript [PATH]` exports the currently displayed transcript. Treat the export as potentially sensitive.

Worker branches and lanes are disposable implementation surfaces, but recovery patches are deliberately retained under `.minha/recovery/<run>/`. Do not delete them while diagnosing an integration failure. Minha does not commit, merge, push, or discard user changes automatically.

## Cache and cleanup

Inspect cache totals through the `/status` inspector/dashboard, `/context`, `/clean`, or CLI `status --json`. `/clean` is bounded and local:

1. remove expired cache entries;
2. trim the workspace cache to `cache.max_bytes` by least-recent use;
3. reclaim expired task leases;
4. refresh the TUI cache counters.

It does not delete session history, transcripts, profiles, source files, worktrees, issue-clarification state, or recovery patches. SQLite schema v1 retains clarification snapshots, agent TODOs, memory indexes, office-room/message cursors, and cache statistics. Pre-v1 prototype databases are timestamp-archived on open rather than translated.

## Auto-compaction checkpoints

Auto-compaction is predictive and runs at a model boundary when the configured context threshold is reached, when the hard window would be exceeded, or after `/compact`. The runtime builds a secret-safe observed-input manifest before persisting compaction state. When that manifest is available, each durable checkpoint stores its predecessor ID, forming a chain, and the transaction marks prior persisted messages for that run as `compacted`. The active context then contains the durable summary plus the configured recent turns.

If input filenames or contents contain recognized secret-like material, manifest creation fails closed for this persistence path. Minha may still use the in-memory compaction result to continue the turn, but it does not create the manifest-backed cache input or durable checkpoint chain for that compaction. A cached safe summary follows the same checkpoint-chain path.

## Incidents and problems

Runtime failures are typed incidents with code, severity, category, retryability, action suggestions, and a correlation ID. The TUI `/problems` drawer keeps the observed incident list for the active run. CLI `events` and `show` expose the same evidence.

| Incident | Meaning | First action |
| --- | --- | --- |
| `auth.login_required` | No usable enabled credential | `minha login` or `minha login list` |
| `auth.account_id_missing` | Token record lacks a required account identifier | Sign out and complete device login again |
| `model.unavailable` | Required configured exact slug is absent from catalog | `minha models --json`, then adjust config only if the account is entitled to a supported slug |
| `provider.rate_limited` | Provider account window reached a limit | Wait, inspect reset data, or enable another authorized profile |
| `provider.transport` | Request/stream/network failure | Check connectivity; retry/resume the durable run |
| `provider.unavailable` | Provider server failure | Retry later; preserve run state |
| `tool.permission_denied` | Read-only or command policy rejected an operation | Review role and permission policy; do not assume execution occurred |
| `state.persistence` | SQLite/store failure | Stop mutation, run `/doctor`, preserve database and correlation ID |
| `config.invalid` | Merged TOML violates typed bounds | Repair against [`minha.toml.example`](../minha.toml.example) |
| `run.interrupted` | User interruption | Resume or retry when ready |

Retry only when the incident says it is retryable and the underlying condition is understood. A retry can repeat a provider/network failure; it cannot make an unavailable model entitled or turn a denied command into an allowed one.

## Instruction and skill troubleshooting

When behavior seems inconsistent, inspect the target path and all applicable scopes. The effective order is nearest applicable scope last, with `CLAUDE.md` before `AGENTS.md` at each scope; `AGENTS.md` wins same-scope conflicts. `.agents/` and `.claude/` compatibility files are recognized, but canonicalized symlink aliases are loaded once.

Use the TUI `/skills` command to inspect metadata. A skill description may appear without its body because full bodies load lazily after `$name` selection. `$caveman` changes internal report compression, not permissions or tools. If a same-name project skill exists, inspect its source and scope before assuming the built-in is selected.

## Local evidence versus live qualification

| Claim | Local tests can establish | Live/human evidence still required |
| --- | --- | --- |
| Build correctness | Rust formatting, unit/integration tests, clippy | None for the local claim; still report environment/toolchain |
| OAuth implementation shape | Request/response parsing, redaction, file modes, mock contracts | A real device login and refresh with the intended account |
| Model routing | Exact catalog matching and failure behavior | That the intended account is entitled to each selected slug |
| Scheduler/recovery | DAG, lease, generation, snapshot/worktree, patch persistence tests | Real repository review of generated lanes and conflict handling |
| TUI behavior | Reducer/command tests and a locally launched screen | Human visual, keyboard, terminal-size, mouse, and accessibility review |
| Provider reliability | Error classification and bounded diagnostics | Network/provider-rate-limit behavior over time |
| Security | Static boundaries, redaction tests, permission tests | Threat review, host permissions, credential hygiene, and incident response |

Do not describe a local pass as provider qualification, entitlement confirmation, cross-platform support, or release readiness.

## Safe handoff checklist

Include:

- repository path and dirty/clean/unborn status;
- exact command and run ID;
- terminal state and incident code/correlation ID;
- `minha doctor`, `minha models --json`, and relevant test output;
- whether the account was a named profile and whether other profiles were enabled;
- paths to recovery patches, without attaching secrets or raw credential files.

Do not include access/refresh/ID tokens, `~/.minha/accounts/*.json`, raw authorization headers, private source content, or unredacted provider payloads.
