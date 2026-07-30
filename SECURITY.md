# Security

Minha is a local coding harness with permission to read, modify, and execute code in the selected repository. Its safety boundary is deliberately smaller than a general automation platform: fixed tools, workspace-contained paths, explicit account files, and no MCP or dynamic plugin loader.

## Credentials and account profiles

`minha login` uses ChatGPT Codex device authorization. Credential material may exist in two compatible locations:

```text
~/.minha/auth.json                 legacy/default record
~/.minha/accounts/profiles.json   profile metadata and active name
~/.minha/accounts/<name>.json     one OAuth record per named profile
```

Credential files contain access, refresh, and ID tokens. They are written through a temporary file, synchronized, atomically renamed, and created with mode `0600` on Unix. Profile names are restricted before being used as paths. Treat the entire `~/.minha` account directory like a password store: never commit it, paste it into prompts, or attach it to logs or issues.

JWT payloads are decoded without signature verification only to display account metadata and supply the ChatGPT account identifier. Minha does not treat those local claims as proof of authorization; the service validates the access token. A login without an account identifier is rejected.

`minha logout` removes the active profile and the compatibility record when present. Other enabled profiles remain available. Use `minha login list` and `minha login remove NAME` when auditing or deleting profile state.

DeepSeek API keys live separately in the platform user configuration directory's `minha/providers.json`. The file is atomically replaced and mode `0600` on Unix. Keys are never accepted from project TOML or written to SQLite, events, transcripts, board entries, recovery artifacts, or debug output. Explicitly configuring the provider is the data-routing consent boundary.

## Local tool boundary

The model-facing catalog is fixed in source: bounded file reads, text search, patch application, argv-based execution, blocking questions, books, structured read-only GitHub queries, bundled quality checks, and one compact hive/office tool. There is no shell-string tool, MCP connection, arbitrary tool name, plugin marketplace, or credit-redemption operation.

Paths are canonicalized beneath the workspace. Parent traversal and symlink escapes are rejected; patches reject absolute, parent, and symlink targets. Commands are passed as argv vectors without shell interpolation, pipelines, redirects, command substitution, or environment-wrapper launchers. Output, message, artifact, and provider-error sizes are bounded.

Read-only roles are denied mutation by the executor, not merely instructed to avoid it. Their permitted argv commands reject absolute or parent-traversing paths outside the workspace, canonicalize existing path arguments to catch symlink escapes, and deny command-specific escape hooks such as Git directory overrides and ripgrep preprocessors. Risky local commands and detected remote-write commands follow the configured `deny`, `ask`, or `allow` policy. In `ask` mode, approval is represented by a typed persisted event, bound to the exact approved `exec` argv, and consumed once even when the next risky call does not match. Some shell launchers and nested-agent executables remain unavailable even after approval.

The `github` convenience tool accepts only an allowlisted read query and validates owner/repository and tag values before passing an argv vector to `gh`. Auth tokens remain in GitHub CLI storage; Minha does not read or persist them. Agents needing an unsupported `gh` operation must use `exec`, where remote mutation is separately classified and defaults to a one-use user question.

Issue Clarifier runs before normal work and is read-only. Its Luna role receives only bounded `read_files`, `search`, and `github`; its optional Terra consultant receives no tools. Full skill bodies, compatible-agent bodies, and repository instruction bodies are omitted from the intake prefix. The prompt tells the clarifier not to read credential-, token-, key-, or secret-like paths, but this is defense in depth rather than a complete content classifier. Do not submit secret-bearing logs or configuration files as evidence. A screenshot path is persisted as text only and is not evidence that the model inspected the image.

`minha update` trusts release metadata returned by the authenticated `gh` CLI but does not trust asset bytes alone: it requires an exact platform asset name and matching `.sha256` asset before installing. Downloads, diagnostics, and subprocess time are bounded. Release checksums protect against accidental corruption or mismatched assets; they are not a substitute for code signing or a protected release process.

These controls do not make arbitrary repository code safe. Tests and build scripts can execute project-controlled code, and a permitted command can modify local files. Run Minha only in repositories you trust, inspect diffs and recovery patches, and do not expose an authenticated process as a public service.

## Cache, books, and durable state

SQLite under `.minha/` stores transcripts, tasks, TODOs, tool evidence, incidents, cache entries, usage, compaction checkpoints, Issue Clarifier answers, confirmed briefs, hive consumption records, and generated memory. Keep it out of version control unless the repository explicitly wants that state. `/clean` prunes cache entries and expired leases; it does not erase transcripts, credentials, issue-intake state, memory, source files, or recovery patches.

Local cache keys hash sorted observed inputs. Secret-shaped filenames and common credential markers make a result non-cacheable, and provider diagnostics apply additional redaction. These checks are defense in depth, not a general secret scanner; avoid putting credentials in source, prompts, hive messages, book drafts, or tool output.

Memory insertion rejects empty and common credential-assignment content before SQLite writes. Generated memory is scope-isolated, reviewable, supersedable, pinnable, and tombstoned rather than physically rewritten. It is advisory and cannot override checked-in instructions or current evidence. This filter is deliberately conservative, not a guarantee that arbitrary prose contains no sensitive information; review memory with `minha memory` and disable generation where repository policy requires it.

Bundled books are compile-time assets with a validated SHA-256 content digest and explicit `builtin:` trust markers. The current runtime does not fetch or cryptographically authorize external registries. Registry and signature fields in the embedded manifest are metadata, not proof of external authorization.

## Network boundary

OAuth defaults to `https://auth.openai.com`; Codex model traffic defaults to `https://chatgpt.com/backend-api/codex`; configured DeepSeek traffic defaults to `https://api.deepseek.com`. Provider diagnostics are bounded and redact token-, account-, authorization-, and cookie-shaped values. Minha has no separate telemetry path by default.

Model-catalog GET requests use bounded retries for transient transport, rate-limit, and server failures. Responses POST requests are not retried automatically because a failed response does not prove that the provider rejected the turn before processing or billing it. Provider requests also have finite total timeouts; resume the durable run explicitly after diagnosing a transport failure.

Named accounts are rotated deterministically across parallel worker slots. This is not quota pooling or a security boundary: each profile must independently be authorized, and a rate-limited or revoked account can still fail its assigned call.

## Reporting

Report vulnerabilities through [GitHub private vulnerability reporting](https://github.com/bybrooklyn/minha/security/advisories/new). Include the affected revision, a minimal reproduction, impact, operating system, and whether credentials, remote writes, recovery patches, or external endpoints are involved. Never include real tokens, account identifiers, private repository content, or an unredacted database.
