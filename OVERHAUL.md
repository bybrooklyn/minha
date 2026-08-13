# Minha Overhaul — consolidated roadmap

Merges `audit.md` (historical bug ledger + gap analysis), `polish.md` (opencode/Codex
CLI/Claude Code feel research), and `TODO.md` (the locked product spec) into one
current-state-aware document. Those three files are removed as of this merge — this is now
the single source of truth. `findings.md` and `paper.md` remain separate (session bug-hunt
notes and the control-room design paper, respectively — still valid, not superseded).

Date of this merge: 2026-08-01. Research snapshots folded in: 2026-07-30 (TODO.md), 2026-07-31
(audit.md), 2026-08-01 (polish.md + this session's Operations panel / decision card /
integration approval gate work + a fresh visibility/QOL audit).

Implementation closeout, 2026-08-01: the current working slice implements the canonical
idempotent usage ledger; versioned microtask contracts and V1/V2 dispatch receipts; adaptive 90%
taper / 95% new-call pause admission; provider-neutral equal-weight deficit round-robin with
durable health/cooldown state; Xiaomi MiMo's offline adapter, credential flow, capability and
price references; typed book cards and office envelopes; fixed bounded terminal and Justfile
tools; review-only patch staging through `minha maintain --patch`; and the local TUI settings,
theme, editor, rendering, recovery, and visibility work recorded below. MiMo's live API
qualification remains blocked on credentials; the implementation does not claim a live quota
signal or live provider certification. Browser/desktop work and voice/transcription are explicitly
deferred.

**Design principle, stated explicitly because it governs everything below**: Minha keeps its
own visual identity — its own palette, its own border/theme system, its own layout language.
Research into opencode, Codex CLI, Claude Code, and other tools is mined for *interaction
patterns and behavior*, not visual style. When an item below says "adopt from opencode," it
means the mechanism (a toast that auto-dismisses, a confirm gesture, an error-copy
substitution), never opencode's colors or chrome. Anything that reads as "make Minha look like
X" has been screened out already; if you see something that still reads that way, it's a
mistake, not intent.

## Contents

1. [Status snapshot](#1-status-snapshot)
2. [Historical next — audit record superseded by closeout](#2-historical-next-superseded-by-the-current-closeout)
3. [Backlog by area](#3-backlog-by-area) (formerly TODO.md P0–P12)
4. [Historical bug ledger](#4-historical-bug-ledger) — 55 bugs, all fixed (formerly audit.md §4–9)
5. [opencode architecture deep-dive](#5-opencode-architecture-deep-dive) — reference for future features
6. [Feature ideas](#6-feature-ideas) (formerly audit.md §11)
7. [Visual-quality reference](#7-visual-quality-reference) — Rust/ratatui-specific, new this session

---

## 1. Status snapshot

### Done

- **All 55 historically-audited bugs** (security, runtime/store, provider transport, TUI,
  CLI/config) — fixed 2026-07-31, each with a regression test. Full ledger in §4.
- **Command registry + keymap unification** (formerly TODO P1/P2, audit 2.3, polish §3, top
  priority everywhere): one 48-entry `commands.rs` registry feeds slash completion, `Ctrl-P`,
  help, and dispatch — no more drift between three separate lists. `/` opens reactively with
  live fuzzy-ranked filtering (Exact/Prefix/AliasPrefix/Substring/Fuzzy/Description tiers).
  `keymap.rs` resolves editor bindings through one table with terminal-quirk normalization
  (raw control bytes, stray `SHIFT` bits), macOS `Cmd`-chords with portable `Ctrl` fallbacks
  always available. Adds `/provider`, `/route`.
- **Editor keys**: `Ctrl-A/E/K/U/W`, `Cmd-Delete`-to-line-start on macOS (`Ctrl-U` portable
  fallback), whole-line delete (`Ctrl-X`) as a distinct action.
- **Mina rename**: Luna → Mina as the one consistent leader identity across all ~9 previously
  disjoint "lead" role strings (session lead, clarifier, planner, integrator, auditor, repair,
  conversation). Terra/Sol/Spark unchanged. `model_label` now keys off role text, not model
  slug (fixed the "Codex" mislabel bug).
- **Provider independence and fair routing** (formerly TODO P11): reserve tracking is
  provider-keyed with backward-compatible config migration; typed role policy and degraded-route
  explanations remain visible; eligibility filters capability, role, reserve, and health; then
  equal-weight WDRR selects rather than price ranking. Fair work is normalized as input + cached
  input / 4 + output × 4 + reasoning output × 4. Pins only narrow an eligible pool; explicit
  reserve/cooldown overrides do not alter safety. The store preserves route fairness state,
  idempotent settlement, complete cancellation rollback, bounded provider cooldowns, and an
  `unknown` telemetry state. DeepSeek Pro remains excluded from automatic worker routing.
- **Hive/office coordination**: `hive send`'s false-delivery bug fixed (roster-backed
  recipient resolution replaces the dead `"manager"` default); new `roster` action; `room` is
  now a real enforced delivery scope; `kind` validated per action; Coordination/Board views
  resolve identities instead of showing raw UUIDs.
- **Kitty overlay vertical-centering bug** (polish §1, was the confirmed top bug): fixed by
  deduplicating overlay dimensions into one `overlay_size()` function shared by the real widget
  and the kitty raster backdrop, closing the drift class entirely rather than patching one call
  site.
- **Operations panel** (formerly TODO P3's right operations panel, P8's Usage panel, P11's
  routing observability): the drawer is opt-in on wide terminals and remembered per width class
  for the session (not yet persisted across restarts — see §2). `Shift-Tab` is a plain
  show/hide toggle now, not a tab-cycle-then-close. Two new tabs:
  **Route** (wires the previously-fully-dead `RuntimeEvent::RoutingDecision`) and **Usage**
  (surfaces already-computed token/context/balance data, previously only visible in the Status
  overlay).
- **Inline decision cards** (formerly TODO P4, audit's "only the container is missing," the
  single most-repeated finding across all three source docs): the reserved-but-always-zero
  `question_height` layout seam now renders real content. Exec approval, mid-run questions, and
  pre-run clarification all render as one card above the composer instead of a centered modal.
  Exec-approval defaults to "Decline" so a reflexive Enter can never arm a destructive action.
- **Integration approval gate**: new, config-gated (`scheduler.integration_approval`, default
  off) human-in-the-loop pause before Mina integrates worker output, reusing the exec-approval
  machinery end to end rather than inventing a parallel one.
- **Two real bugs found and fixed while integrating the above, not by design intent**:
  `RuntimeEvent::SessionFinished` was unconditionally clearing the very approval/question card
  it was reporting (fires for every settled outcome, including a pause — not just a true end);
  and the decision card claimed "only the command above runs" / "run the command above" even
  for the command-less integration-approval case, understating the scope of the
  highest-blast-radius approval in the app.
- **Accounting, dispatch, and provider foundation**: runtime/store accounting now shares one
  versioned idempotent usage ledger; task contracts and receipts are persisted before a worker
  call; adaptive admission tapers at 90% and pauses a new call at 95%; Xiaomi MiMo is a
  first-class provider with redacted credential storage, explicit Token Plan endpoints, dated
  local pricing references, and no fabricated remaining-quota value.
- **Drawer visibility and focused context**: the compact drawer heading always names the active
  tab at narrow widths; `/settings` opens an anchored inspector with all core sections; focused
  agents show a receipt-backed specialist card rather than inferred role prose.
- **Typed books and office traffic**: book reads state a retrieval reason and cannot silently
  escalate from index to a whole book; selected cards produce deterministic specialist identities
  and receipt source evidence. New hive writes are `OfficeEnvelopeV1`, direct messages require a
  recipient, and the `group:all` broadcast exception requires an explicit reason.
- **Fixed terminal, Justfile, and maintenance boundaries**: the versioned terminal PTY uses
  parsed/redacted output, bounded batches, literal argv-style input, and human pauses for secrets
  or privilege prompts. Justfile discovery/listing is bounded; recipe runs need one-use approval.
  `minha maintain --patch` only stages a human-supplied safe patch for review and never applies or
  publishes it.

### Explicitly deferred (named so nothing is silently dropped)

- Live MiMo API qualification (§3 P0) — the offline adapter and credential flow are built, but
  real entitlement, transport behavior, and regional Token Plan validation still need credentials.
- Human TUI qualification (§3 P9): real IME behavior, terminal/resolution/mouse/screen-reader
  coverage, job control, and accessibility review across the stated terminal matrix.
- Any browser/desktop frontend (§6.6) and voice/transcription (§6.7).
- Live MiMo credentials and regional endpoint qualification, plus real provider reserve/health
  behavior over time.

---

## 2. Historical next (superseded by the current closeout)

These were two fresh audit passes against the then-current state (post Operations-panel/
decision-card/integration-gate work), one on the newest code and one specifically hunting for
adoptable opencode "bits" per the standing design principle above. They are retained as research
and regression context, not an open implementation queue. The current closeout addresses the
machine-verifiable settings/theme, Vim, rendering, scrolling-state, paste/toast/recovery,
fairness, terminal, and Justfile slices; the remaining items are the external qualification
listed in §1 and §3 P9.

### 2.1 Visibility/QOL findings — new code

- **[Fixed this session]** Decision-card safety caption/headline/option wording all
  unconditionally assumed a command existed; wrong for the command-less integration-approval
  case. **[Fixed]** `labeled_field` didn't split on embedded `\n` before wrapping, so the
  integration-approval scope report (multi-paragraph: task list, check summary, recovery note)
  collapsed into one illegible wrapped block.
- **Drawer selection cue survives ambient-vs-interactive state, but nothing shows the
  difference visually.** `drawer_interactive()` correctly gates whether Up/Down/Enter control
  the drawer, but the `List` widgets in `draw_agents` etc. always render the highlighted-row
  cursor regardless — on a wide terminal the panel opens by ambient default with a
  highlighted-looking row that isn't actually navigable until `Shift-Tab` is pressed again, with
  no visual signal why arrow keys do nothing.
- **Tab-bar truncation has no "there are more tabs" indicator.** Adjacent to the known
  truncation issue: when the active tab is Route or Usage (off-screen in the truncated title),
  *no* tab shows as bold/active — the bar reads as "nothing selected" rather than "2 more tabs
  exist off-screen." Fixing the truncation itself (§7) also fixes this.
- **Usage tab and Status overlay disagree on what they track.** Both read the same session-token
  fields, but Usage drops `cache_write_tokens` that Status shows — the two "session tokens"
  views should agree.
- **Nit**: inconsistent hint wording — "Enter answer" (clarification card) vs "Enter answers"
  (pending-request card) for the same action.
- Mouse hit-testing on the decision card and drawer click-to-activate were checked and found
  correct — no findings there.

### 2.2 Opencode "bits" not yet adopted (ranked, mechanism only — see design principle)

1. **Large-paste collapse.** opencode replaces a paste of ≥3 lines/>150 chars with an editable
   placeholder (`[Pasted ~N lines]`) backed by the real text; Minha's `paste()` inserts raw text
   with no collapse. Composer-only change, no visual-identity impact.
2. **"Skip this version" nag pattern**, for whenever an update-check UI exists: decline once,
   persist the skip keyed to that version, only a strictly newer release re-prompts.
3. **Provider-error tone substitution**, generalized beyond opencode's single Gemini hardcode: a
   small match table for known-bad DeepSeek/provider error strings, output through the existing
   `SystemTone::Error` styling.
4. **Ephemeral single-slot toast, additive to (not replacing) the transcript log.** Minha has no
   ephemeral-notification surface at all today (§6 of the old polish.md, confirmed still true) —
   `push_system` stays the durable record; a toast would use Minha's own border/colors, single
   active slot, new replaces old, for things that don't deserve a permanent transcript line
   (clipboard-copy confirmation, "connect a provider" nudge).
5. **Click/keypress-to-expand a truncated inline message** — slots onto Problems-tab entries or
   long tool-error lines using Minha's existing overlay chrome.
6. **Crash view with a pre-filled, length-budgeted GitHub issue link.** Minha has no panic hook
   today (confirmed by grep). Self-contained addition, own error-panel colors.
7. **Recovery dialog as labeled options through the existing decision-card mechanism** — Minha's
   `PendingRequest` already has the right shape (options list, Enter/Esc); this is a content
   pattern (route `.minha/recovery/` results through it) not a new UI primitive.
8. **IME-safe double-defer on submit** — flagged as a check-and-maybe-fix, not a confirmed gap;
   Minha's composer's IME handling wasn't verified in this pass.

Screened out as "too much redesign, not adopted": opencode's leader-key (`which-key`) overlay
(competes with Minha's own `/` + `Ctrl-P`, which the audit already unified — don't duplicate);
the toast's exact visual chrome (top-right position, split-border, theme colors — only the
single-slot/timed *mechanism* is worth taking); the full crash-screen layout (only the
URL-truncation algorithm and footer-state-change idea are portable).

### 2.3 Visual findings from a historical live screenshot

Everything in §2.1/§2.2 above came from reading code or `tmux capture-pane` text dumps. This
subsection is different: the user shared an actual GUI screenshot of `minha tui` running in
Ghostty and reacted "that does NOT look good." It confirms, with direct visual evidence, several
things §3 (P3/P5) and §7 only had as prose research — and surfaces one new finding neither had.
This was explicitly not fixed in the original audit pass. The later implementation addressed the
compact header, reading rail, empty-state geometry, Help/drawer visibility, bounded theme system,
and transcript grammar. The screenshot itself remains valuable regression evidence, but any
remaining glyph, contrast, or layout claim must be revisited by a human in a real terminal rather
than inferred from this historic capture.

- **Dead space.** The welcome card (`draw_welcome`) and the empty Activity panel (`draw_agents`)
  both float in large amounts of unused black space with nothing anchoring or filling the
  layout — the welcome card in particular is a small box vertically stranded in a mostly-empty
  transcript area. This is the same complaint the original TODO.md research screenshot recorded
  ("the small modal floats in a large empty canvas") — confirming it was never addressed, since
  this session's work (Operations panel, decision cards, integration gate) touched the drawer and
  composer, not the header/welcome/empty-state layout. Falls under §3 P3 (conversation rail,
  outlines/surfaces) and P5 (consistent transcript grammar for empty/sparse states).
- **Visual flatness.** Almost entirely black/monochrome, thin single-color borders, no depth or
  accent variety — reads as a wireframe, not a finished app. This is exactly the gap §7 was
  written to close (Opaline-style theming, gradient blending, zebra-striped lists,
  `tui-tabs`-style bordered tab boxes) — none of it has been applied yet; §7 is still pure
  research.
- **Empty states have no design.** "No agents spawned." and the welcome text are the only content
  in otherwise blank panels — no secondary content, hint text, or visual treatment fills the
  space or makes the emptiness feel intentional. Related to P10's "replace prototype
  copy/placeholder empty states" item, which is also still unstarted.
- **Tab-bar truncation, now confirmed via GUI screenshot** in addition to the earlier `tmux`
  text-capture confirmation (§1, §2.1) — `activity | work | board | problems` visible,
  `route`/`usage` cut off. Same fix as already sketched in §7 (`tui-tabs` bordered boxes).
- **New — stray rendering remnants, root cause not confirmed.** Small disconnected vertical-bar
  marks are visible just outside the welcome card's right edge, at roughly the same height as the
  card's top and bottom rows. Working hypothesis, needs investigation before fixing:
  `draw_welcome` hand-builds its border with Unicode block-drawing characters (`▗ ▄ ▖ ▐ ▌ ▝ ▀ ▘`,
  visible in this session's earlier `tmux capture-pane` output) instead of ratatui's standard
  border rendering — plausible that corner/edge characters don't line up correctly under real
  terminal rendering (Ghostty) the way they appeared in the `tmux` text capture. First step for
  whoever picks this up: reproduce with `minha tui` in a real terminal (not just `tmux
  capture-pane`, which may not surface the same artifact), isolate whether it's specific to
  `draw_welcome`'s block-character approach or a wider rendering issue.
- **Minor — model-slug/persona-name mismatch.** The footer and welcome-panel model label still
  reads `gpt-5.6-luna` / `5.6-luna`, which jars against "Mina" branding used everywhere else in
  the same screen. Technically correct (it's the underlying model's own slug, not the persona
  name — deliberately out of scope for the Mina rename per §1), but worth a deliberate decision
  on whether that's the right user-facing tradeoff or whether it should read consistently as
  "Mina" too.

---

## 3. Backlog by area

Formerly TODO.md's P0–P12, reorganized with completion status. Detail preserved verbatim where
still open; condensed where superseded by what's now built.

### P0 — Xiaomi MiMo provider support — **offline foundation implemented; live qualification blocked**

Implemented: `ProviderId::XiaomiMiMo`; strict `xiaomi/mimo-v2.5[-pro]` references; a
first-class canonical-provider adapter using MiMo's documented OpenAI-compatible Chat
Completions endpoint; redacted `sk-`/`tp-` credential storage; `provider add|test|remove xiaomi`;
explicit HTTPS endpoints for Token Plan keys; provider-aware catalog/status plumbing; and static
fallback records for `mimo-v2.5` and `mimo-v2.5-pro` (1,048,576 context, 996,147 effective
boundary, 52,429 protected reserve, and 131,072 maximum output). Deprecated MiMo model names are
not admitted. The adapter does not retry an ambiguous generation POST, and fixture coverage
includes fragmented tool arguments plus secret redaction.

The static pay-as-you-go reference, dated 2026-07-15, is `$0.0036/$0.435/$0.87` per million
cache-hit/cache-miss/output tokens for Pro and `$0.0028/$0.14/$0.28` for standard. This is a
local status estimate, never a fabricated invoice, balance, or entitlement. MiMo has no supported
remaining-quota API in this integration, so the UI/CLI say so explicitly. Its live auth,
regional Token Plan endpoint behavior, actual catalog response, and live generation remain
unqualified until credentials are supplied.

### P1 — Slash commands and command accessibility — **done**

See §1. `/settings` is the anchored local-preferences inspector and `/theme` previews, validates,
imports, exports, and saves an Opaline palette. Vim remains opt-in through `/settings vim on|off`
rather than a redundant standalone command. The registry remains the one source for completion,
palette, help, and dispatch.

### P2 — Editor controls and Vim mode — **implemented locally; IME qualification remains human evidence**

The semantic keymap remains the portable default: Cmd-Delete/Ctrl-U/Ctrl-K/Ctrl-A/E/W,
whole-line delete, terminal-keyboard normalization, and portable Ctrl fallbacks. Vim is a
user-local `/settings vim on|off` preference, off by default. It has visible Insert, Normal, and
operator-pending states with bounded composer-only `h/j/k/l`, `w/b/e`, `0/$`, `i/a/I/A`, `x`,
`dd`, `D`, `C`, `yy`, `p`, `o/O`, and undo/redo behavior. Motions and edits are grapheme-safe;
Vim actions never dispatch a run. Tests cover mode transitions, bounded commands, word motions,
and insert-session undo grouping. `/` command discovery remains available in Insert mode and the
standard double-Esc safe pause remains intact.

Still external: real IME composition, key-repeat/key-release, and diverse terminal keyboard
protocol qualification.

### P3 — Layout, alignment, visual hierarchy — **implemented in code; visual matrix remains human evidence**

The header is now a compact single band with a home-relative workspace path, focused role, and
right-aligned model/mode/status; the decorative badge is gone. On wide terminals the reading rail
uses a small left gutter and a wider measure for code/tables/diffs without moving the composer
away from the transcript. The welcome/empty state is intentionally sparse rather than a floating
fake activity panel. Operations remains right-anchored, active tabs are visible even at narrow
widths, and drawer hit regions derive from the rendered rectangle. Surface, border, corner, and
no-color paths have render tests. A real terminal/monitor review is still required before making
a cross-emulator visual claim.

### P4 — Replace disruptive centered modals — **implemented where an anchored surface is viable**

Approval, question, and clarification cards remain above the composer. `/help` is now an anchored
drawer tab on wide terminals and retains a keyboard-scrollable overlay fallback where a drawer
would crowd a narrow terminal. Local/status overlays retain their bounded fallback behavior rather
than falsely claiming that every small terminal has room for a panel.

### P5 — Text rendering and consistency — **implemented bounded transcript grammar**

Assistant transcript rendering now handles paragraphs, headings, emphasis, inline/fenced code,
lists, quotes, links, tables, and horizontal rules while preserving a raw transcript option.
Code and diffs use bounded lossless lines before visual wrapping; color, ANSI-16, high-contrast,
and no-color fallbacks are exercised. CJK, Indic, emoji, combining-mark, and tab wrapping/cursor
cases have regression coverage. This is a compact renderer, not a claim of full syntax
highlighting or a substitute for human contrast review.

### P6 — Anchored scrolling — **state/navigation core implemented; animation is intentionally not claimed**

The old split scroll/auto-follow state is replaced by `ScrollState`, which keeps the offset and
follow sentinel together. It preserves an inspecting user’s position when new activity arrives,
uses page and empty-composer Home/End top/bottom navigation, and bounds rendering to the viewport
while caching the transcript layout. Reduced motion remains immediate. Time-based easing,
fade-out scrollbars, and touchpad-specific animation are not claimed as complete behavior; they
need a deliberate terminal interaction design and real terminal review rather than a synthetic
claim.

### P7 — `/settings`, themes, customization — **bounded local preferences implemented**

`/settings` is a compact anchored local-preferences inspector covering Appearance, Layout, Input
& Keybindings, Accessibility, Providers, Usage, and Advanced. Its schema-v1 settings live in the
user configuration directory, not project TOML. It persists the bounded controls: theme,
renderer, Vim, raw transcript, reduced motion, and scroll step. `/theme` supports built-in
palettes plus validated Opaline import/export, contrast reporting, live preview, and save; no
color is a runtime override and never rewrites a saved preference.

The earlier proposal to expose every possible terminal/layout control is not adopted. Mouse
capture, keyboard-protocol negotiation, animation duration, and arbitrary width editing remain
runtime/project policy or future explicitly scoped work; they are not silently implied by a local
settings file. This follows the deliberate decision to keep settings bounded rather than recreate
a general desktop preference application.

### P8 — Token/context/quota/cost accounting — **canonical ledger and fair settlement implemented**

Runtime, SQLite, CLI, TUI, and status accounting use versioned usage-ledger entries with an
idempotency key. Provider response IDs settle the same billed turn once; legacy unverified rows
are explicitly labeled rather than silently promoted. Normal turns and compactions write through
the same path, while run aggregates are retained as compatibility counters. Dated DeepSeek and
MiMo prices remain local reference estimates; MiMo quota is unavailable rather than guessed.

Fair-route settlement is now idempotent and normalized across providers; an unworked cancelled
admission rolls back the full WDRR round. Provider account telemetry is surfaced when received and
otherwise remains explicitly unknown. Still external: invoice reconciliation or plan telemetry a
provider does not expose.

### P9 — Accessibility and cross-platform qualification — **code safeguards implemented; human matrix remains**

Keyboard paths, portable Ctrl fallbacks, visible active/blocked/warning/error state, ANSI-16,
high-contrast, and no-color rendering are covered by reducer/render tests. The crash/start-failure
restoration path releases raw mode, mouse capture, bracketed paste, alternate screen, and cursor.
Narrow and wide render cases, raw transcript behavior, Unicode navigation, and mouse geometry have
machine coverage. No local test can certify screen readers, IME composition, real terminal
emulators, shell job control, mouse behavior, or contrast on actual displays. The required human
matrix remains macOS Ghostty/iTerm, Linux kitty/WezTerm/tmux, Windows Terminal/PowerShell/CMD,
and WSL.

### P10 — Onboarding and general polish — **implemented as contextual, non-wizard affordances**

The project deliberately does not add a heavy first-run wizard. The sparse welcome state, slash
discovery, contextual provider/recovery messages, paste collapse with explicit expansion, one-slot
toast behavior, panic terminal recovery, typed runtime model/usage hints, and direct Help/Settings
paths provide the intended low-friction onboarding. Model/worker displays prefer observed runtime
state rather than decorative hard-coded worker hints. Copy and visual tone remain subject to the
human terminal review in P9, not declared universally polished by snapshots.

### P11 — Fair multi-model routing and execution intensity — **implemented locally; live provider qualification remains**

The provider-neutral scheduler now filters candidates by role/capability, reserve, health, and
the explicit DeepSeek Pro worker exclusion. It applies an explicit user pin only after that
filter, then selects the remaining routes with equal-weight WDRR. Fair work is settled as input +
cached input / 4 + output × 4 + reasoning output × 4; durable state is keyed by workspace, role,
provider, and model. Admission, usage settlement, and cancellation are idempotent, and a
pre-provider cancellation restores the entire provisional round rather than aging a route.

Provider health is durable and legible: `unknown` is distinct from healthy, cooling down,
authentication-required, and unsupported. Retry-After is parsed when present; otherwise a
15-second exponential cooldown caps at five minutes. User reserve/cooldown overrides are narrow
local routing controls, never permission or credential bypasses. V2 receipts retain candidates,
health, reserve/pin/fairness evidence, and the selected route; V1 events remain compatibility
projections. The Route inspector shows this assignment evidence. What cannot be claimed locally
is real-account entitlement, reserve telemetry, or provider behavior over time.

The retained Economy/Balanced/Turbo profiles are the implementation's execution-intensity
surface: 1 / up-to-4 explicitly requested / up-to-8 genuinely disjoint lanes. They do not change
destructive permissions, credential access, reserves, tool safety, or judge mutability. A separate
five-name UI taxonomy is intentionally not added merely to duplicate those compatibility profiles.

### P12 — Delivery order — **superseded by actual delivery order taken**

Original proposed 7-slice order (interaction foundation → typed settings → usage ledger → fair
router → MiMo → rendering polish → qualification) is now stale — the actual order taken this
session diverged (interaction foundation, then Operations panel/decision cards/integration gate
ahead of settings/usage-ledger/fair-router). Superseded by whatever order is chosen from §2/§3
going forward; no need to force-fit the old sequence.

---

## 4. Historical bug ledger

**All 55 bugs below are fixed** (2026-07-31), each with a regression test. Quality gates clean
at time of fix: fmt, clippy `-D warnings`, 245 tests (now 294 after this session's additions).
Kept in full for anyone debugging a regression — every fix should still be locked in by its
named test; if one of these symptoms resurfaces, the test that should have caught it is named
inline.

| Severity | Count | IDs |
| --- | --- | --- |
| High | 4 | R-1, P-1, T-1, C-1 |
| Medium | 24 | R-2..R-8, P-2..P-9, T-2..T-7, C-2..C-4 |
| Low-Medium | 6 | R-9, R-10, P-10, C-5, C-8, C-10 |
| Low | 21 | R-11..R-13, P-11..P-13, T-8..T-16, C-6, C-7, C-9, C-11..C-13 |
| **Total** | **55** | R: 13, P: 13, T: 16, C: 13 |

Fix order was: C-1 (symlink exfiltration) → P-1/P-2 (key leak in errors) → T-1 (guaranteed TUI
panic) → R-1 (whole-run abort) → the mid-severity cluster (R-3 questions, R-4 load balancing,
P-7 device flow, P-9 balance poisoning, T-3 stuck states, T-4 paste, T-5 mouse hits) → the rest.

### Runtime and store

- **R-1 HIGH** — Lease path normalization mismatch (`tasks_overlap` vs `lease_resources`
  disagree on trimming `./`) aborted the whole run and stranded tasks in `Running`.
  `runtime.rs:2334-2342, 4733-4742, 4757-4771`.
- **R-2 MEDIUM** — Account-reserve pause preserved the failed `attempt` count, so a
  paused-then-paused-again task hit `Failed` at `max_attempts` despite never actually failing.
  `runtime.rs:2454-2463, 2607`.
- **R-3 MEDIUM** — Multiple concurrent workers asking questions: only the first `Blocked` task
  was ever resumed, the rest stayed blocked forever. `runtime.rs:2429-2452, 660-689`.
- **R-4 MEDIUM** — Per-slot client load balancing defeated by `run_agent_as` re-pooling with
  slot 0 — every agent routed to the first ChatGPT account. `runtime.rs:2897` vs `2374, 1736`.
- **R-5 MEDIUM** — Crash window between worker-lane creation and the `Running` DB update
  permanently poisoned a session (retry re-enters with identical lane names, `AlreadyExists`).
  `runtime.rs:2330-2351`, `worktree.rs:267-276`.
- **R-6 MEDIUM** — `archive_prototype_database` made the v3→v9 migration chain unreachable;
  users upgrading from dev builds opened an empty app. `store.rs:2998-3023, 235-682`. Judgment
  call: prototype DBs stay archived, the misleading in-place upgrade chain was collapsed into an
  honest fresh-bootstrap instead of repaired.
- **R-7 MEDIUM** — Hive dedup key omitted `sender_id`; distinct messages from different workers
  with the same body/kind/task/refs collapsed to one. `store.rs:1829-1832`.
- **R-8 MEDIUM** — `Harness::open` force-reattached every run to the current workspace on any
  path change, orphaning board/memory/cache entries scoped by the old workspace id.
  `runtime.rs:279-281`.
- **R-9 LOW-MED** — `try_reserve_budget` never compared against `soft_token_target`; the session
  token budget was never enforced. `runtime.rs:4452-4457`. Now enforced, activating previously
  dead warning/termination branches.
- **R-10 LOW-MED** — Tasks stuck `Running` forever after a crash resumed via `continue_session`
  (the recovery pass only lived in `run_implementation`). `runtime.rs:431-473` vs `1967-1979`.
- **R-11 LOW** — `validate_patch_paths` bypassed by binary patches (no `---`/`+++` lines to
  inspect). `executor.rs:1740-1790`.
- **R-12 LOW** — `put_office_artifact` returned a digest that could mismatch stored content on a
  dedup conflict. `store.rs:2050-2075`.
- **R-13 LOW** — Worker lanes/snapshot dirs/git worktree branches accumulated without bound.
  `runtime.rs:2329-2333, 4694-4730`.

### Provider transport

- **P-1 HIGH** — DeepSeek error paths surfaced raw, unredacted provider bodies including the
  submitted API key on auth failures. `deepseek.rs:136-138, 170-171, 220-222`.
- **P-2 MEDIUM** — `redact_provider_detail`'s marker list missed `api_key`/`key`/`sk-` forms.
  `provider.rs:577-600`.
- **P-3 MEDIUM** — `response.failed` read the wrong JSON path (top-level instead of nested
  `response.error.message`); every failure surfaced as a generic "unspecified stream error."
  `provider.rs:753-761`.
- **P-4 MEDIUM** — One malformed SSE frame aborted the entire turn and discarded accumulated
  output. `provider.rs:713`, `deepseek.rs:373`.
- **P-5 MEDIUM** — DeepSeek tool-call accumulator: index collisions on missing `index`, no reset
  on re-sent `id`/`name`, args never validated as JSON before dispatch. `deepseek.rs:434-449`.
- **P-6 MEDIUM** — 300s request timeout spanned the entire stream, killing long billed turns
  mid-stream with no recovery. `provider.rs:26-27, 443-448`, `deepseek.rs:214`.
- **P-7 MEDIUM** — Device-flow poll mapped HTTP statuses backwards vs RFC 8628.
  `auth.rs:517-529`.
- **P-8 MEDIUM** — Refresh race: concurrent runs could rotate the same refresh token, the loser
  failing spuriously despite a valid token on disk. `runtime.rs:4322-4334, 4365-4376`,
  `auth.rs:559-582`.
- **P-9 MEDIUM** — Provider-controlled balance strings (`NaN`/`inf`) poisoned the SQLite
  high-water mark and crashed preflight. `deepseek.rs:151-155`, `runtime.rs:1406-1424`,
  `store.rs:1230-1253`.
- **P-10 LOW-MED** — DeepSeek usage frame wholesale-replaced `TokenUsage` (silently zeroing
  absent fields) and mislabeled cache-miss tokens as cache-write. `deepseek.rs:384-400`.
- **P-11 LOW** — Rate-limit headers silently dropped on any format deviation (`"12.5%"`,
  ISO-8601 dates, non-lowercase booleans). `provider.rs:916-945`.
- **P-12 LOW** — `[DONE]` never set `completed` in the ChatGPT parser, discarding fully
  successful streams terminated that way. `provider.rs:710-712`.
- **P-13 LOW** — Cache secret-rejection bypassed for bare credential values (no `key=value`
  structure required). `cache.rs:404-435, 160-191`.

### TUI

- **T-1 HIGH** — `escape()` cleared input but left `input_cursor` stale → guaranteed panic on
  the next edit. `app.rs:1235`.
- **T-2 MEDIUM** — Centered modals overlapped the composer on ≤24-row terminals, hiding typed
  answers. `ui.rs:1604-1617, 1896-1909, 2568-2575`.
- **T-3 MEDIUM** — `SessionFinished`/`Error`/`TurnInterrupted` never cleared `pending_request`,
  leaving a stuck "answer required" state on a dead run. `app.rs:2006-2050`. (Note: a related,
  more specific variant of this exact bug class resurfaced and was fixed again this session —
  see §1's "two real bugs" entry. The original T-3 fix cleared pending state on *every*
  `SessionFinished`; the new fix narrows that to not clear it when the state is itself a pause
  the request belongs to.)
- **T-4 MEDIUM** — Bracketed paste never enabled; pasting multi-line text submitted partial
  messages line-by-line. `lib.rs:60-88, 1025`.
- **T-5 MEDIUM** — Drawer mouse-hit region used a fixed `terminal_width - 49` formula while the
  drawer was centered at width ≥110 — unclickable or mis-targeted on wide terminals.
  `lib.rs:1052-1061` vs `ui.rs:100-105, 121-131`.
- **T-6 MEDIUM** — `EditorLayout::new` collapsed interior blank lines in multiline input.
  `editor.rs:37-43`.
- **T-7 MEDIUM** — Resuming/forking while another run was live mixed the old run's streaming
  events into the new transcript. `lib.rs:373-387`, `app.rs:2069-2081`.
- **T-8 LOW** — `deferred_answer` was a single slot; a second queued answer overwrote the first.
  `lib.rs:143, 191-199, 301-308`.
- **T-9 LOW** — `Submission::Answer` on a `UsagePaused` run discarded the typed answer text.
  `lib.rs:284-300`.
- **T-10 LOW** — `byte_at_column` landed a click just right of a trailing wide char on the wide
  char itself instead of after it. `editor.rs:103-110`.
- **T-11 LOW** — A new `Question`/`Approval` overwrote an unanswered pending request; a Question
  during clarification orphaned the request. `app.rs:1750-1772, 1804-1822, 761-767`.
- **T-12 LOW** — `TerminalSession::start` failure path could leave the terminal in the alternate
  screen. `lib.rs:63-71`.
- **T-13 LOW** — `escape()` didn't clear `completion_items`, leaving a stale popup floating.
  `app.rs:1200-1236`.
- **T-14 LOW** — Single-candidate slash completion rewrote the entire input, dropping text after
  the cursor. `app.rs:1463-1468`. (Regressed and re-fixed this session — see §1; the new bug was
  in the fuzzy-completion rewrite's `command_token()`, not this original site.)
- **T-15 LOW** — `Submission::Start`/`Continue` silently dropped while a task was finishing, no
  feedback. `lib.rs:228-245`.
- **T-16 LOW** — Kitty `SurfaceRenderer::drop` wrote the delete-all sequence unflushed, risking
  leaked image placements across sessions. `kitty.rs:132-138`.

### CLI, config, security

- **C-1 HIGH** — Instruction discovery followed symlinks outside the workspace and fed the
  target content into every run's system prompt — secret exfiltration plus prompt injection.
  `instructions.rs:72-96`.
- **C-2 MEDIUM** — `exec` timeout was unbounded while `github`/`quality` were clamped.
  `executor.rs:350-353`.
- **C-3 MEDIUM** — `run_command` could hang forever after the child exited if it spawned a
  background process inheriting the pipes. `executor.rs:869-901`.
- **C-4 MEDIUM** — Destructive-git preflight bypass: `update-ref`/`symbolic-ref`/`replace`/
  `update-index` weren't in the denylist. `executor.rs:1668-1713`.
- **C-5 LOW-MED** — Credential temp files used predictable pid-based names, followed symlinks
  (TOCTOU), and swallowed chmod errors. `auth.rs:662-686, 808-835`.
- **C-6 LOW** — `read_files` loaded the entire file into memory before capping output.
  `executor.rs:205-212`.
- **C-7 LOW** — `apply_patch` check-then-apply TOCTOU (three separate passes). `executor.rs:283-295, 1779-1784`.
- **C-8 LOW-MED** — User-level relative `database_path` silently re-based onto the project root,
  sharing one DB file across projects. `config.rs:378-380`.
- **C-9 LOW** — `models.reasoning_effort` exempt from empty-string validation. `config.rs:420-436`.
- **C-10 LOW-MED** — `minha answer cancel` during `Collecting` didn't actually cancel — the run
  kept routing toward work. `clarify.rs:171-218`, `main.rs:749-776`, `runtime.rs:585-608`.
- **C-11 LOW** — Clarification rounds were unbounded — "not sure" forever looped, spending
  tokens per round. `runtime.rs:1110-1231`, `clarify.rs:293-345`.
- **C-12 LOW** — `--jsonl` streamed typed events then appended a differently-shaped envelope
  line with no type marker. `main.rs:725-747, 1128-1162`.
- **C-13 LOW** — Self-update checksum asset trusted from the same release with no failure path
  on ambiguous glob matches. `update.rs:215-241`, `install.sh:47-61`.

### Verified-clean (checked, found sound — kept for confidence)

Runtime/store: SQLite access properly serialized (`Arc<Mutex<Connection>>`), lease generation-
fencing correct, dependency/disjointness logic correct, patch-path traversal rejection correct.
Provider: SSE CRLF/LF split-frame handling correct across chunk boundaries, `send_get_with_retry`
bounded and never retries POSTs, cache keying unambiguous. TUI: grapheme slicing boundary-safe
throughout, `CursorSet` clamps, `EditorLayout` never divides by zero, scroll sentinel handling
correct. CLI/security: `github.rs` argv allowlist rejects injection, credential writes atomic
0600, `executor::contained` canonicalization passes traversal/symlink-escape tests, destructive-
policy one-use approval wiring correct, install.sh checksum verification fail-safe.

---

## 5. opencode architecture deep-dive

Reference material for future feature ideas (§6), not current-priority work. From a direct
second-pass research dive into the cloned `opencode/` repo, independent of the polish-focused
research in §1/§2.

**PTY subsystem** (`packages/core/src/pty.ts`, 318 lines) — the strongest available precedent for
"full persistent TTY access" (§6.1): in-memory session map, 2MB ring buffer per session, absolute
output cursor, `attach({cursor?, onData, onEnd})` with replay-then-live delivery, exited-session
retention (25-session FIFO eviction), typed `Created|Updated|Exited|Deleted` events, WebSocket
transport with 60s connect tickets. Notably: **opencode's own TUI has no in-TUI terminal view** —
PTY is consumed by other surfaces (web/console) via HTTP/WebSocket, not by the agent loop itself,
which still uses a plain `bash` tool. Rust-reachable today via `portable-pty` (wezterm, MIT,
`native_pty_system()`/`openpty()`, Windows conpty support).

**HTTP API surface** — a typed `HttpApi` with OpenAPI generation across 22 route groups
(`config, control, event, file, mcp, permission, project, provider, pty, question, session, sync,
tui, workspace, ...`). `GET /event` is an SSE typed-event subscribe; `question` group is the
steering/answer API (`GET /question`, `POST /question/:id/reply|reject`). SDKs generated from the
API; the desktop app and web console are just HTTP clients — this is the blueprint if Minha ever
builds a web/desktop frontend (§6.6), and Minha already has the typed-event half of the
prerequisite (`protocol.rs`, JSONL serialization already proven).

**Desktop shell** (`packages/desktop`, Electron) — main process runs the CLI as a background
child with process supervision; renderer is a thin webview over the local server. All logic stays
in the CLI + server; desktop is purely a wrapper.

**Agents and sessions** — typed agent `Info` schema (name/description/mode/permission/model/
steps/prompt), agents can be generated from a prompt. Session pipeline: `processor.ts` drives
provider turns, a typed busy/retry/idle status machine, `message-v2.ts` as the durable event
backbone. Worktrees are a typed, user/observer-facing concept in opencode (comparable to Minha's
`worktree.rs`, but Minha's is the execution-isolation primitive, not an observer surface).

**What this doesn't cover**: agent-facing terminal tools, batched terminal actions, Justfile
support, voice capture, self-maintenance — opencode has no precedent for any of these; they're
net-new ideas, covered in §6.

---

## 6. Feature ideas

Eight scoped ideas, independent of the polish/gap-analysis work above. Each respects Minha's
existing hard boundaries: fixed tool surface with justified prompt cost, read-only judges, no
remote writes, secrets never in SQLite/UI, user authority over anything risky.

**Closeout**: 6.1–6.4 and the review-only portion of 6.8 are implemented as one fixed-tool
slice. 6.5 is deliberately bounded to explicit human-scoped edits rather than autonomous recipe
churn. 6.6 and 6.7 are explicitly deferred.

### 6.1 Bounded process-lifetime TTY access — **implemented**

opencode's PTY subsystem (§5) is the reference shape. Minha today: `exec` runs argv-only
children with no TTY at all. Design: a `pty` module in minha-core, `TtySession{id, command, cwd,
pid, status, buffer ring, cursor}` via `portable-pty`, runtime-owned (killed on teardown), cursor-
replay attach for both TUI and observers, typed `TtyStarted/Output/Exited` events, session
metadata persisted in SQLite (raw output stays in-memory/bounded). Agent access is the open
question — see 6.3. Risks: agents typing into interactive prompts (sudo, editors) must never
auto-respond; output volume needs the ring cap + 6.2's compact summaries; orphaned-PTY cleanup on
crash/resume; resize handling with no attached observer. Effort: medium-high.

Delivered scope: one schema-v1 `terminal` tool in `minha-core`, with `start`, `observe`, `batch`,
`resize`, and `close`. PTYs are process-local; only redacted metadata lives under
`.minha/terminal-sessions-v1.json`, so restart attachment is intentionally not promised. Agents
never answer credential or privilege prompts.

### 6.2 Agent-readable terminal parser — **implemented**

The `vt100` crate (MIT, doy/vt100-rust) is built for exactly this: feed a PTY byte stream, get a
`Screen` cell grid, `contents_diff` produces compact deltas between snapshots. opencode does no
parsing at all — ships raw chunks. Design: one `vt100::Parser` per PTY session (fixed grid);
agent-facing surface returns bounded structured state (visible text, cursor position, diff since
last call, prompt-pattern heuristic hints) — this is the "observation" half paired with 6.3's
"action" half. Risks: prompt cost (must emit deltas not full screens), TUI-program churn (needs
change-rate throttling), secrets on screen (redact like existing `redact_secrets`). Effort:
low-medium.

Delivered scope: `vt100` owns a bounded parsed screen; the tool returns redacted observation
data rather than raw escape bytes. The output ring is capped in process memory.

### 6.3 Batched terminal actions — **implemented with fixed bounds**

No opencode precedent (their agent still uses plain `bash`) — this is expect/tmux-send-keys
lineage. Design: one tool call (`terminal_batch`) with ordered steps (`send`/`wait_for`/`expect`),
runtime-executed (not model-turn-per-keystroke), bounded output, a timeout fails the whole batch
with no partial resumption:

```json
{ "session": "…", "steps": [
  { "send": "cargo test -p minha-core\n" },
  { "wait_for": { "pattern": "test result:", "timeout_s": 300 } },
  { "expect": { "pattern": "0 failed" } },
  { "send": "" }
], "output_cap": 64000 }
```

Risks: pattern matching on noisy streams (echo-before-output ordering), timeouts burning a whole
call, interactive TUI programs never settling — mitigate with an 8-step cap, wall-clock cap,
6.2's stable-frame detection. Effort: medium. This is the tool that makes 6.1/6.2 worth building.

Delivered scope: `batch` accepts at most eight literal argv-style lines, has at most 60 seconds
of cumulative waits, supports an expectation, and reapplies normal command policy on every line.
Shell syntax, path escapes, credential values, nested shells, and privileged commands are
rejected.

### 6.4 Built-in Justfile support — **implemented**

`just` (casey/just) has no opencode integration at all. Minha's `github.rs` (allowlisted argv
wrapper, bounded output) is the exact pattern to mirror. Design: discovery (walk upward for
`justfile`/`.justfile`, bounded depth), `just --list --unstable` parsed into a compact recipe
index, execution routed through the existing `exec` safety preflight (recipes are arbitrary
shell — not trusted, same blast radius as `exec`). Risks: large recipe listings need a size cap;
`just` not installed → report unavailable, don't auto-install. Effort: low-medium.

Delivered scope: the fixed `just` tool searches upward only inside the workspace, lists with
`just --list`, and requires one-use explicit approval for every recipe run. Direct `exec just`
is denied so the recipe approval cannot be bypassed.

### 6.5 Agent-maintained Justfiles — **deliberately not automatic**

No new tool — ordinary `apply_patch` edits, since Minha already has that path plus read-only
judges. Design: agents propose recipe additions after observing repeated identical `exec`
commands (evidence-triggered, like delegation) or on explicit task scope ("document workflows"),
never unprompted; a `justfile validate` step (parse check, `just --fmt --check` if available)
after any patch touching a justfile. Risk: churn from agents "improving" recipes unprompted —
mitigated by the evidence-triggered gate. Effort: low.

Current policy is stricter: Minha does not propose or maintain Justfiles autonomously. An
explicitly scoped user task may use the ordinary patch path, and a human must review it.

### 6.6 Web and desktop frontend — **explicitly deferred**

opencode's blueprint (§5): local HTTP+SSE server, generated SDKs, Electron-as-wrapper. Minha has
none of this today. Phased design: (1) read-only viewer — optional localhost HTTP+SSE server
emitting the existing typed `RuntimeEvent` envelopes, bound to loopback + per-launch token,
CORS-allowlisted, no credentials on the wire; (2) control — steer/pause/answer endpoints mapping
onto existing `resume_with_answer`/interrupt paths; (3) desktop — Tauri (smaller than Electron)
embedding the same server, TUI remains primary; (4) web terminal view reusing 6.1's attach/replay
over WebSocket. Risks: attack surface (loopback server must never leak provider keys — reuse the
redaction layer), scope creep (biggest single item in this list). Effort: high overall; slice 1
alone is medium.

### 6.7 Voice-note idea capture — **explicitly deferred**

No precedent anywhere. Design: `minha note <audio>` (CLI first, `/note`-style TUI later) —
transcribe locally by default (whisper.cpp/whisper-rs; cloud optional and disclosed like external
providers), one bounded model pass extracts structured items (idea text, implied task, suggested
project affinity), stored `issue_intakes`-style and surfaced as a reviewable inbox; user
confirms/attaches before it enters the normal task plane. Idempotent via audio-file hash. Risks:
transcription cost/quality, project misattribution (default unattached), audio privacy (never in
SQLite, kept next to the source file). Effort: medium.

### 6.8 Limited self-maintenance — **review-only staging implemented**

Minha already has the primitives: recovery patches, `minha doctor`, checksum-verified
`minha update`, the completion judge. Design: a bounded `minha maintain` mode — scope limited to
fixing its own bugs, improving bundled recipes/skills/books, updating Justfiles (6.5), entirely
through existing boundaries (fixed tools, `apply_patch`, read-only judges, no schema migrations
without review, no credential access, no pushes/merges); token/turn budget like any run; changes
land as reviewable patches only, never self-merge; same quality gates required before
presentation; explicit user approval beyond patch staging. Risks: feedback loops (a buggy fix
re-fixing itself), scope creep toward "fully unsupervised" — slice it (recipes/skills first, bug
fixes later once the judge-as-gate pattern is proven). Effort: medium.

Delivered scope: `minha maintain --patch` accepts a human-supplied bounded patch, rejects
migration/credential/VCS/remote/release/protected-path content, and stages the unchanged patch
under `.minha/maintenance/`. It makes no model call and never generates, applies, commits,
pushes, or releases a patch.

---

## 7. Visual-quality reference

Rust/ratatui-specific findings from a dedicated "make it look nicer" research pass — kept
separate from §2's opencode-bits list because these are library/technique recommendations, not
opencode-derived. Consistent with the design principle at the top: these are options to evaluate
for Minha's *own* palette, not a style to copy.

- **[Opaline](https://github.com/hyperb1iss/opaline)** — a token-based theme engine built
  specifically for ratatui. 39 built-in themes, `palette → token → style → gradient` resolution
  from TOML, runtime theme switching, a contract test suite enforcing every theme implements the
  same 26 semantic tokens/13 styles/5 gradients. Directly relevant to §3 P7 (theme
  customization) if that work happens — evaluate as a dependency, or at minimum copy its
  token/contract-test design, rather than hand-rolling a theme system from zero.
- **[`tui-tabs`](https://github.com/ratatui/awesome-ratatui)** — a dedicated tab-navigation
  widget with individually bordered, rounded-corner boxes per tab. The compact drawer heading
  now preserves active-tab visibility at narrow widths; this remains a possible later visual
  affordance if individual bounded tabs become more valuable than the current title treatment.
- **Gradient blending** — Lipgloss's `Blend1D`/`Blend2D` pattern (interpolate between two colors
  across a run of cells) is portable to ratatui via manual `Color::Rgb` interpolation. Minha's
  palette is currently flat named constants (`ACTIVE`/`MUTED`/`BRIGHT`/`WARN`) — a real "wow"
  technique Minha doesn't have anywhere, worth a small trial on something low-risk (a status
  spinner or header accent) before considering it more broadly.
- **`AdaptiveColor` pattern** — not "invert for dark mode," genuinely different hand-picked
  colors per light/dark background resolved at runtime. Worth checking whether Minha's
  `no_color`/theme handling does real per-background tuning or just a flip — if the latter, this
  is a cheap correctness-of-intent improvement, not a new feature.
- **Zebra striping** — alternating subtle background tint per row in list/table widgets, for
  readability. Minha's Board/Problems/Activity list rendering doesn't do this currently; cheap,
  high-value, and purely a rendering change (no data/behavior implications).
- **Reference apps for inspiration on ratatui specifically** (not to copy visually, just to see
  what's achievable in the same library): `lazygit`, `bottom`, `gitui` — all widely regarded as
  polished, all built on ratatui.
- **Charm's Crush** (Go, same product category — agentic coding CLI): worth one specific
  behavioral note even though it's a different language/library — its diff view renders *inside*
  the permission/approval dialog itself, so approving an edit shows the actual diff content, not
  just a prose description. Directly relevant to the decision-card work in §1/§2: the current
  exec-approval card shows `command`/`reason` text only; showing an actual diff for
  `apply_patch`-triggered approvals (if any exist) would be the same upgrade Crush made.
