use crate::commands::{self, Availability, Category, CommandContext};
use crate::editor::EditorLayout;
use crate::settings::{self, ThemePalette, TuiSettingsV1};
use anyhow::Result;
use minha_core::books::{ManifestEntry, SignedRegistryManifest};
use minha_core::deepseek::{estimate_cost_usd, pricing_for_model};
use minha_core::mimo::{
    estimate_cost_usd as estimate_mimo_cost_usd, pricing_for_model as mimo_pricing_for_model,
};
use minha_core::office::MAX_OFFICE_SUMMARY_BYTES;
use minha_core::protocol::{
    AgentState, BoardEntryView, CatalogModel, ClarificationStatus, DispatchReceiptV1, EventAgentId,
    EventEnvelope, ExitState, IncidentView, IssueClarificationView, ItemId, PlanTask, RequestId, RunId,
    RunPhase, RuntimeEvent, TerminationReason, TodoItem,
};
use minha_core::runtime::RunKind;
use minha_core::store::{RunRecord, UsageTotals};
use ratatui::text::Line;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;
use unicode_segmentation::UnicodeSegmentation;

const INPUT_LIMIT: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum WorkMode {
    #[default]
    Auto,
    Plan,
    Implement,
    Audit,
    Review,
}

impl WorkMode {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Auto => "AUTO",
            Self::Plan => "PLAN",
            Self::Implement => "IMPLEMENT",
            Self::Audit => "AUDIT",
            Self::Review => "REVIEW",
        }
    }

    pub(crate) fn run_kind(self) -> RunKind {
        match self {
            Self::Auto => RunKind::Auto,
            Self::Implement => RunKind::Implement,
            Self::Plan => RunKind::Plan,
            Self::Audit => RunKind::Audit,
            Self::Review => RunKind::Review,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum TranscriptItem {
    User {
        text: String,
        steering: bool,
    },
    Assistant {
        item_id: ItemId,
        agent_id: EventAgentId,
        role: String,
        text: String,
        streaming: bool,
    },
    Tool {
        agent_id: EventAgentId,
        call_id: String,
        name: String,
        arguments: String,
        output: String,
        exit_code: Option<i32>,
        running: bool,
        expanded: bool,
    },
    Diff {
        path: Option<String>,
        diff: String,
        expanded: bool,
    },
    System {
        tone: SystemTone,
        text: String,
    },
    Status {
        lines: Vec<String>,
    },
    Coordination {
        room_id: String,
        sender: String,
        recipient: String,
        kind: String,
        summary: String,
    },
}

#[derive(Default)]
pub(crate) struct TranscriptLayoutCache {
    pub(crate) width: u16,
    pub(crate) revision: u64,
    pub(crate) focused_agent: Option<EventAgentId>,
    pub(crate) raw_transcript: bool,
    pub(crate) lines: Vec<Line<'static>>,
    pub(crate) builds: u64,
    pub(crate) last_viewport_lines: usize,
    pub(crate) max_scroll: u16,
}

/// Transcript navigation state.  Keeping the sentinel and the follow policy
/// together avoids the old split `scroll`/`auto_follow` state drifting apart
/// when a replay, resize, or streamed event changes the transcript height.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ScrollState {
    offset: u16,
    pub(crate) auto_follow: bool,
}

impl Default for ScrollState {
    fn default() -> Self {
        Self {
            offset: 0,
            auto_follow: true,
        }
    }
}

impl ScrollState {
    pub(crate) fn offset_for(self, max_scroll: u16) -> u16 {
        if self.auto_follow {
            max_scroll
        } else {
            self.offset.min(max_scroll)
        }
    }

    pub(crate) fn follow(&mut self) {
        self.auto_follow = true;
    }

    pub(crate) fn top(&mut self) {
        self.offset = 0;
        self.auto_follow = false;
    }

    #[cfg(test)]
    pub(crate) fn set_manual(&mut self, offset: u16) {
        self.offset = offset;
        self.auto_follow = false;
    }

    pub(crate) fn scroll_down(&mut self, amount: u16, max_scroll: u16) {
        let next = self.offset_for(max_scroll).saturating_add(amount).min(max_scroll);
        self.auto_follow = next >= max_scroll;
        self.offset = next;
    }

    pub(crate) fn scroll_up(&mut self, amount: u16, max_scroll: u16) {
        self.offset = self.offset_for(max_scroll).saturating_sub(amount);
        self.auto_follow = false;
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Toast {
    pub(crate) tone: SystemTone,
    pub(crate) text: String,
    created_at: Instant,
}

impl Toast {
    fn new(tone: SystemTone, text: impl Into<String>) -> Self {
        Self {
            tone,
            text: text.into(),
            created_at: Instant::now(),
        }
    }

    pub(crate) fn is_expired(&self) -> bool {
        self.created_at.elapsed().as_secs() >= 8
    }
}

#[derive(Clone, Debug)]
pub(crate) struct PasteSummary {
    pub(crate) lines: usize,
    pub(crate) graphemes: usize,
    pub(crate) expanded: bool,
}

/// A deliberately bounded Vim composer state machine.  It owns only local
/// editor operations; it never changes run control, approvals, or commands.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum VimMode {
    #[default]
    Insert,
    Normal,
    DeletePending,
    YankPending,
}

impl VimMode {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Insert => "INSERT",
            Self::Normal => "NORMAL",
            Self::DeletePending => "NORMAL · d",
            Self::YankPending => "NORMAL · y",
        }
    }
}

impl TranscriptItem {
    pub(crate) fn agent_id(&self) -> Option<EventAgentId> {
        match self {
            Self::Assistant { agent_id, .. } | Self::Tool { agent_id, .. } => Some(*agent_id),
            Self::User { .. }
            | Self::Diff { .. }
            | Self::System { .. }
            | Self::Status { .. }
            | Self::Coordination { .. } => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SystemTone {
    Info,
    Success,
    Warning,
    Error,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AgentView {
    pub(crate) id: EventAgentId,
    pub(crate) role: String,
    pub(crate) model: String,
    pub(crate) state: AgentState,
    pub(crate) detail: String,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AgentContextView {
    pub(crate) model: String,
    pub(crate) estimated_tokens: u64,
    pub(crate) advertised_limit: u64,
    pub(crate) effective_limit: u64,
    pub(crate) forecast_tokens: u64,
    pub(crate) output_allowance: u64,
    pub(crate) protected_reserve: u64,
    pub(crate) capability_source: String,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Diagnostic {
    pub(crate) label: String,
    pub(crate) ok: bool,
    pub(crate) detail: String,
}

/// The last `RuntimeEvent::RoutingDecision` seen this session, for the Route
/// drawer tab. See the field doc on `App::last_routing` for what this event
/// does and does not cover.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RouteView {
    pub(crate) mode: String,
    pub(crate) reason: String,
    pub(crate) provider: String,
    pub(crate) model: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PendingRequest {
    pub(crate) id: RequestId,
    pub(crate) question: String,
    pub(crate) options: Vec<String>,
    pub(crate) approval: bool,
    pub(crate) reason: Option<String>,
    pub(crate) command: Option<Vec<String>>,
}

/// What the completion popup is currently offering.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum CompletionKind {
    #[default]
    None,
    /// Slash commands, filtered through the command registry.
    Command,
    /// Workspace paths.
    Path,
}

/// One row in the completion popup.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CompletionEntry {
    /// Text substituted into the composer when this entry is accepted.
    pub(crate) value: String,
    /// Label shown in the popup, including the argument shape for commands.
    pub(crate) display: String,
    pub(crate) description: String,
    pub(crate) category: Option<Category>,
    /// Set when the command is listed but cannot run right now, and why.
    pub(crate) unavailable: Option<&'static str>,
    /// Commands that refuse to run bare are completed rather than executed.
    pub(crate) needs_argument: bool,
    /// Whether accepting and running this can reach a model or the network.
    pub(crate) network: bool,
}

impl CompletionEntry {
    fn path(value: String, description: &str) -> Self {
        Self {
            display: value.clone(),
            value,
            description: description.to_owned(),
            category: None,
            unavailable: None,
            needs_argument: false,
            network: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Overlay {
    Help,
    Keymap,
    Sessions,
    Status,
    Context,
    Books,
    Doctor,
    Recovery {
        title: String,
        detail: String,
    },
    LocalAnswer {
        question: String,
        answer: String,
    },
    Login {
        verification_uri: String,
        user_code: String,
        message: String,
    },
}

/// Terminal width at and above which the drawer defaults to visible.  Below
/// this, the fixed 48-column Operations drawer leaves too little measure for
/// a useful conversation/composer rail, so it remains an explicit overlay.
pub(crate) const WIDE_DRAWER_MIN_WIDTH: u16 = 120;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum DrawerTab {
    #[default]
    Activity,
    Work,
    Board,
    Problems,
    Route,
    Usage,
    Settings,
    Help,
}

impl DrawerTab {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Activity => "activity",
            Self::Work => "work",
            Self::Board => "board",
            Self::Problems => "problems",
            Self::Route => "route",
            Self::Usage => "usage",
            Self::Settings => "settings",
            Self::Help => "help",
        }
    }

    pub(crate) const fn position(self) -> usize {
        match self {
            Self::Activity => 1,
            Self::Work => 2,
            Self::Board => 3,
            Self::Problems => 4,
            Self::Route => 5,
            Self::Usage => 6,
            Self::Settings => 7,
            Self::Help => 8,
        }
    }

    pub(crate) const fn count() -> usize {
        8
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Submission {
    Start {
        kind: RunKind,
        text: String,
    },
    Continue {
        run_id: RunId,
        text: String,
    },
    Steer {
        run_id: RunId,
        text: String,
    },
    AgentMessage {
        run_id: RunId,
        recipient: String,
        text: String,
    },
    Answer {
        run_id: RunId,
        text: String,
    },
    Clarify {
        run_id: RunId,
        answers: Vec<(String, String)>,
    },
    Interrupt {
        run_id: RunId,
    },
    Pause {
        run_id: RunId,
    },
    Shell {
        argv: Vec<String>,
        display: String,
    },
    Quality {
        action: String,
    },
    GitHub {
        action: String,
        number: Option<u64>,
    },
    Resume {
        run_id: RunId,
    },
    Fork {
        run_id: RunId,
    },
    Rename {
        run_id: RunId,
        title: String,
    },
    Archive {
        run_id: RunId,
    },
    Compact {
        run_id: RunId,
    },
    Retry {
        run_id: RunId,
        fresh: bool,
    },
    Clean,
    Doctor,
    Login,
    ShowStatus,
    ShowBoard,
    AddNote {
        text: String,
    },
    PinBoard {
        id: String,
    },
    ResolveBoard {
        id: String,
    },
    Export {
        path: Option<PathBuf>,
    },
    RefreshSessions,
    ShowDiff,
    ShowSkills,
    ShowProviders,
    ShowMemories {
        query: Option<String>,
    },
    SetMemories {
        setting: String,
        enabled: bool,
    },
    MemoryPin {
        id: String,
    },
    MemoryDelete {
        id: String,
    },
    Quit,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AppAction {
    Quit,
    Submit,
    Newline,
    Backspace,
    Delete,
    CursorLeft,
    CursorRight,
    CursorUp,
    CursorDown,
    WordLeft,
    WordRight,
    DeleteWordBackward,
    /// Delete from the cursor back to the start of the line (`Ctrl-U`, `Cmd-Delete`).
    DeleteToLineStart,
    /// Delete from the cursor to the end of the line (`Ctrl-K`).
    DeleteToLineEnd,
    /// Delete the whole line the cursor is on, regardless of cursor position.
    DeleteLine,
    CursorHome,
    CursorEnd,
    CursorSet(usize),
    Undo,
    Redo,
    Paste(String),
    Complete,
    Escape,
    Interrupt,
    ToggleDrawer,
    ToggleDetails,
    ToggleTasks,
    HistoryPrevious,
    CommandPalette,
    Help,
    Input(char),
    ScrollUp,
    ScrollDown,
    ScrollTop,
    ScrollBottom,
    PageUp,
    PageDown,
    SelectUp,
    SelectDown,
    Activate,
    ActivateClarificationOption(usize),
    ActivateIndex(usize),
    VimInsert,
    VimAppend,
    VimInsertLineStart,
    VimAppendLineEnd,
    VimNormal,
    VimDeleteChar,
    VimDeletePending,
    VimDeleteLine,
    VimDeleteToLineEnd,
    VimChangeToLineEnd,
    VimYankPending,
    VimYankLine,
    VimPasteLine,
    VimOpenBelow,
    VimOpenAbove,
    VimMoveUp,
    VimMoveDown,
    VimWordForward,
    VimWordBackward,
    VimWordEnd,
    None,
}

pub struct App {
    pub(crate) root: PathBuf,
    pub(crate) mode: WorkMode,
    pub(crate) input: String,
    pub(crate) input_cursor: usize,
    pub(crate) completion_items: Vec<CompletionEntry>,
    pub(crate) completion_kind: CompletionKind,
    /// Scroll offset for text overlays (help, keymap) that outgrow their modal.
    pub(crate) overlay_scroll: u16,
    /// Largest useful `overlay_scroll`, published by the renderer once it knows
    /// how many lines it produced and how tall the modal is.
    pub(crate) overlay_scroll_max: Cell<u16>,
    pub(crate) selected_completion: usize,
    /// Draft stashed when `Ctrl-P` replaces the composer with `/`, restored on Esc.
    pub(crate) command_draft: Option<String>,
    pub(crate) items: Vec<TranscriptItem>,
    pub(crate) transcript_revision: u64,
    pub(crate) transcript_layout: RefCell<TranscriptLayoutCache>,
    pub(crate) composer_inner_width: Cell<usize>,
    pub(crate) agents: Vec<AgentView>,
    pub(crate) contexts: HashMap<EventAgentId, AgentContextView>,
    pub(crate) plan: Vec<PlanTask>,
    pub(crate) todos: HashMap<EventAgentId, Vec<TodoItem>>,
    pub(crate) todo_active: u64,
    pub(crate) todo_blocked: u64,
    pub(crate) todo_completed: u64,
    pub(crate) todo_stale_agents: u64,
    pub(crate) todo_active_goals: Vec<String>,
    pub(crate) todo_blocked_work: Vec<String>,
    pub(crate) todo_recently_completed: Vec<String>,
    pub(crate) board: Vec<BoardEntryView>,
    pub(crate) catalog: Vec<CatalogModel>,
    pub(crate) library: Vec<ManifestEntry>,
    pub(crate) diagnostics: Vec<Diagnostic>,
    pub(crate) sessions: Vec<RunRecord>,
    pub(crate) selected_session: usize,
    pub(crate) selected_agent: usize,
    pub(crate) selected_task: usize,
    pub(crate) selected_board: usize,
    pub(crate) selected_problem: usize,
    pub(crate) selected_book: usize,
    pub(crate) focused_agent: Option<EventAgentId>,
    pub(crate) drawer_visible: bool,
    /// The user's explicit Shift-Tab/tab-command choice for wide terminals
    /// (`>= 120` cols), independent of the choice remembered for narrower
    /// ones. `None` means no explicit choice yet this session; drawers begin
    /// hidden so an idle conversation never opens as a mostly empty dashboard.
    pub(crate) drawer_override_wide: Option<bool>,
    pub(crate) drawer_override_narrow: Option<bool>,
    /// Set by `sync_drawer_visibility` each frame; read by `set_drawer_visible`
    /// so a manual toggle knows which width class's override to record.
    pub(crate) last_terminal_width: u16,
    pub(crate) drawer_tab: DrawerTab,
    pub(crate) tasks_visible: bool,
    pub(crate) details_expanded: bool,
    pub(crate) overlay: Option<Overlay>,
    pub(crate) pending_request: Option<PendingRequest>,
    pub(crate) clarification: Option<IssueClarificationView>,
    pub(crate) clarification_answers: Vec<(String, String)>,
    pub(crate) selected_clarification_question: usize,
    pub(crate) selected_clarification_option: usize,
    pub(crate) clarification_note_open: bool,
    pub(crate) active_run: Option<RunId>,
    pub(crate) running: bool,
    pub(crate) state: ExitState,
    /// The typed stop cause remains visible after a later `SessionFinished`
    /// projection reports the generic `UsagePaused` state.
    pub(crate) termination_reason: Option<TerminationReason>,
    pub(crate) model: String,
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) cached_input_tokens: u64,
    pub(crate) cache_write_tokens: u64,
    pub(crate) reasoning_output_tokens: u64,
    pub(crate) deepseek_estimated_usd: f64,
    pub(crate) deepseek_cache_savings_usd: f64,
    pub(crate) mimo_estimated_usd: f64,
    pub(crate) mimo_cache_savings_usd: f64,
    pub(crate) deepseek_balance: Option<String>,
    pub(crate) deepseek_reserve_percent: Option<f64>,
    /// Which provider the two fields above actually describe. The backend
    /// event (`RuntimeEvent::ProviderBalance`) carries this per-provider, but
    /// only one balance is remembered at a time, so a second provider's
    /// balance would otherwise silently overwrite the fields while the label
    /// still said "DeepSeek" — name it honestly instead.
    pub(crate) balance_provider: String,
    /// The last `RuntimeEvent::RoutingDecision`, if any has arrived this
    /// session. This event only covers the `/auto` mode classification
    /// (chat/implement/plan/audit/review) — it is not per-agent, per-task
    /// routing, so the Route tab must present it as "last known decision,"
    /// not a live per-agent picture.
    pub(crate) last_routing: Option<RouteView>,
    /// Durable per-assignment explanations. Unlike `last_routing`, these are
    /// emitted at the actual worker dispatch boundary.
    pub(crate) dispatch_receipts: Vec<DispatchReceiptV1>,
    /// The most recent `RuntimeEvent::Warning` message, shown alongside the
    /// route as a catch-all "recent notice" — this is how a degraded leader
    /// fallback (`LeadRoute::degraded`, runtime.rs) currently surfaces, since
    /// it is not yet its own structured event.
    pub(crate) last_warning: Option<String>,
    pub(crate) lifetime_input_tokens: u64,
    pub(crate) lifetime_output_tokens: u64,
    pub(crate) lifetime_cached_input_tokens: u64,
    pub(crate) lifetime_cache_write_tokens: u64,
    pub(crate) lifetime_reasoning_output_tokens: u64,
    pub(crate) cache_entries: u64,
    pub(crate) cache_bytes: u64,
    pub(crate) cache_hits: u64,
    pub(crate) cache_misses: u64,
    pub(crate) cache_saved_tokens: u64,
    pub(crate) indexed_books: u64,
    pub(crate) stale_books: u64,
    pub(crate) active_office_agents: u64,
    pub(crate) open_office_tasks: u64,
    pub(crate) blocked_office_tasks: u64,
    pub(crate) manager_consultations: u64,
    pub(crate) account_profiles: usize,
    pub(crate) active_account: Option<String>,
    /// The latest provider-supplied, read-only quota windows.  Runtime events
    /// carry this as JSON for protocol compatibility, but the TUI keeps the
    /// typed form so the Usage panel never has to guess at opaque fields.
    pub(crate) account_usage: Vec<minha_core::usage::RateLimitSnapshot>,
    pub(crate) incidents: Vec<IncidentView>,
    pub(crate) current_context_tokens: u64,
    pub(crate) compaction_count: u64,
    pub(crate) compact_at_tokens: u64,
    pub(crate) context_limit: u64,
    pub(crate) queued_steering: usize,
    pub(crate) message_target: Option<String>,
    pub(crate) scroll_state: ScrollState,
    pub(crate) status: String,
    pub(crate) phase: RunPhase,
    pub(crate) phase_started_at: Instant,
    pub(crate) theme: String,
    /// The persisted document is user-local (`~/.config/minha/...`), never a
    /// project TOML.  `theme` above can temporarily differ while previewing.
    pub(crate) tui_settings: TuiSettingsV1,
    pub(crate) preview_settings: Option<TuiSettingsV1>,
    pub(crate) theme_palette: ThemePalette,
    pub(crate) no_color_forced: bool,
    pub(crate) settings_path: Option<PathBuf>,
    pub(crate) settings_dirty: bool,
    pub(crate) settings_notice: Option<String>,
    pub(crate) surface_renderer: String,
    pub(crate) active_surface_renderer: String,
    surface_renderer_reload: bool,
    pub(crate) reduced_motion: bool,
    pub(crate) paste_summary: Option<PasteSummary>,
    pub(crate) toast: Option<Toast>,
    pub(crate) vim_mode: VimMode,
    vim_yank: Option<String>,
    /// True once the current Vim Insert session has captured its pre-edit
    /// snapshot.  It resets on Normal mode, so one undo reverts a contiguous
    /// Insert session without changing Standard-mode undo behavior.
    vim_insert_undo_grouped: bool,
    pub(crate) last_sequence: HashMap<RunId, u64>,
    history: Vec<String>,
    history_cursor: Option<usize>,
    history_draft: Option<String>,
    preferred_column: Option<usize>,
    undo_stack: Vec<(String, usize)>,
    redo_stack: Vec<(String, usize)>,
    last_escape_at: Option<Instant>,
    submission: Option<Submission>,
}

impl App {
    pub fn new(root: PathBuf, context_limit: u64) -> Self {
        Self {
            root,
            mode: WorkMode::Auto,
            input: String::new(),
            input_cursor: 0,
            completion_items: Vec::new(),
            completion_kind: CompletionKind::None,
            overlay_scroll: 0,
            overlay_scroll_max: Cell::new(0),
            selected_completion: 0,
            command_draft: None,
            items: Vec::new(),
            transcript_revision: 1,
            transcript_layout: RefCell::new(TranscriptLayoutCache::default()),
            composer_inner_width: Cell::new(80),
            agents: Vec::new(),
            contexts: HashMap::new(),
            plan: Vec::new(),
            todos: HashMap::new(),
            todo_active: 0,
            todo_blocked: 0,
            todo_completed: 0,
            todo_stale_agents: 0,
            todo_active_goals: Vec::new(),
            todo_blocked_work: Vec::new(),
            todo_recently_completed: Vec::new(),
            board: Vec::new(),
            catalog: Vec::new(),
            library: SignedRegistryManifest::bundled()
                .map(|manifest| manifest.packs.into_iter().flat_map(|pack| pack.entries).collect())
                .unwrap_or_default(),
            diagnostics: Vec::new(),
            sessions: Vec::new(),
            selected_session: 0,
            selected_agent: 0,
            selected_task: 0,
            selected_board: 0,
            selected_problem: 0,
            selected_book: 0,
            focused_agent: None,
            drawer_visible: false,
            drawer_override_wide: None,
            drawer_override_narrow: None,
            last_terminal_width: 0,
            drawer_tab: DrawerTab::Activity,
            tasks_visible: true,
            details_expanded: false,
            overlay: None,
            pending_request: None,
            clarification: None,
            clarification_answers: Vec::new(),
            selected_clarification_question: 0,
            selected_clarification_option: 0,
            clarification_note_open: false,
            active_run: None,
            running: false,
            state: ExitState::Pending,
            termination_reason: None,
            model: "gpt-5.6-luna".into(),
            input_tokens: 0,
            output_tokens: 0,
            cached_input_tokens: 0,
            cache_write_tokens: 0,
            reasoning_output_tokens: 0,
            deepseek_estimated_usd: 0.0,
            deepseek_cache_savings_usd: 0.0,
            mimo_estimated_usd: 0.0,
            mimo_cache_savings_usd: 0.0,
            deepseek_balance: None,
            deepseek_reserve_percent: None,
            balance_provider: String::new(),
            last_routing: None,
            dispatch_receipts: Vec::new(),
            last_warning: None,
            lifetime_input_tokens: 0,
            lifetime_output_tokens: 0,
            lifetime_cached_input_tokens: 0,
            lifetime_cache_write_tokens: 0,
            lifetime_reasoning_output_tokens: 0,
            cache_entries: 0,
            cache_bytes: 0,
            cache_hits: 0,
            cache_misses: 0,
            cache_saved_tokens: 0,
            indexed_books: 0,
            stale_books: 0,
            active_office_agents: 0,
            open_office_tasks: 0,
            blocked_office_tasks: 0,
            manager_consultations: 0,
            account_profiles: 0,
            active_account: None,
            account_usage: Vec::new(),
            incidents: Vec::new(),
            current_context_tokens: 0,
            compaction_count: 0,
            compact_at_tokens: (context_limit as f64 * 0.72) as u64,
            context_limit: context_limit.max(1),
            queued_steering: 0,
            message_target: None,
            scroll_state: ScrollState::default(),
            status: "ready".into(),
            phase: RunPhase::Complete,
            phase_started_at: Instant::now(),
            theme: "dark".into(),
            tui_settings: TuiSettingsV1::default(),
            preview_settings: None,
            theme_palette: ThemePalette::default_dark(),
            no_color_forced: false,
            settings_path: settings::user_settings_path(),
            settings_dirty: false,
            settings_notice: None,
            surface_renderer: "auto".into(),
            active_surface_renderer: "quadrant".into(),
            surface_renderer_reload: false,
            reduced_motion: false,
            paste_summary: None,
            toast: None,
            vim_mode: VimMode::Insert,
            vim_yank: None,
            vim_insert_undo_grouped: false,
            last_sequence: HashMap::new(),
            history: Vec::new(),
            history_cursor: None,
            history_draft: None,
            preferred_column: None,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            last_escape_at: None,
            submission: None,
        }
    }

    /// Apply the persisted *user-local* settings after the harness has
    /// supplied legacy startup defaults.  `NO_COLOR` is a runtime override:
    /// it never rewrites the user's saved choice, and it wins over previews.
    pub(crate) fn apply_tui_settings(&mut self, mut settings: TuiSettingsV1, no_color_forced: bool) {
        match settings.validate() {
            Ok(()) => {
                self.theme = settings.theme.clone();
                self.surface_renderer = settings.surface_renderer.clone();
                self.reduced_motion = settings.reduced_motion;
                self.theme_palette = settings
                    .palette()
                    .unwrap_or_else(|_| ThemePalette::default_dark());
                self.tui_settings = settings;
                self.preview_settings = None;
                self.settings_dirty = false;
                self.settings_notice = None;
            }
            Err(error) => {
                self.settings_notice = Some(format!("settings rejected: {error}"));
            }
        }
        self.no_color_forced = no_color_forced;
        self.surface_renderer_reload = true;
    }

    pub(crate) fn effective_theme(&self) -> &str {
        if self.no_color_forced {
            "no_color"
        } else {
            &self.theme
        }
    }

    pub(crate) fn is_imported_theme_active(&self) -> bool {
        !self.no_color_forced && self.theme == "imported"
    }

    pub(crate) fn canvas_rgb(&self) -> [u8; 3] {
        if self.is_imported_theme_active() {
            self.theme_palette.background
        } else {
            [5, 12, 24]
        }
    }

    pub(crate) fn vim_scroll_enabled(&self) -> bool {
        self.tui_settings.vim_scroll
    }

    pub(crate) fn vim_mode_label(&self) -> Option<&'static str> {
        self.vim_scroll_enabled().then_some(self.vim_mode.label())
    }

    pub(crate) fn vim_mode(&self) -> VimMode {
        self.vim_mode
    }

    pub(crate) fn raw_transcript_enabled(&self) -> bool {
        self.tui_settings.raw_transcript
    }

    pub(crate) fn take_surface_renderer_reload(&mut self) -> bool {
        std::mem::take(&mut self.surface_renderer_reload)
    }

    pub(crate) fn set_active_surface_renderer(&mut self, active: impl Into<String>) {
        self.active_surface_renderer = active.into();
    }

    pub(crate) fn expire_toast(&mut self) -> bool {
        if self.toast.as_ref().is_some_and(Toast::is_expired) {
            self.toast = None;
            true
        } else {
            false
        }
    }

    pub(crate) fn show_recovery(&mut self, title: impl Into<String>, detail: impl Into<String>) {
        let title = title.into();
        let detail = detail.into();
        self.toast = Some(Toast::new(SystemTone::Error, format!("{title}: {detail}")));
        self.overlay = Some(Overlay::Recovery { title, detail });
    }

    fn apply_runtime_settings(&mut self, settings: &TuiSettingsV1) -> Result<()> {
        self.theme = settings.theme.clone();
        self.surface_renderer = settings.surface_renderer.clone();
        self.reduced_motion = settings.reduced_motion;
        self.theme_palette = settings.palette()?;
        self.surface_renderer_reload = true;
        Ok(())
    }

    fn save_tui_settings(&mut self) {
        self.tui_settings.theme = self.theme.clone();
        self.tui_settings.surface_renderer = self.surface_renderer.clone();
        self.tui_settings.reduced_motion = self.reduced_motion;
        self.tui_settings.version = settings::SETTINGS_VERSION;
        match settings::save_user_settings(&self.tui_settings) {
            Ok(path) => {
                self.settings_path = Some(path.clone());
                self.settings_dirty = false;
                self.settings_notice = Some(format!("saved user-local settings to {}", path.display()));
                self.toast = Some(Toast::new(SystemTone::Success, "TUI settings saved locally"));
                self.push_system(SystemTone::Success, "TUI settings saved locally");
            }
            Err(error) => {
                self.settings_dirty = true;
                self.settings_notice = Some(format!("could not save TUI settings: {error}"));
                self.push_system(SystemTone::Error, format!("could not save TUI settings: {error}"));
            }
        }
    }

    fn commit_tui_settings(&mut self, settings: TuiSettingsV1, notice: impl Into<String>) {
        match self.apply_runtime_settings(&settings) {
            Ok(()) => {
                self.tui_settings = settings;
                self.preview_settings = None;
                self.settings_dirty = true;
                self.settings_notice = Some(notice.into());
                self.save_tui_settings();
            }
            Err(error) => self.push_system(
                SystemTone::Error,
                format!("could not apply TUI settings: {error}"),
            ),
        }
    }

    fn preview_tui_settings(&mut self, settings: TuiSettingsV1, notice: impl Into<String>) {
        match self.apply_runtime_settings(&settings) {
            Ok(()) => {
                self.preview_settings = Some(settings);
                self.settings_notice = Some(notice.into());
                self.toast = Some(Toast::new(
                    SystemTone::Info,
                    "theme preview active; use /theme apply or /theme reset",
                ));
                self.push_system(
                    SystemTone::Info,
                    "theme preview active; use /theme apply or /theme reset",
                );
            }
            Err(error) => self.push_system(
                SystemTone::Error,
                format!("could not preview TUI settings: {error}"),
            ),
        }
    }

    pub fn update(&mut self, action: AppAction) -> Result<bool> {
        // Anything that can move the cursor or change the composer text has to
        // re-derive the command list afterwards, so `/` completion can never go
        // stale. Actions that own the popup themselves (Complete, Escape,
        // Activate, SelectUp/Down) are deliberately excluded.
        let resync = matches!(
            action,
            AppAction::Input(_)
                | AppAction::Backspace
                | AppAction::Delete
                | AppAction::DeleteWordBackward
                | AppAction::DeleteToLineStart
                | AppAction::DeleteToLineEnd
                | AppAction::DeleteLine
                | AppAction::CursorLeft
                | AppAction::CursorRight
                | AppAction::CursorUp
                | AppAction::CursorDown
                | AppAction::CursorHome
                | AppAction::CursorEnd
                | AppAction::CursorSet(_)
                | AppAction::WordLeft
                | AppAction::WordRight
                | AppAction::Newline
                | AppAction::Paste(_)
                | AppAction::Undo
                | AppAction::Redo
                | AppAction::HistoryPrevious
                | AppAction::VimAppend
                | AppAction::VimInsertLineStart
                | AppAction::VimAppendLineEnd
                | AppAction::VimDeleteChar
                | AppAction::VimDeleteLine
                | AppAction::VimDeleteToLineEnd
                | AppAction::VimChangeToLineEnd
                | AppAction::VimPasteLine
                | AppAction::VimOpenBelow
                | AppAction::VimOpenAbove
                | AppAction::VimMoveUp
                | AppAction::VimMoveDown
                | AppAction::VimWordForward
                | AppAction::VimWordBackward
                | AppAction::VimWordEnd
        );
        let previous_overlay = self.overlay.clone();
        if matches!(
            action,
            AppAction::Input(_)
                | AppAction::Backspace
                | AppAction::Delete
                | AppAction::DeleteWordBackward
                | AppAction::DeleteToLineStart
                | AppAction::DeleteToLineEnd
                | AppAction::DeleteLine
                | AppAction::Newline
                | AppAction::Undo
                | AppAction::Redo
        ) {
            // A summary only represents the exact large paste that created
            // it.  As soon as the user edits, show the real composer again.
            self.paste_summary = None;
        }
        let quit = self.apply_action(action)?;
        if self.overlay != previous_overlay {
            self.overlay_scroll = 0;
        }
        if resync {
            self.sync_completion_after_edit();
        }
        Ok(quit)
    }

    fn apply_action(&mut self, action: AppAction) -> Result<bool> {
        match action {
            AppAction::Quit => return Ok(true),
            AppAction::Input(character) if !character.is_control() => {
                if self.input.len() < INPUT_LIMIT {
                    if self.has_active_clarification() {
                        self.clarification_note_open = true;
                    }
                    self.checkpoint_editor();
                    self.input.insert(self.input_cursor, character);
                    self.input_cursor += character.len_utf8();
                    self.history_cursor = None;
                    self.preferred_column = None;
                }
            }
            AppAction::Backspace => {
                if let Some((previous, _)) =
                    self.input[..self.input_cursor].grapheme_indices(true).next_back()
                {
                    self.checkpoint_editor();
                    self.input.drain(previous..self.input_cursor);
                    self.input_cursor = previous;
                }
                self.history_cursor = None;
                self.preferred_column = None;
            }
            AppAction::Delete => {
                if let Some(next_len) = self.input[self.input_cursor..]
                    .graphemes(true)
                    .next()
                    .map(str::len)
                {
                    self.checkpoint_editor();
                    self.input.drain(self.input_cursor..self.input_cursor + next_len);
                }
                self.history_cursor = None;
                self.preferred_column = None;
            }
            AppAction::CursorLeft => {
                if let Some((previous, _)) =
                    self.input[..self.input_cursor].grapheme_indices(true).next_back()
                {
                    self.input_cursor = previous;
                }
                self.preferred_column = None;
            }
            AppAction::CursorRight => {
                if let Some(next) = self.input[self.input_cursor..].graphemes(true).next() {
                    self.input_cursor += next.len();
                }
                self.preferred_column = None;
            }
            AppAction::CursorUp => self.move_cursor_vertical(-1),
            AppAction::CursorDown => self.move_cursor_vertical(1),
            AppAction::WordLeft => {
                self.input_cursor = previous_word_boundary(&self.input, self.input_cursor);
                self.preferred_column = None;
            }
            AppAction::WordRight => {
                self.input_cursor = next_word_boundary(&self.input, self.input_cursor);
                self.preferred_column = None;
            }
            AppAction::DeleteWordBackward => {
                let previous = previous_word_boundary(&self.input, self.input_cursor);
                if previous < self.input_cursor {
                    self.checkpoint_editor();
                    self.input.drain(previous..self.input_cursor);
                    self.input_cursor = previous;
                }
                self.preferred_column = None;
            }
            AppAction::DeleteToLineStart => {
                let start = self.line_start();
                if start < self.input_cursor {
                    self.checkpoint_editor();
                    self.input.drain(start..self.input_cursor);
                    self.input_cursor = start;
                }
                self.history_cursor = None;
                self.preferred_column = None;
            }
            AppAction::DeleteToLineEnd => {
                let end = self.line_end();
                if end > self.input_cursor {
                    self.checkpoint_editor();
                    self.input.drain(self.input_cursor..end);
                }
                self.history_cursor = None;
                self.preferred_column = None;
            }
            AppAction::DeleteLine => {
                let start = self.line_start();
                let end = self.line_end();
                // Take the line's own newline with it; on the last line, take the
                // preceding one instead so no blank line is left behind.
                let (start, end) = if end < self.input.len() {
                    (start, end + 1)
                } else if start > 0 {
                    (start - 1, end)
                } else {
                    (start, end)
                };
                if start < end {
                    self.checkpoint_editor();
                    self.input.drain(start..end);
                    self.input_cursor = start.min(self.input.len());
                }
                self.history_cursor = None;
                self.preferred_column = None;
            }
            AppAction::CursorHome => {
                self.input_cursor = self.line_start();
            }
            AppAction::CursorEnd => {
                self.input_cursor = self.line_end();
            }
            AppAction::CursorSet(cursor) => {
                self.input_cursor = cursor.min(self.input.len());
                self.preferred_column = None;
                self.history_cursor = None;
            }
            AppAction::Newline => {
                if self.input.len() < INPUT_LIMIT {
                    self.checkpoint_editor();
                    self.input.insert(self.input_cursor, '\n');
                    self.input_cursor += 1;
                    self.history_cursor = None;
                    self.preferred_column = None;
                }
            }
            AppAction::Paste(text) => self.paste(&text),
            AppAction::Undo => {
                self.vim_insert_undo_grouped = false;
                self.undo();
            }
            AppAction::Redo => {
                self.vim_insert_undo_grouped = false;
                self.redo();
            }
            AppAction::VimInsert => self.begin_vim_insert(false),
            AppAction::VimAppend => {
                // `a` stays on the current line at its end. Advancing over
                // the newline would silently move Insert mode to the next
                // line, which is not an append to the current one.
                if self.input_cursor < self.line_end()
                    && let Some(next) = self.input[self.input_cursor..].graphemes(true).next()
                {
                    self.input_cursor += next.len();
                }
                self.begin_vim_insert(false);
            }
            AppAction::VimInsertLineStart => {
                self.input_cursor = self.line_start();
                self.begin_vim_insert(false);
            }
            AppAction::VimAppendLineEnd => {
                self.input_cursor = self.line_end();
                self.begin_vim_insert(false);
            }
            AppAction::VimNormal => {
                self.vim_mode = VimMode::Normal;
                self.vim_insert_undo_grouped = false;
            }
            AppAction::VimDeleteChar => self.vim_delete_char(),
            AppAction::VimDeletePending => self.vim_mode = VimMode::DeletePending,
            AppAction::VimDeleteLine => self.vim_delete_line(),
            AppAction::VimDeleteToLineEnd => self.vim_delete_to_line_end(false),
            AppAction::VimChangeToLineEnd => self.vim_delete_to_line_end(true),
            AppAction::VimYankPending => self.vim_mode = VimMode::YankPending,
            AppAction::VimYankLine => self.vim_yank_line(),
            AppAction::VimPasteLine => self.vim_paste_line(),
            AppAction::VimOpenBelow => self.vim_open_line(false),
            AppAction::VimOpenAbove => self.vim_open_line(true),
            AppAction::VimMoveUp => {
                if self.input.is_empty() {
                    self.scroll_up(self.tui_settings.scroll_lines);
                } else {
                    self.move_cursor_vertical_bounded(-1);
                }
            }
            AppAction::VimMoveDown => {
                if self.input.is_empty() {
                    self.scroll_down(self.tui_settings.scroll_lines);
                } else {
                    self.move_cursor_vertical_bounded(1);
                }
            }
            AppAction::VimWordForward => {
                self.input_cursor = vim_next_word_start(&self.input, self.input_cursor);
                self.preferred_column = None;
            }
            AppAction::VimWordBackward => {
                self.input_cursor = previous_word_boundary(&self.input, self.input_cursor);
                self.preferred_column = None;
            }
            AppAction::VimWordEnd => {
                self.input_cursor = vim_word_end(&self.input, self.input_cursor);
                self.preferred_column = None;
            }
            AppAction::Complete => self.complete_input(),
            AppAction::Submit => self.submit_input(),
            AppAction::HistoryPrevious => self.history_previous(),
            AppAction::CommandPalette => self.open_command_surface(),
            AppAction::Escape => self.escape(),
            AppAction::Interrupt => {
                if let Some(run_id) = self.running.then_some(self.active_run).flatten() {
                    self.submission = Some(Submission::Interrupt { run_id });
                }
            }
            AppAction::ToggleDrawer => {
                // A plain show/hide toggle. Tab switching has its own path
                // (the /activity, /work, /board, /problems, /route, /usage
                // commands jump directly to a tab) — Shift-Tab used to cycle
                // through tabs before finally closing, which meant "toggle
                // the panel" took up to six presses to actually happen.
                self.set_drawer_visible(!self.drawer_visible);
                if self.drawer_visible && self.drawer_tab == DrawerTab::Board {
                    self.submission = Some(Submission::ShowBoard);
                }
            }
            AppAction::ToggleDetails => self.toggle_selected_item(),
            AppAction::ToggleTasks => self.tasks_visible = !self.tasks_visible,
            AppAction::Help => self.open_help(),
            AppAction::ScrollUp => self.scroll_up(self.tui_settings.scroll_lines),
            AppAction::ScrollDown => self.scroll_down(self.tui_settings.scroll_lines),
            AppAction::ScrollTop => self.scroll_state.top(),
            AppAction::ScrollBottom => self.scroll_state.follow(),
            AppAction::PageUp => self.page(-1),
            AppAction::PageDown => self.page(1),
            AppAction::SelectUp => self.select(-1),
            AppAction::SelectDown => self.select(1),
            AppAction::Activate => self.activate_selection(),
            AppAction::ActivateClarificationOption(index) => {
                let count = if self.has_active_clarification() {
                    self.clarification_option_count()
                } else {
                    self.pending_request
                        .as_ref()
                        .map_or(0, |request| request.options.len())
                };
                if count > 0 {
                    self.selected_clarification_option = index.min(count - 1);
                }
                self.activate_selection();
            }
            AppAction::ActivateIndex(index) => {
                // A visible wide drawer defaults to passive so arrow keys
                // remain transcript navigation. A pointer click is explicit
                // intent, though: promote it to interactive before applying
                // the selected row so Activity clicks never look live but do
                // nothing.
                if self.drawer_visible && !self.drawer_interactive() {
                    self.set_drawer_visible(true);
                }
                match self.drawer_tab {
                    DrawerTab::Activity => {
                        self.selected_agent = index.min(self.agents.len().saturating_sub(1))
                    }
                    DrawerTab::Work => {
                        self.selected_task = index.min(self.work_item_count().saturating_sub(1))
                    }
                    DrawerTab::Board => self.selected_board = index.min(self.board.len().saturating_sub(1)),
                    DrawerTab::Problems => {
                        self.selected_problem = index.min(self.incidents.len().saturating_sub(1));
                    }
                    // Static key/value panels, nothing to select.
                    DrawerTab::Route | DrawerTab::Usage | DrawerTab::Settings | DrawerTab::Help => {}
                }
                self.activate_selection();
            }
            AppAction::None | AppAction::Input(_) => {}
        }
        Ok(false)
    }

    fn submit_input(&mut self) {
        let text = self.input.trim().to_owned();
        if text.is_empty() && !self.has_active_clarification() && self.pending_request.is_none() {
            return;
        }
        self.clear_input();
        if !text.is_empty() && self.history.last() != Some(&text) {
            self.history.push(text.clone());
        }

        if self.has_active_clarification() {
            self.submit_clarification(text);
            return;
        }

        if let Some(command) = text.strip_prefix('/') {
            self.handle_slash(command);
            return;
        }
        if let Some(command) = text.strip_prefix('!') {
            match split_argv(command) {
                Ok(argv) if !argv.is_empty() => {
                    self.submission = Some(Submission::Shell {
                        argv,
                        display: command.trim().to_owned(),
                    });
                }
                Ok(_) => {}
                Err(error) => self.push_system(SystemTone::Error, error),
            }
            return;
        }
        if let (Some(recipient), Some(run_id)) = (self.message_target.clone(), self.active_run) {
            if text.len() > MAX_OFFICE_SUMMARY_BYTES {
                self.input = text;
                self.input_cursor = self.input.len();
                self.push_system(
                    SystemTone::Warning,
                    format!(
                        "direct messages are limited to {MAX_OFFICE_SUMMARY_BYTES} bytes so the office record stays compact"
                    ),
                );
                return;
            }
            self.message_target = None;
            self.submission = Some(Submission::AgentMessage {
                run_id,
                recipient,
                text,
            });
            return;
        }
        if self.pending_request.is_some() {
            self.submit_pending_request(text);
            return;
        }
        if self.running
            && let Some(run_id) = self.active_run
        {
            self.submission = Some(Submission::Steer { run_id, text });
            return;
        }
        self.items.push(TranscriptItem::User {
            text: text.clone(),
            steering: false,
        });
        self.invalidate_transcript_layout();
        self.running = true;
        self.state = ExitState::Running;
        self.set_phase(RunPhase::Queued);
        self.status = "queued".into();
        self.scroll_state.follow();
        self.submission = Some(match self.active_run {
            Some(run_id) => Submission::Continue { run_id, text },
            None => Submission::Start {
                kind: self.mode.run_kind(),
                text,
            },
        });
    }

    fn clarification_option_count(&self) -> usize {
        let Some(clarification) = &self.clarification else {
            return 0;
        };
        if clarification.status == ClarificationStatus::Reviewing {
            return 4;
        }
        clarification
            .pending_batch
            .as_ref()
            .and_then(|batch| batch.questions.get(self.selected_clarification_question))
            .map_or(0, |question| {
                question.options.len()
                    + usize::from(question.allow_not_sure)
                    + usize::from(question.allow_free_text)
            })
    }

    pub(crate) fn has_active_clarification(&self) -> bool {
        self.clarification.as_ref().is_some_and(|clarification| {
            matches!(
                clarification.status,
                ClarificationStatus::Collecting | ClarificationStatus::Reviewing
            )
        })
    }

    fn submit_clarification(&mut self, text: String) {
        let Some(run_id) = self.active_run else {
            return;
        };
        let Some(clarification) = self.clarification.clone() else {
            return;
        };
        if clarification.status == ClarificationStatus::Reviewing {
            let actions = ["confirm", "edit", "keep_clarifying", "cancel"];
            let normalized = text.trim().to_ascii_lowercase().replace(' ', "_");
            let selected = actions
                .get(self.selected_clarification_option.min(actions.len() - 1))
                .copied()
                .unwrap_or("confirm");
            let action = if self.clarification_note_open {
                selected.to_owned()
            } else if normalized.is_empty() {
                actions
                    .get(self.selected_clarification_option.min(actions.len() - 1))
                    .copied()
                    .unwrap_or("confirm")
                    .to_owned()
            } else if actions.contains(&normalized.as_str()) {
                normalized
            } else {
                self.submission = Some(Submission::Clarify {
                    run_id,
                    answers: vec![("$action".into(), "edit".into()), ("$edit".into(), text)],
                });
                return;
            };
            let mut answers = vec![("$action".into(), action.clone())];
            if !text.trim().is_empty() {
                answers.push((if action == "edit" { "$edit" } else { "$note" }.into(), text));
            }
            self.clarification_note_open = false;
            self.submission = Some(Submission::Clarify { run_id, answers });
            return;
        }

        let normalized_action = text
            .trim()
            .trim_start_matches('/')
            .to_ascii_lowercase()
            .replace(' ', "_");
        if matches!(
            normalized_action.as_str(),
            "best" | "best_judgment" | "use_best_judgment"
        ) {
            self.submission = Some(Submission::Clarify {
                run_id,
                answers: vec![("$action".into(), "use_best_judgment".into())],
            });
            return;
        }
        if matches!(normalized_action.as_str(), "summary" | "summarize") {
            self.submission = Some(Submission::Clarify {
                run_id,
                answers: vec![("$action".into(), "summarize".into())],
            });
            return;
        }
        if normalized_action == "cancel" {
            self.submission = Some(Submission::Clarify {
                run_id,
                answers: vec![("$action".into(), "cancel".into())],
            });
            return;
        }

        let Some(question) = clarification
            .pending_batch
            .as_ref()
            .and_then(|batch| batch.questions.get(self.selected_clarification_question))
        else {
            return;
        };
        let option_count = question.options.len();
        let not_sure_index = question.allow_not_sure.then_some(option_count);
        let other_index = question
            .allow_free_text
            .then_some(option_count + usize::from(question.allow_not_sure));
        let free_text = text.trim().to_owned();
        let answer = if !free_text.is_empty() {
            free_text
        } else if let Some(option) = question.options.get(self.selected_clarification_option) {
            option.value.clone()
        } else if Some(self.selected_clarification_option) == not_sure_index {
            "Not sure".into()
        } else if Some(self.selected_clarification_option) == other_index {
            self.clarification_note_open = true;
            return;
        } else {
            return;
        };
        self.clarification_answers.push((question.id.clone(), answer));
        self.clear_input();
        self.clarification_note_open = false;
        self.selected_clarification_question += 1;
        self.selected_clarification_option = 0;
        let question_count = clarification
            .pending_batch
            .as_ref()
            .map_or(0, |batch| batch.questions.len());
        if self.selected_clarification_question >= question_count {
            self.submission = Some(Submission::Clarify {
                run_id,
                answers: std::mem::take(&mut self.clarification_answers),
            });
        }
    }

    /// Answer a mid-run question or exec-approval request. An empty `text`
    /// means the user pressed Enter/clicked without typing — in that case the
    /// highlighted option in the inline card is the answer, matching the
    /// clarification card's arrow-key-then-Enter grammar.
    fn submit_pending_request(&mut self, text: String) {
        let Some(run_id) = self.active_run else {
            return;
        };
        let Some(request) = &self.pending_request else {
            return;
        };
        let answer = if !text.is_empty() {
            text
        } else if let Some(option) = request.options.get(self.selected_clarification_option) {
            option.clone()
        } else {
            return;
        };
        self.submission = Some(Submission::Answer { run_id, text: answer });
    }

    fn handle_slash(&mut self, command: &str) {
        let (name, args) = command
            .split_once(char::is_whitespace)
            .map_or((command, ""), |(name, args)| (name, args.trim()));
        // Every dispatchable command is in the registry, so unknown names,
        // missing arguments, and unmet preconditions are all rejected here with
        // an explanation instead of silently doing nothing.
        let Some(spec) = commands::find(name) else {
            let hint = commands::suggestion(name)
                .map(|spec| format!("; did you mean /{}?", spec.name))
                .unwrap_or_else(|| "; use /help".into());
            self.push_system(SystemTone::Warning, format!("unknown command /{name}{hint}"));
            return;
        };
        if let Availability::Unavailable(reason) = spec.availability(self.command_context()) {
            self.push_system(SystemTone::Info, format!("/{} {reason}", spec.name));
            return;
        }
        if spec.needs_argument && args.is_empty() {
            self.push_system(
                SystemTone::Info,
                format!("{} — {}", spec.display(), spec.description),
            );
            return;
        }
        let name = spec.name;
        match name {
            "new" => self.reset_session(),
            "plan" | "implement" | "audit" | "review" => {
                self.mode = match name {
                    "plan" => WorkMode::Plan,
                    "audit" => WorkMode::Audit,
                    "review" => WorkMode::Review,
                    _ => WorkMode::Implement,
                };
                if !args.is_empty() {
                    self.submission = Some(Submission::Start {
                        kind: self.mode.run_kind(),
                        text: args.into(),
                    });
                }
            }
            "resume" => {
                self.overlay = Some(Overlay::Sessions);
                self.submission = Some(Submission::RefreshSessions);
            }
            "fork" => {
                if let Some(run_id) = self.active_run {
                    self.submission = Some(Submission::Fork { run_id });
                }
            }
            "rename" => {
                if let Some(run_id) = self.active_run {
                    self.submission = Some(Submission::Rename {
                        run_id,
                        title: args.into(),
                    });
                }
            }
            "archive" => {
                if let Some(run_id) = self.active_run {
                    self.submission = Some(Submission::Archive { run_id });
                }
            }
            "diff" => self.submission = Some(Submission::ShowDiff),
            "activity" => {
                self.drawer_tab = DrawerTab::Activity;
                self.set_drawer_visible(true);
            }
            "work" => {
                self.drawer_tab = DrawerTab::Work;
                self.set_drawer_visible(true);
            }
            "to" => {
                if args.is_empty() {
                    self.message_target = None;
                    self.push_system(SystemTone::Info, "direct-message target cleared");
                } else if let Some(agent) = self
                    .agents
                    .iter()
                    .find(|agent| agent.id.to_string() == args || agent.role.eq_ignore_ascii_case(args))
                {
                    self.message_target = Some(format!("agent:{}", agent.id));
                } else {
                    self.push_system(
                        SystemTone::Warning,
                        "agent not found; use /activity to inspect current agents",
                    );
                }
            }
            "board" => {
                self.drawer_tab = DrawerTab::Board;
                self.set_drawer_visible(true);
                self.submission = Some(Submission::ShowBoard);
            }
            "problems" => {
                self.drawer_tab = DrawerTab::Problems;
                self.set_drawer_visible(true);
            }
            "route" => {
                self.drawer_tab = DrawerTab::Route;
                self.set_drawer_visible(true);
            }
            "note" => {
                self.submission = Some(Submission::AddNote { text: args.into() });
            }
            "pin" => {
                let id = if args.is_empty() {
                    self.board.get(self.selected_board).map(|entry| entry.id.clone())
                } else {
                    Some(args.into())
                };
                if let Some(id) = id {
                    self.submission = Some(Submission::PinBoard { id });
                } else {
                    self.push_system(SystemTone::Info, "select a board entry or pass its id");
                }
            }
            "resolve" => {
                let id = if args.is_empty() {
                    self.board.get(self.selected_board).map(|entry| entry.id.clone())
                } else {
                    Some(args.into())
                };
                if let Some(id) = id {
                    self.submission = Some(Submission::ResolveBoard { id });
                } else {
                    self.push_system(SystemTone::Info, "select a board entry or pass its id");
                }
            }
            "compact" => {
                if let Some(run_id) = self.active_run {
                    self.submission = Some(Submission::Compact { run_id });
                } else {
                    self.push_system(SystemTone::Info, "no active session to compact");
                }
            }
            "model" => self.push_system(
                SystemTone::Info,
                format!(
                    "lead {} · workers {}",
                    persona_model_label("Mina", &self.model),
                    self.worker_models_summary()
                ),
            ),
            "memories" => {
                let mut parts = args.split_whitespace();
                let setting = parts.next();
                let value = parts.next();
                if let (Some(setting), Some(value)) = (setting, value) {
                    if let Some(enabled) = parse_toggle(value) {
                        self.submission = Some(Submission::SetMemories {
                            setting: setting.into(),
                            enabled,
                        });
                    } else {
                        self.push_system(SystemTone::Warning, "memory control value must be on or off");
                    }
                } else {
                    self.submission = Some(Submission::ShowMemories { query: None });
                }
            }
            "memory" => {
                let (action, value) = args.split_once(char::is_whitespace).unwrap_or(("search", args));
                match action {
                    "pin" if !value.trim().is_empty() => {
                        self.submission = Some(Submission::MemoryPin {
                            id: value.trim().into(),
                        });
                    }
                    "delete" if !value.trim().is_empty() => {
                        self.submission = Some(Submission::MemoryDelete {
                            id: value.trim().into(),
                        });
                    }
                    "search" if !value.trim().is_empty() => {
                        self.submission = Some(Submission::ShowMemories {
                            query: Some(value.trim().into()),
                        });
                    }
                    _ if !args.trim().is_empty() => {
                        self.submission = Some(Submission::ShowMemories {
                            query: Some(args.trim().into()),
                        });
                    }
                    _ => self.submission = Some(Submission::ShowMemories { query: None }),
                }
            }
            "skills" => self.submission = Some(Submission::ShowSkills),
            "ask" => self.local_answer(args),
            "usage" => {
                self.drawer_tab = DrawerTab::Usage;
                self.set_drawer_visible(true);
                self.push_system(
                    SystemTone::Info,
                    format!(
                        "{} input + {} output tokens · {:.1}% context estimate",
                        self.input_tokens,
                        self.output_tokens,
                        self.context_percent()
                    ),
                );
            }
            "settings" => self.edit_settings(args),
            "status" => self.local_status(),
            "context" => self.overlay = Some(Overlay::Context),
            "doctor" => self.submission = Some(Submission::Doctor),
            "check" | "lint" | "test" | "docs" | "security" => {
                self.submission = Some(Submission::Quality { action: name.into() });
            }
            "quality" => {
                self.submission = Some(Submission::Quality {
                    action: if args.is_empty() {
                        "detect".into()
                    } else {
                        args.into()
                    },
                });
            }
            "gh" => {
                let mut parts = args.split_whitespace();
                let action = parts.next().unwrap_or("repo").to_owned();
                let number = parts.next().and_then(|value| value.parse().ok());
                self.submission = Some(Submission::GitHub { action, number });
            }
            "clean" => self.submission = Some(Submission::Clean),
            "books" => self.overlay = Some(Overlay::Books),
            "login" => self.submission = Some(Submission::Login),
            "retry" => {
                if let Some(run_id) = self.active_run {
                    self.submission = Some(Submission::Retry {
                        run_id,
                        fresh: args == "--fresh",
                    });
                } else {
                    self.push_system(SystemTone::Info, "no active session to retry");
                }
            }
            "transcript" => {
                let path = (!args.is_empty()).then(|| PathBuf::from(args));
                self.submission = Some(Submission::Export { path });
            }
            "help" => self.open_help(),
            "quit" => self.submission = Some(Submission::Quit),
            "auto" => self.mode = WorkMode::Auto,
            "provider" => self.submission = Some(Submission::ShowProviders),
            "theme" => self.set_theme(args),
            "keymap" => self.overlay = Some(Overlay::Keymap),
            // Unreachable: the registry lookup above rejects unknown names, and
            // this test proves every entry has an arm.
            _ => self.push_system(
                SystemTone::Warning,
                format!("/{name} is registered but not wired up; please report this"),
            ),
        }
    }

    fn set_theme(&mut self, args: &str) {
        let (action, value) = args
            .split_once(char::is_whitespace)
            .map_or((args.trim(), ""), |(action, value)| (action, value.trim()));
        match action.to_ascii_lowercase().as_str() {
            "" | "list" => self.push_system(
                SystemTone::Info,
                format!(
                    "theme {}{} · available: {} · import/export/contrast/preview/apply/reset",
                    self.effective_theme(),
                    self.preview_settings.as_ref().map_or("", |_| " (preview)"),
                    settings::available_themes().join(", ")
                ),
            ),
            "preview" => {
                if value.is_empty() {
                    self.push_system(
                        SystemTone::Info,
                        "usage: /theme preview THEME or /theme preview PATH",
                    );
                    return;
                }
                let mut preview = self.tui_settings.clone();
                match settings::canonical_theme(value) {
                    Ok(theme) => {
                        preview.theme = theme;
                        match preview.validate() {
                            Ok(()) => self.preview_tui_settings(preview, "theme preview is not saved"),
                            Err(error) => self.push_system(SystemTone::Warning, error.to_string()),
                        }
                    }
                    Err(_) => match settings::import_theme(&PathBuf::from(value)) {
                        Ok(imported) => match preview.set_imported_theme(imported) {
                            Ok(()) => {
                                self.preview_tui_settings(preview, "imported theme preview is not saved")
                            }
                            Err(error) => self.push_system(SystemTone::Warning, error.to_string()),
                        },
                        Err(error) => self.push_system(SystemTone::Warning, error.to_string()),
                    },
                }
            }
            "apply" => {
                if let Some(preview) = self.preview_settings.clone() {
                    self.commit_tui_settings(preview, "saved active theme preview");
                } else {
                    self.push_system(SystemTone::Info, "no active theme preview to apply");
                }
            }
            "reset" => {
                self.preview_settings = None;
                let saved = self.tui_settings.clone();
                match self.apply_runtime_settings(&saved) {
                    Ok(()) => self.push_system(
                        SystemTone::Info,
                        "theme preview reset to saved user-local setting",
                    ),
                    Err(error) => self.push_system(SystemTone::Error, error.to_string()),
                }
            }
            "import" => {
                if value.is_empty() {
                    self.push_system(SystemTone::Info, "usage: /theme import PATH");
                    return;
                }
                match settings::import_theme(&PathBuf::from(value)) {
                    Ok(imported) => {
                        let name = imported.name.clone();
                        let mut next = self.tui_settings.clone();
                        match next.set_imported_theme(imported) {
                            Ok(()) => {
                                self.commit_tui_settings(next, format!("imported Opaline theme {name}"))
                            }
                            Err(error) => self.push_system(SystemTone::Error, error.to_string()),
                        }
                    }
                    Err(error) => self.push_system(SystemTone::Error, error.to_string()),
                }
            }
            "export" => {
                if value.is_empty() {
                    self.push_system(SystemTone::Info, "usage: /theme export PATH");
                    return;
                }
                let active = self.preview_settings.as_ref().unwrap_or(&self.tui_settings);
                match settings::export_theme(active, &PathBuf::from(value)) {
                    Ok(()) => self.push_system(SystemTone::Success, format!("exported theme to {value}")),
                    Err(error) => self.push_system(SystemTone::Error, error.to_string()),
                }
            }
            "contrast" => {
                let report = self.theme_palette.contrast_report();
                self.push_system(
                    if report.normal_passes() && report.active_passes() {
                        SystemTone::Success
                    } else {
                        SystemTone::Warning
                    },
                    format!(
                        "contrast · text {:.2}:1 ({}) · muted {:.2}:1 ({}) · accent {:.2}:1 ({})",
                        report.normal,
                        if report.normal_passes() { "AA" } else { "below AA" },
                        report.muted,
                        if report.muted_passes() { "AA" } else { "below AA" },
                        report.active,
                        if report.active_passes() {
                            "UI pass"
                        } else {
                            "below UI target"
                        },
                    ),
                );
            }
            requested => match settings::canonical_theme(requested) {
                Ok(theme) => {
                    let mut next = self.tui_settings.clone();
                    next.theme = theme.clone();
                    match next.validate() {
                        Ok(()) => {
                            self.commit_tui_settings(next, format!("saved theme {theme}"));
                            if self.no_color_forced {
                                self.push_system(
                                    SystemTone::Info,
                                    "NO_COLOR is set, so the saved theme will apply only after it is removed",
                                );
                            }
                        }
                        Err(error) => self.push_system(SystemTone::Warning, error.to_string()),
                    }
                }
                Err(error) => self.push_system(SystemTone::Warning, error.to_string()),
            },
        }
    }

    /// The settings drawer is a real user-local editor, not an invitation to
    /// edit project TOML.  Slash actions remain keyboard-first and are easy to
    /// discover from the anchored panel.
    fn edit_settings(&mut self, args: &str) {
        self.drawer_tab = DrawerTab::Settings;
        self.set_drawer_visible(true);
        let (action, value) = args
            .split_once(char::is_whitespace)
            .map_or((args.trim(), ""), |(action, value)| (action, value.trim()));
        match action.to_ascii_lowercase().as_str() {
            "" | "show" => {}
            "theme" => self.set_theme(value),
            "scroll" => match value.parse::<u16>() {
                Ok(lines) if (1..=100).contains(&lines) => {
                    let mut next = self.tui_settings.clone();
                    next.scroll_lines = lines;
                    self.commit_tui_settings(next, format!("saved scroll step {lines} lines"));
                }
                _ => self.push_system(SystemTone::Info, "usage: /settings scroll 1..100"),
            },
            "vim" => self.edit_settings_toggle("Vim mode", value, |settings, enabled| {
                settings.vim_scroll = enabled;
            }),
            "raw" => self.edit_settings_toggle("raw transcript", value, |settings, enabled| {
                settings.raw_transcript = enabled;
            }),
            "motion" => self.edit_settings_toggle("reduced motion", value, |settings, enabled| {
                settings.reduced_motion = enabled;
            }),
            "renderer" => match settings::canonical_renderer(value) {
                Ok(renderer) => {
                    let mut next = self.tui_settings.clone();
                    next.surface_renderer = renderer.clone();
                    self.commit_tui_settings(next, format!("saved surface renderer {renderer}"));
                }
                Err(error) => self.push_system(SystemTone::Warning, error.to_string()),
            },
            "save" => self.save_tui_settings(),
            "reset" => self.commit_tui_settings(TuiSettingsV1::default(), "reset TUI settings to defaults"),
            "path" => self.push_system(
                SystemTone::Info,
                self.settings_path.as_ref().map_or_else(
                    || "no user settings path is available on this host".into(),
                    |path| format!("user-local settings: {}", path.display()),
                ),
            ),
            "help" => self.push_system(
                SystemTone::Info,
                format!(
                    "/settings theme … · scroll N · vim on|off · raw on|off · motion on|off · renderer {} · save · reset · path",
                    settings::available_renderers().join("|")
                ),
            ),
            _ => self.push_system(SystemTone::Warning, "unknown settings action; use /settings help"),
        }
    }

    fn edit_settings_toggle(
        &mut self,
        label: &str,
        value: &str,
        apply: impl FnOnce(&mut TuiSettingsV1, bool),
    ) {
        match parse_toggle(value) {
            Some(enabled) => {
                let mut next = self.tui_settings.clone();
                apply(&mut next, enabled);
                self.commit_tui_settings(
                    next,
                    format!("saved {label} {}", if enabled { "on" } else { "off" }),
                );
            }
            None => self.push_system(
                SystemTone::Info,
                format!(
                    "usage: /settings {} on|off",
                    label.split(' ').next().unwrap_or(label)
                ),
            ),
        }
    }

    fn local_answer(&mut self, question: &str) {
        let q = question.trim().to_ascii_lowercase();
        let answer = if q.contains("agent") {
            format!(
                "{} agents known; {} still active.",
                self.agents.len(),
                self.agents
                    .iter()
                    .filter(|agent| !terminal_agent_state(agent.state))
                    .count()
            )
        } else if q.contains("token") || q.contains("usage") || q.contains("context") {
            format!(
                "{} input and {} output tokens; {:.1}% of configured context.",
                self.input_tokens,
                self.output_tokens,
                self.context_percent()
            )
        } else if q.contains("plan") || q.contains("task") {
            format!(
                "{} plan tasks: {} complete, {} blocked or failed.",
                self.plan.len(),
                self.plan
                    .iter()
                    .filter(|task| task.state == minha_core::protocol::PlanTaskState::Completed)
                    .count(),
                self.plan
                    .iter()
                    .filter(|task| matches!(
                        task.state,
                        minha_core::protocol::PlanTaskState::Blocked
                            | minha_core::protocol::PlanTaskState::Failed
                    ))
                    .count()
            )
        } else {
            format!(
                "Session is {} in {} mode. No model call was made for this answer.",
                self.status,
                self.mode.label()
            )
        };
        self.overlay = Some(Overlay::LocalAnswer {
            question: question.into(),
            answer,
        });
    }

    fn local_status(&mut self) {
        self.overlay = Some(Overlay::Status);
        self.submission = Some(Submission::ShowStatus);
    }

    fn open_help(&mut self) {
        self.overlay_scroll = 0;
        if self.last_terminal_width >= WIDE_DRAWER_MIN_WIDTH {
            self.overlay = None;
            self.drawer_tab = DrawerTab::Help;
            self.set_drawer_visible(true);
        } else {
            self.overlay = toggle_overlay(&self.overlay, Overlay::Help);
        }
    }

    fn clear_input(&mut self) {
        self.input.clear();
        self.input_cursor = 0;
        self.vim_insert_undo_grouped = false;
        self.paste_summary = None;
        self.history_cursor = None;
        self.close_completion();
    }

    /// Drop outstanding input surfaces when a run ends, errors, or is
    /// interrupted so a dead session cannot stay in "answer required" state
    /// and answers can never be sent to a finished run.
    fn clear_pending_input(&mut self) {
        self.pending_request = None;
        self.clarification = None;
        self.clarification_answers.clear();
        self.clarification_note_open = false;
        self.selected_clarification_question = 0;
        self.selected_clarification_option = 0;
    }

    fn escape(&mut self) {
        // Esc dismisses the completion popup first, and preserves the draft:
        // closing the list must never clear what was typed.
        if self.completion_open() {
            self.dismiss_command_surface();
            return;
        }
        if self.has_active_clarification() {
            if !self.input.is_empty() {
                self.clear_input();
            }
            self.clarification_note_open = false;
            return;
        }
        // A pending request no longer floats over the transcript (it's a
        // fixed row now), so Esc has nothing to hide. It only needs to
        // protect a typed draft from the pause shortcut below; with no draft,
        // Esc is free to fall through to pausing the run when one is active.
        if self.pending_request.is_some() && !self.input.is_empty() {
            self.clear_input();
            return;
        }
        if self.overlay.take().is_some() {
            return;
        }
        if self.drawer_visible && self.drawer_tab == DrawerTab::Help {
            self.set_drawer_visible(false);
            return;
        }
        if self.focused_agent.take().is_some() {
            return;
        }
        if self.running
            && let Some(run_id) = self.active_run
        {
            let now = Instant::now();
            let double_escape = self
                .last_escape_at
                .is_some_and(|previous| now.duration_since(previous).as_millis() <= 600);
            self.last_escape_at = Some(now);
            if double_escape {
                self.last_escape_at = None;
                self.submission = Some(Submission::Pause { run_id });
                self.push_system(SystemTone::Info, "pausing at the next safe boundary");
            } else {
                self.push_system(
                    SystemTone::Info,
                    "press Esc again to pause safely · Ctrl-C interrupts",
                );
            }
            return;
        }
        self.clear_input();
    }

    /// True when the visible surface owns text scrolling rather than a list
    /// with its own selection. Help moves into the anchored Operations drawer
    /// on wide terminals, while the inspectors remain modal views.
    pub(crate) fn overlay_scrolls(&self) -> bool {
        matches!(
            self.overlay,
            Some(Overlay::Help | Overlay::Keymap | Overlay::Status | Overlay::Context | Overlay::Doctor)
        ) || (self.drawer_visible && self.drawer_tab == DrawerTab::Help)
    }

    fn page(&mut self, direction: isize) {
        if self.completion_open() {
            let step = self.completion_items.len().min(8) as isize;
            self.select(direction * step.max(1));
        } else if self.overlay_scrolls() {
            self.scroll_overlay(direction * 8);
        } else if direction < 0 {
            self.scroll_up(self.tui_settings.scroll_lines.saturating_mul(4));
        } else {
            self.scroll_down(self.tui_settings.scroll_lines.saturating_mul(4));
        }
    }

    fn scroll_overlay(&mut self, delta: isize) {
        self.overlay_scroll = if delta < 0 {
            self.overlay_scroll.saturating_sub(delta.unsigned_abs() as u16)
        } else {
            self.overlay_scroll
                .saturating_add(delta as u16)
                .min(self.overlay_scroll_max.get())
        };
    }

    fn select(&mut self, delta: isize) {
        if self.overlay_scrolls() {
            self.scroll_overlay(delta);
            return;
        }
        if self.completion_open() {
            self.selected_completion =
                move_index(self.selected_completion, self.completion_items.len(), delta);
        } else if self.has_active_clarification() && !self.clarification_note_open {
            let count = self.clarification_option_count();
            self.selected_clarification_option = move_index(self.selected_clarification_option, count, delta);
        } else if let Some(request) = &self.pending_request {
            let count = request.options.len();
            self.selected_clarification_option = move_index(self.selected_clarification_option, count, delta);
        } else if matches!(self.overlay, Some(Overlay::Sessions)) {
            self.selected_session = move_index(self.selected_session, self.sessions.len(), delta);
        } else if matches!(self.overlay, Some(Overlay::Books)) {
            self.selected_book = move_index(self.selected_book, self.library.len(), delta);
        } else if self.drawer_interactive() || self.focused_agent.is_some() {
            match self.drawer_tab {
                DrawerTab::Activity => {
                    self.selected_agent = move_index(self.selected_agent, self.agents.len(), delta)
                }
                DrawerTab::Work => {
                    self.selected_task = move_index(self.selected_task, self.work_item_count(), delta)
                }
                DrawerTab::Board => {
                    self.selected_board = move_index(self.selected_board, self.board.len(), delta)
                }
                DrawerTab::Problems => {
                    self.selected_problem = move_index(self.selected_problem, self.incidents.len(), delta)
                }
                // Static key/value panels, nothing to select.
                DrawerTab::Route | DrawerTab::Usage | DrawerTab::Settings | DrawerTab::Help => {}
            }
        } else {
            if delta < 0 {
                self.scroll_up(1);
            } else {
                self.scroll_down(1);
            }
        }
    }

    /// True while the completion popup owns Up/Down, Enter, and Esc.
    pub(crate) fn completion_open(&self) -> bool {
        self.completion_kind != CompletionKind::None && !self.completion_items.is_empty()
    }

    /// Enter on the command list: accept the highlighted entry.
    ///
    /// Commands that take no argument run immediately; commands that expect one
    /// are completed into the composer so the argument can be typed.
    fn accept_completion(&mut self) {
        let Some(entry) = self.completion_items.get(self.selected_completion).cloned() else {
            return;
        };
        match self.completion_kind {
            CompletionKind::Command => {
                self.insert_command(&entry.value);
                if !entry.needs_argument {
                    self.command_draft = None;
                    self.clear_input();
                    self.handle_slash(&entry.value);
                }
            }
            CompletionKind::Path => {
                let token_start = self.input[..self.input_cursor]
                    .rfind(char::is_whitespace)
                    .map_or(0, |index| index + 1);
                self.checkpoint_editor();
                self.input
                    .replace_range(token_start..self.input_cursor, &entry.value);
                self.input_cursor = token_start + entry.value.len();
                self.close_completion();
            }
            CompletionKind::None => {}
        }
    }

    fn activate_selection(&mut self) {
        if self.completion_open() {
            self.accept_completion();
        } else if self.has_active_clarification() {
            self.submit_clarification(String::new());
        } else if self.pending_request.is_some() {
            self.submit_pending_request(String::new());
        } else if matches!(self.overlay, Some(Overlay::Sessions)) {
            if let Some(run) = self.sessions.get(self.selected_session) {
                self.submission = Some(Submission::Resume { run_id: run.id });
                self.overlay = None;
            }
        } else if self.drawer_interactive()
            && self.drawer_tab == DrawerTab::Activity
            && let Some(agent) = self.agents.get(self.selected_agent)
        {
            self.focused_agent = Some(agent.id);
            self.set_drawer_visible(false);
            self.scroll_state.top();
        }
    }

    fn toggle_selected_item(&mut self) {
        if let Some(summary) = &mut self.paste_summary {
            summary.expanded = !summary.expanded;
            return;
        }
        let Some(index) = self
            .items
            .iter()
            .rposition(|item| matches!(item, TranscriptItem::Tool { .. } | TranscriptItem::Diff { .. }))
        else {
            return;
        };
        if let TranscriptItem::Diff { expanded, .. } = &mut self.items[index] {
            *expanded = !*expanded;
            self.invalidate_transcript_layout();
            return;
        }
        let (kind, expanded) = match &self.items[index] {
            TranscriptItem::Tool { name, expanded, .. } => (tool_activity_kind(name), !*expanded),
            _ => return,
        };
        let mut start = index;
        while start > 0
            && matches!(&self.items[start - 1], TranscriptItem::Tool { name, .. } if tool_activity_kind(name) == kind)
        {
            start -= 1;
        }
        for item in &mut self.items[start..=index] {
            if let TranscriptItem::Tool { expanded: value, .. } = item {
                *value = expanded;
            }
        }
        self.invalidate_transcript_layout();
    }

    fn scroll_down(&mut self, amount: u16) {
        let max_scroll = self.transcript_layout.borrow().max_scroll;
        self.scroll_state.scroll_down(amount, max_scroll);
    }

    fn scroll_up(&mut self, amount: u16) {
        let max_scroll = self.transcript_layout.borrow().max_scroll;
        self.scroll_state.scroll_up(amount, max_scroll);
    }

    fn history_previous(&mut self) {
        if self.history.is_empty() {
            return;
        }
        if self.history_cursor.is_none() {
            self.history_draft = Some(self.input.clone());
        }
        let index = self
            .history_cursor
            .map_or(self.history.len() - 1, |index| index.saturating_sub(1));
        self.history_cursor = Some(index);
        self.input.clone_from(&self.history[index]);
        self.input_cursor = self.input.len();
    }

    fn history_next(&mut self) {
        let Some(index) = self.history_cursor else {
            return;
        };
        if index + 1 < self.history.len() {
            self.history_cursor = Some(index + 1);
            self.input.clone_from(&self.history[index + 1]);
        } else {
            self.history_cursor = None;
            self.input = self.history_draft.take().unwrap_or_default();
        }
        self.input_cursor = self.input.len();
    }

    fn checkpoint_editor(&mut self) {
        if self.vim_scroll_enabled() && self.vim_mode == VimMode::Insert {
            if self.vim_insert_undo_grouped {
                return;
            }
            self.vim_insert_undo_grouped = true;
        }
        let snapshot = (self.input.clone(), self.input_cursor);
        if self.undo_stack.last() != Some(&snapshot) {
            self.undo_stack.push(snapshot);
            if self.undo_stack.len() > 100 {
                self.undo_stack.remove(0);
            }
        }
        self.redo_stack.clear();
    }

    fn undo(&mut self) {
        if let Some(snapshot) = self.undo_stack.pop() {
            self.redo_stack.push((self.input.clone(), self.input_cursor));
            (self.input, self.input_cursor) = snapshot;
            self.preferred_column = None;
        }
    }

    fn redo(&mut self) {
        if let Some(snapshot) = self.redo_stack.pop() {
            self.undo_stack.push((self.input.clone(), self.input_cursor));
            (self.input, self.input_cursor) = snapshot;
            self.preferred_column = None;
        }
    }

    fn vim_delete_char(&mut self) {
        if let Some(next) = self.input[self.input_cursor..]
            .graphemes(true)
            .next()
            .map(str::len)
        {
            self.checkpoint_editor();
            self.input.drain(self.input_cursor..self.input_cursor + next);
            self.preferred_column = None;
        }
        self.paste_summary = None;
        self.vim_mode = VimMode::Normal;
        self.vim_insert_undo_grouped = false;
    }

    fn vim_delete_line(&mut self) {
        let start = self.line_start();
        let end = self.line_end();
        let (start, end) = if end < self.input.len() {
            (start, end + 1)
        } else if start > 0 {
            (start - 1, end)
        } else {
            (start, end)
        };
        if start < end {
            self.checkpoint_editor();
            self.input.drain(start..end);
            self.input_cursor = start.min(self.input.len());
        }
        self.paste_summary = None;
        self.preferred_column = None;
        self.vim_mode = VimMode::Normal;
        self.vim_insert_undo_grouped = false;
    }

    fn vim_delete_to_line_end(&mut self, insert: bool) {
        let end = self.line_end();
        let mut changed = false;
        if self.input_cursor < end {
            self.checkpoint_editor();
            self.input.drain(self.input_cursor..end);
            changed = true;
        }
        self.paste_summary = None;
        self.preferred_column = None;
        if insert {
            self.begin_vim_insert(changed);
        } else {
            self.vim_mode = VimMode::Normal;
            self.vim_insert_undo_grouped = false;
        }
    }

    fn vim_yank_line(&mut self) {
        let start = self.line_start();
        let end = self.line_end();
        let mut line = self.input[start..end].to_owned();
        line.push('\n');
        self.vim_yank = Some(line);
        self.vim_mode = VimMode::Normal;
        self.vim_insert_undo_grouped = false;
        self.push_system(SystemTone::Info, "yanked current line locally");
    }

    fn vim_paste_line(&mut self) {
        let Some(line) = self.vim_yank.clone() else {
            self.push_system(SystemTone::Info, "nothing yanked yet; use yy first");
            return;
        };
        let text = line.trim_end_matches('\n');
        if self.input.len().saturating_add(text.len()).saturating_add(1) > INPUT_LIMIT {
            self.push_system(SystemTone::Warning, "yanked line does not fit in the composer");
            return;
        }
        self.checkpoint_editor();
        if self.input.is_empty() {
            self.input.push_str(text);
            self.input_cursor = 0;
        } else {
            let insert_at = self.line_end();
            let insertion = format!("\n{text}");
            self.input.insert_str(insert_at, &insertion);
            self.input_cursor = insert_at.saturating_add(1);
        }
        self.paste_summary = None;
        self.preferred_column = None;
        self.vim_mode = VimMode::Normal;
        self.vim_insert_undo_grouped = false;
    }

    fn vim_open_line(&mut self, above: bool) {
        if self.input.len() >= INPUT_LIMIT {
            return;
        }
        self.checkpoint_editor();
        let insert_at = if above { self.line_start() } else { self.line_end() };
        self.input.insert(insert_at, '\n');
        self.input_cursor = if above { insert_at } else { insert_at + 1 };
        self.paste_summary = None;
        self.preferred_column = None;
        self.begin_vim_insert(true);
    }

    fn begin_vim_insert(&mut self, undo_group_started: bool) {
        self.vim_mode = VimMode::Insert;
        self.vim_insert_undo_grouped = undo_group_started;
    }

    fn paste(&mut self, value: &str) {
        let normalized = value.replace("\r\n", "\n").replace('\r', "\n");
        let remaining = INPUT_LIMIT.saturating_sub(self.input.len());
        let mut end = 0;
        for (index, grapheme) in normalized.grapheme_indices(true) {
            if index + grapheme.len() > remaining {
                break;
            }
            end = index + grapheme.len();
        }
        if end > 0 {
            if self.has_active_clarification() {
                self.clarification_note_open = true;
            }
            self.checkpoint_editor();
            self.input.insert_str(self.input_cursor, &normalized[..end]);
            self.input_cursor += end;
            self.history_cursor = None;
            self.preferred_column = None;
            let pasted = &normalized[..end];
            let lines = pasted.lines().count().max(1);
            let graphemes = pasted.graphemes(true).count();
            self.paste_summary = (lines > 12 || graphemes > 1_024).then_some(PasteSummary {
                lines,
                graphemes,
                expanded: false,
            });
        }
    }

    /// Standard arrow-key movement preserves the long-standing history
    /// shortcut at the top/bottom of the composer.
    fn move_cursor_vertical(&mut self, direction: isize) {
        self.move_cursor_vertical_impl(direction, true);
    }

    /// Vim Normal-mode `j`/`k` is strictly current-composer navigation. It
    /// must not replace the draft with history at an edge.
    fn move_cursor_vertical_bounded(&mut self, direction: isize) {
        self.move_cursor_vertical_impl(direction, false);
    }

    fn move_cursor_vertical_impl(&mut self, direction: isize, history_at_edges: bool) {
        let layout = EditorLayout::new(&self.input, self.input_cursor, self.composer_inner_width.get());
        let column = self.preferred_column.unwrap_or(layout.cursor_column);
        self.preferred_column = Some(column);
        if direction < 0 {
            if layout.cursor_row == 0 {
                if history_at_edges && self.input_cursor == 0 {
                    self.history_previous();
                }
                return;
            }
            self.input_cursor = layout.byte_at_column(&self.input, layout.cursor_row - 1, column);
        } else if layout.cursor_row + 1 >= layout.lines.len() {
            if history_at_edges && self.input_cursor == self.input.len() {
                self.history_next();
            }
        } else {
            self.input_cursor = layout.byte_at_column(&self.input, layout.cursor_row + 1, column);
        }
    }

    /// `Ctrl-P`: open the same registry-backed, searchable surface that typing
    /// `/` opens, rather than a second, non-searchable list.
    ///
    /// Pressing it again closes the surface. Any draft already in the composer is
    /// stashed and restored on close, so the shortcut never destroys typed text.
    fn open_command_surface(&mut self) {
        if self.completion_kind == CompletionKind::Command {
            self.dismiss_command_surface();
            return;
        }
        if !self.input.starts_with('/') {
            if !self.input.is_empty() {
                self.command_draft = Some(self.input.clone());
            }
            self.checkpoint_editor();
            self.input = "/".into();
        }
        self.input_cursor = self.command_token().map_or(self.input.len(), |token| token.end);
        self.refresh_command_completion();
    }

    /// Close the command list, restoring any draft `Ctrl-P` stashed. The typed
    /// text is never cleared — Esc dismisses the popup, not the composer.
    fn dismiss_command_surface(&mut self) {
        self.close_completion();
        if let Some(draft) = self.command_draft.take() {
            self.input = draft;
            self.input_cursor = self.input.len();
        }
    }

    /// Byte offset of the start of the logical line the cursor is on.
    fn line_start(&self) -> usize {
        self.input[..self.input_cursor]
            .rfind('\n')
            .map_or(0, |index| index + 1)
    }

    /// Byte offset of the end of the logical line the cursor is on, excluding
    /// the newline itself.
    fn line_end(&self) -> usize {
        self.input_cursor
            + self.input[self.input_cursor..]
                .find('\n')
                .unwrap_or(self.input.len() - self.input_cursor)
    }

    /// The command token's byte range, when the cursor sits inside it.
    ///
    /// The token runs from the leading `/` to the first whitespace. Once the
    /// cursor leaves it (the user has moved on to arguments) the command list is
    /// no longer relevant and closes.
    fn command_token(&self) -> Option<std::ops::Range<usize>> {
        if !self.input.starts_with('/') {
            return None;
        }
        let end = self.input[1..]
            .find(char::is_whitespace)
            .map_or(self.input.len(), |index| index + 1);
        (self.input_cursor <= end).then_some(0..end)
    }

    /// Re-derive the command list from the registry for the current composer
    /// contents. Called after every edit, cursor move, undo, and redo so the
    /// popup can never go stale, and closes it once the cursor leaves the token.
    pub(crate) fn refresh_command_completion(&mut self) {
        if self.completion_kind == CompletionKind::Path {
            return;
        }
        let Some(token) = self.command_token() else {
            if self.completion_kind == CompletionKind::Command {
                self.close_completion();
            }
            return;
        };
        // Bounded by the cursor, not the token's end: text after the cursor is
        // not part of what the user has typed as the query, and must survive
        // completion untouched (see `insert_command`).
        let query = self.input[token.start + 1..self.input_cursor].to_owned();
        let previous = self
            .completion_items
            .get(self.selected_completion)
            .map(|entry| entry.value.clone());
        self.completion_items = commands::matches(&query, self.command_context())
            .into_iter()
            .map(|entry| CompletionEntry {
                value: entry.spec.name.to_owned(),
                display: entry.spec.display(),
                description: entry.spec.description.to_owned(),
                category: Some(entry.spec.category),
                unavailable: entry.availability.reason(),
                needs_argument: entry.spec.needs_argument,
                network: entry.spec.network,
            })
            .collect();
        self.completion_kind = if self.completion_items.is_empty() {
            CompletionKind::None
        } else {
            CompletionKind::Command
        };
        // Keep the highlight on the same command when it survives the re-filter,
        // so the selection does not jump around as the query narrows.
        self.selected_completion = previous
            .and_then(|value| {
                self.completion_items
                    .iter()
                    .position(|entry| entry.value == value)
            })
            .unwrap_or(0);
    }

    pub(crate) fn command_context(&self) -> CommandContext {
        CommandContext {
            active_run: self.active_run.is_some(),
        }
    }

    /// Record an explicit user choice for the drawer, scoped to whichever
    /// width class the terminal is in right now, and apply it immediately.
    pub(crate) fn set_drawer_visible(&mut self, visible: bool) {
        if self.last_terminal_width >= WIDE_DRAWER_MIN_WIDTH {
            self.drawer_override_wide = Some(visible);
        } else {
            self.drawer_override_narrow = Some(visible);
        }
        self.drawer_visible = visible;
    }

    /// Recompute `drawer_visible` from the current terminal width and
    /// whichever width class's override is set. Called every frame from the
    /// event loop (cheap and idempotent) so a resize across the wide/narrow
    /// boundary always lands on the right default without needing its own
    /// dedicated resize handler. Drawers are opt-in at every width; an
    /// explicit choice is remembered only for the running session.
    pub(crate) fn sync_drawer_visibility(&mut self, terminal_width: u16) {
        self.last_terminal_width = terminal_width;
        self.drawer_visible = if terminal_width >= WIDE_DRAWER_MIN_WIDTH {
            self.drawer_override_wide.unwrap_or(false)
        } else {
            self.drawer_override_narrow.unwrap_or(false)
        };
    }

    /// True only when the user explicitly opened the drawer (Shift-Tab or a
    /// tab command). Keyboard selection must never be stolen from the
    /// transcript merely because an operations panel is visible.
    pub(crate) fn drawer_interactive(&self) -> bool {
        let explicit = if self.last_terminal_width >= WIDE_DRAWER_MIN_WIDTH {
            self.drawer_override_wide
        } else {
            self.drawer_override_narrow
        };
        self.drawer_visible && explicit == Some(true)
    }

    /// The Work drawer renders plan tasks followed by every agent TODO. Keep
    /// selection in that same flattened coordinate space so keyboard and
    /// mouse input can reach the visible TODO rows as well as the plan.
    pub(crate) fn work_item_count(&self) -> usize {
        self.plan.len() + self.todos.values().map(Vec::len).sum::<usize>()
    }

    /// Post-edit hook: path suggestions are dropped, the command list re-derives.
    fn sync_completion_after_edit(&mut self) {
        if self.completion_kind == CompletionKind::Path {
            self.close_completion();
        }
        self.refresh_command_completion();
    }

    pub(crate) fn close_completion(&mut self) {
        self.completion_items.clear();
        self.completion_kind = CompletionKind::None;
        self.selected_completion = 0;
    }

    /// Replace the command token up to the cursor with `name`, leaving a
    /// trailing space so the argument can be typed straight away. Only the
    /// typed prefix is replaced — text after the cursor is preserved, not
    /// clobbered by the full token span.
    fn insert_command(&mut self, name: &str) {
        let Some(token) = self.command_token() else {
            return;
        };
        self.checkpoint_editor();
        let completed = format!("/{name} ");
        self.input
            .replace_range(token.start..self.input_cursor, &completed);
        self.input_cursor = token.start + completed.len();
        self.close_completion();
    }

    /// Tab: complete the highlighted entry.
    fn complete_input(&mut self) {
        if self.completion_kind == CompletionKind::Command
            && let Some(entry) = self.completion_items.get(self.selected_completion)
        {
            let value = entry.value.clone();
            self.insert_command(&value);
            return;
        }
        if let Some(token) = self.command_token() {
            self.refresh_command_completion();
            // A clear winner needs no disambiguation, so Tab takes it directly.
            // "Clear winner" means either the only match, or strictly better
            // ranked than the runner-up — fuzzy/substring/description hits pad
            // the popup for browsing but must never be silently accepted just
            // because the popup happens to be showing one of them.
            let query = self.input[token.start + 1..self.input_cursor].to_owned();
            let ranked = commands::matches(&query, self.command_context());
            let winner = match ranked.as_slice() {
                [only] => Some(only.spec.name),
                [first, second, ..] if first.rank < second.rank => Some(first.spec.name),
                _ => None,
            };
            if let Some(name) = winner {
                self.insert_command(name);
            }
            return;
        }
        self.close_completion();
        let token_start = self.input[..self.input_cursor]
            .rfind(char::is_whitespace)
            .map_or(0, |index| index + 1);
        let token = &self.input[token_start..self.input_cursor];
        if token.is_empty() {
            return;
        }
        let typed = PathBuf::from(token);
        let parent = typed.parent().unwrap_or_else(|| std::path::Path::new(""));
        let prefix = typed
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        let directory = if parent.as_os_str().is_empty() {
            self.root.clone()
        } else if parent.is_absolute() {
            parent.to_owned()
        } else {
            self.root.join(parent)
        };
        let mut paths = std::fs::read_dir(directory)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let name = entry.file_name().to_str()?.to_owned();
                name.starts_with(prefix).then(|| {
                    let mut completed = parent.join(&name).to_string_lossy().into_owned();
                    let description = if entry.path().is_dir() {
                        completed.push('/');
                        "directory"
                    } else {
                        "path"
                    };
                    CompletionEntry::path(completed, description)
                })
            })
            .take(8)
            .collect::<Vec<_>>();
        paths.sort_by(|left, right| left.value.cmp(&right.value));
        self.completion_items = paths.clone();
        self.selected_completion = 0;
        self.completion_kind = if paths.is_empty() {
            CompletionKind::None
        } else {
            CompletionKind::Path
        };
        if let [candidate] = paths.as_slice() {
            self.checkpoint_editor();
            // Complete in place so text after the cursor survives.
            self.input
                .replace_range(token_start..self.input_cursor, &candidate.value);
            self.input_cursor = token_start + candidate.value.len();
            self.close_completion();
        }
    }

    pub(crate) fn take_submission(&mut self) -> Option<Submission> {
        self.submission.take()
    }

    pub(crate) fn apply_event(&mut self, envelope: &EventEnvelope) {
        let persisted = envelope.sequence != u64::MAX;
        if persisted {
            if self
                .last_sequence
                .get(&envelope.run_id)
                .is_some_and(|last| envelope.sequence <= *last)
            {
                return;
            }
            self.last_sequence.insert(envelope.run_id, envelope.sequence);
        }
        self.transcript_revision = self.transcript_revision.wrapping_add(1).max(1);
        match &envelope.event {
            RuntimeEvent::SessionStarted { .. } => {
                self.active_run = Some(envelope.run_id);
                self.running = true;
                self.state = ExitState::Running;
                self.termination_reason = None;
                self.status = "working".into();
                self.pending_request = None;
                self.clarification = None;
                self.clarification_answers.clear();
                self.set_phase(RunPhase::Preflight);
            }
            RuntimeEvent::SessionResumed => {
                self.active_run = Some(envelope.run_id);
                self.running = true;
                self.state = ExitState::Running;
                self.termination_reason = None;
                self.status = "working".into();
                self.pending_request = None;
                self.overlay = None;
            }
            RuntimeEvent::SessionState { state } => {
                self.state = *state;
                if *state == ExitState::Running {
                    self.termination_reason = None;
                }
                if *state != ExitState::UsagePaused
                    || !matches!(
                        self.termination_reason,
                        Some(TerminationReason::BudgetTarget | TerminationReason::ProviderReserve)
                    )
                {
                    self.status = state_label(*state).into();
                }
            }
            RuntimeEvent::UserMessage { text, steering } => {
                if !matches!(self.items.last(), Some(TranscriptItem::User { text: old, steering: old_steering }) if old == text && old_steering == steering)
                {
                    self.items.push(TranscriptItem::User {
                        text: text.clone(),
                        steering: *steering,
                    });
                }
            }
            RuntimeEvent::AgentStarted {
                agent_id,
                role,
                model,
                ..
            } => {
                self.running = true;
                self.state = ExitState::Running;
                self.termination_reason = None;
                self.status = "working".into();
                if !self.agents.iter().any(|agent| agent.id == *agent_id) {
                    self.agents.push(AgentView {
                        id: *agent_id,
                        role: role.clone(),
                        model: model.clone(),
                        state: AgentState::Starting,
                        detail: "starting".into(),
                    });
                }
                if role.contains("Mina") {
                    self.model = model.clone();
                }
            }
            RuntimeEvent::AgentState {
                agent_id,
                state,
                detail,
            } => {
                if let Some(agent) = self.agents.iter_mut().find(|agent| agent.id == *agent_id) {
                    agent.state = *state;
                    agent.detail = detail.clone();
                }
            }
            RuntimeEvent::TextDelta {
                agent_id,
                item_id,
                delta,
            } => {
                let role = self
                    .agents
                    .iter()
                    .find(|agent| agent.id == *agent_id)
                    .map(|agent| agent.role.as_str())
                    .unwrap_or("agent");
                if !internal_role(role) {
                    if let Some(TranscriptItem::Assistant { text, .. }) = self.items.iter_mut().find(
                        |item| matches!(item, TranscriptItem::Assistant { item_id: id, .. } if id == item_id),
                    ) {
                        text.push_str(delta);
                    } else {
                        self.items.push(TranscriptItem::Assistant {
                            item_id: *item_id,
                            agent_id: *agent_id,
                            role: role.to_owned(),
                            text: delta.clone(),
                            streaming: true,
                        });
                    }
                }
            }
            RuntimeEvent::AssistantMessage {
                agent_id,
                item_id,
                role,
                text,
                ..
            } => {
                let visible_text = strip_control_tags(text);
                if internal_role(role) || visible_text.is_empty() {
                    self.items.retain(
                        |item| !matches!(item, TranscriptItem::Assistant { item_id: id, .. } if id == item_id),
                    );
                    self.invalidate_transcript_layout();
                    return;
                }
                if let Some(TranscriptItem::Assistant {
                    text: current,
                    streaming,
                    ..
                }) = self.items.iter_mut().find(
                    |item| matches!(item, TranscriptItem::Assistant { item_id: id, .. } if id == item_id),
                ) {
                    *current = visible_text.clone();
                    *streaming = false;
                } else {
                    self.items.push(TranscriptItem::Assistant {
                        item_id: *item_id,
                        agent_id: *agent_id,
                        role: role.clone(),
                        text: visible_text,
                        streaming: false,
                    });
                }
            }
            RuntimeEvent::PlanCreated { tasks, .. } => self.plan = tasks.clone(),
            RuntimeEvent::PlanTaskChanged {
                task_id,
                state,
                agent_id,
            } => {
                if let Some(task) = self.plan.iter_mut().find(|task| task.id == *task_id) {
                    task.state = *state;
                    task.agent_id = *agent_id;
                }
            }
            RuntimeEvent::TodoChanged { agent_id, item } => {
                let items = self.todos.entry(*agent_id).or_default();
                if let Some(existing) = items.iter_mut().find(|existing| existing.id == item.id) {
                    *existing = item.clone();
                } else {
                    items.push(item.clone());
                }
                items.sort_by_key(|item| (item.order, item.id.clone()));
            }
            RuntimeEvent::TodoRollupChanged {
                active,
                blocked,
                completed,
                stale_agents,
                active_goals,
                blocked_work,
                recently_completed,
            } => {
                self.todo_active = *active;
                self.todo_blocked = *blocked;
                self.todo_completed = *completed;
                self.todo_stale_agents = *stale_agents;
                self.todo_active_goals = active_goals.clone();
                self.todo_blocked_work = blocked_work.clone();
                self.todo_recently_completed = recently_completed.clone();
            }
            RuntimeEvent::ToolStarted {
                agent_id,
                call_id,
                name,
                arguments,
            } => self.items.push(TranscriptItem::Tool {
                agent_id: *agent_id,
                call_id: call_id.clone(),
                name: name.clone(),
                arguments: compact_json(arguments),
                output: String::new(),
                exit_code: None,
                running: true,
                expanded: self.details_expanded,
            }),
            RuntimeEvent::ToolOutput {
                call_id,
                stdout,
                stderr,
                exit_code,
                ..
            } => {
                if let Some(TranscriptItem::Tool {
                    output,
                    exit_code: code,
                    running,
                    ..
                }) =
                    self.items.iter_mut().rev().find(
                        |item| matches!(item, TranscriptItem::Tool { call_id: id, .. } if id == call_id),
                    )
                {
                    *output = if stderr.is_empty() {
                        stdout.clone()
                    } else if stdout.is_empty() {
                        stderr.clone()
                    } else {
                        format!("{stdout}\n{stderr}")
                    };
                    *code = *exit_code;
                    *running = false;
                }
            }
            RuntimeEvent::FileChange { path, diff, .. } => {
                self.items.push(TranscriptItem::Diff {
                    path: path.clone(),
                    diff: diff.clone(),
                    expanded: false,
                });
            }
            RuntimeEvent::Question {
                request_id,
                question,
                options,
                ..
            } => {
                // A live question supersedes any clarification session so
                // answers can never be routed to the wrong surface.
                self.clarification = None;
                self.clarification_answers.clear();
                self.pending_request = Some(PendingRequest {
                    id: *request_id,
                    question: question.clone(),
                    options: options.clone(),
                    approval: false,
                    reason: None,
                    command: None,
                });
                self.selected_clarification_option = 0;
                self.running = self.agents.iter().any(|agent| !terminal_agent_state(agent.state));
                self.state = ExitState::NeedsInput;
                self.status = if self.running {
                    "question waiting · hive working".into()
                } else {
                    "needs input".into()
                };
            }
            RuntimeEvent::ClarificationStarted { clarification }
            | RuntimeEvent::ClarificationUpdated { clarification } => {
                self.clarification = Some(clarification.clone());
                self.clarification_answers.clear();
                self.selected_clarification_question = 0;
                self.selected_clarification_option = 0;
                self.clarification_note_open = false;
                self.pending_request = None;
                self.overlay = None;
                self.running = false;
                self.state = if clarification.status == ClarificationStatus::Cancelled {
                    ExitState::Cancelled
                } else if clarification.status == ClarificationStatus::Confirmed {
                    ExitState::Running
                } else {
                    ExitState::NeedsInput
                };
                self.status = if clarification.status == ClarificationStatus::Reviewing {
                    "confirm details".into()
                } else {
                    "needs input".into()
                };
            }
            RuntimeEvent::ClarificationConfirmed { brief } => {
                if let Some(clarification) = self.clarification.as_mut() {
                    clarification.status = ClarificationStatus::Confirmed;
                    clarification.brief = Some(brief.clone());
                }
                self.overlay = None;
                self.status = "issue brief confirmed".into();
            }
            RuntimeEvent::Approval {
                request_id,
                reason,
                command,
                ..
            } => {
                self.clarification = None;
                self.clarification_answers.clear();
                let question = if command.is_some() {
                    "Approve this risky action?"
                } else {
                    "Approve integrating this work?"
                };
                self.pending_request = Some(PendingRequest {
                    id: *request_id,
                    question: question.into(),
                    options: vec!["yes".into(), "no".into()],
                    approval: true,
                    reason: Some(reason.clone()),
                    command: command.clone(),
                });
                // Default the highlighted option to "no": Enter now answers
                // the card directly, and a destructive action must never be
                // one reflexive keystroke away from running.
                self.selected_clarification_option = 1;
                self.running = false;
                self.state = ExitState::ApprovalRequired;
                self.status = "approval required".into();
            }
            RuntimeEvent::RequestResolved { .. } => {
                self.pending_request = None;
                self.selected_clarification_option = 0;
            }
            RuntimeEvent::SteeringQueued { text } => {
                self.queued_steering += 1;
                self.items.push(TranscriptItem::User {
                    text: text.clone(),
                    steering: true,
                });
            }
            RuntimeEvent::SteeringApplied { .. } => {
                self.queued_steering = self.queued_steering.saturating_sub(1);
            }
            RuntimeEvent::Usage {
                model,
                input_tokens,
                output_tokens,
                cached_input_tokens,
                cache_write_tokens,
                reasoning_output_tokens,
                ..
            } => {
                self.input_tokens += input_tokens;
                self.output_tokens += output_tokens;
                self.cached_input_tokens += cached_input_tokens;
                self.cache_write_tokens += cache_write_tokens;
                self.reasoning_output_tokens += reasoning_output_tokens;
                if let Some(cost) =
                    estimate_cost_usd(model, *input_tokens, *cached_input_tokens, *output_tokens)
                {
                    self.deepseek_estimated_usd += cost;
                    if let Some(pricing) = pricing_for_model(model) {
                        self.deepseek_cache_savings_usd += (*cached_input_tokens).min(*input_tokens) as f64
                            * (pricing.cache_miss_input_per_million - pricing.cache_hit_input_per_million)
                            / 1_000_000.0;
                    }
                }
                if let Some(cost) =
                    estimate_mimo_cost_usd(model, *input_tokens, *cached_input_tokens, *output_tokens)
                {
                    self.mimo_estimated_usd += cost;
                    if let Some(pricing) = mimo_pricing_for_model(model) {
                        self.mimo_cache_savings_usd += (*cached_input_tokens).min(*input_tokens) as f64
                            * (pricing.cache_miss_input_per_million - pricing.cache_hit_input_per_million)
                            / 1_000_000.0;
                    }
                }
            }
            RuntimeEvent::ContextUsage {
                agent_id,
                model,
                estimated_tokens,
                advertised_limit,
                effective_limit,
                forecast_tokens,
                output_allowance,
                protected_reserve,
                capability_source,
            } => {
                self.contexts.insert(
                    *agent_id,
                    AgentContextView {
                        model: model.clone(),
                        estimated_tokens: *estimated_tokens,
                        advertised_limit: *advertised_limit,
                        effective_limit: *effective_limit,
                        forecast_tokens: *forecast_tokens,
                        output_allowance: *output_allowance,
                        protected_reserve: *protected_reserve,
                        capability_source: capability_source.clone(),
                    },
                );
                let is_lead = self
                    .agents
                    .iter()
                    .find(|agent| agent.id == *agent_id)
                    .is_none_or(|agent| {
                        !agent.role.to_ascii_lowercase().contains("worker")
                            && !agent.role.to_ascii_lowercase().contains("auditor")
                    });
                if is_lead {
                    self.current_context_tokens = *estimated_tokens;
                    self.context_limit = (*advertised_limit).max(1);
                    self.compact_at_tokens = *effective_limit;
                }
            }
            RuntimeEvent::RunPhase { phase, detail } => {
                self.set_phase(*phase);
                self.status = detail.clone();
            }
            RuntimeEvent::ModelCatalog { models, .. } => {
                for model in models {
                    if let Some(existing) = self
                        .catalog
                        .iter_mut()
                        .find(|existing| existing.provider == model.provider && existing.slug == model.slug)
                    {
                        *existing = model.clone();
                    } else {
                        self.catalog.push(model.clone());
                    }
                }
            }
            RuntimeEvent::AgentRetry {
                task_id,
                attempt,
                reason,
            } => self.push_system(
                SystemTone::Warning,
                format!("retrying {task_id} (attempt {attempt}): {reason}"),
            ),
            RuntimeEvent::LeaseChanged {
                task_id, acquired, ..
            } => {
                if *acquired {
                    self.status = format!("working on {task_id}");
                }
            }
            RuntimeEvent::DispatchReceipt { receipt } => {
                if let Some(existing) = self
                    .dispatch_receipts
                    .iter_mut()
                    .find(|existing| existing.receipt_id == receipt.receipt_id)
                {
                    *existing = receipt.clone();
                } else {
                    self.dispatch_receipts.push(receipt.clone());
                }
            }
            RuntimeEvent::BoardChanged { entry } => {
                if let Some(existing) = self.board.iter_mut().find(|item| item.id == entry.id) {
                    *existing = entry.clone();
                } else {
                    self.board.insert(0, entry.clone());
                }
            }
            RuntimeEvent::Compacted { .. } => {
                self.compaction_count = self.compaction_count.saturating_add(1);
                self.push_system(SystemTone::Info, "context compacted");
            }
            RuntimeEvent::Cache {
                hit,
                saved_input_tokens,
                ..
            } => {
                if *hit {
                    self.cache_hits += 1;
                    self.cache_saved_tokens += saved_input_tokens;
                } else {
                    self.cache_misses += 1;
                }
            }
            RuntimeEvent::OfficeHealth {
                active_agents,
                open_tasks,
                blocked_tasks,
                manager_consultations,
            } => {
                self.active_office_agents = *active_agents;
                self.open_office_tasks = *open_tasks;
                self.blocked_office_tasks = *blocked_tasks;
                self.manager_consultations = *manager_consultations;
            }
            RuntimeEvent::ProviderBalance {
                provider,
                currency,
                total,
                reserve_percent,
                ..
            } => {
                self.deepseek_balance = Some(format!("{currency} {total}"));
                self.deepseek_reserve_percent = *reserve_percent;
                self.balance_provider = provider.clone();
            }
            RuntimeEvent::AccountUsage { snapshot } => {
                let parsed =
                    serde_json::from_value::<Vec<minha_core::usage::RateLimitSnapshot>>(snapshot.clone())
                        .or_else(|_| {
                            serde_json::from_value::<minha_core::usage::RateLimitSnapshot>(snapshot.clone())
                                .map(|window| vec![window])
                        });
                match parsed {
                    Ok(windows) => self.account_usage = windows,
                    Err(_) => self.push_system(
                        SystemTone::Warning,
                        "provider account usage could not be decoded; quota remains unavailable",
                    ),
                }
            }
            RuntimeEvent::OfficeMessageChanged {
                room_id,
                sender,
                recipient,
                kind,
                summary,
                deduplicated,
                ..
            } => {
                if !deduplicated {
                    self.items.push(TranscriptItem::Coordination {
                        room_id: room_id.clone(),
                        sender: sender.clone(),
                        recipient: recipient.clone(),
                        kind: kind.clone(),
                        summary: summary.clone(),
                    });
                }
            }
            RuntimeEvent::OfficeRoomChanged { .. } => {}
            RuntimeEvent::BookCatalog { indexed, stale, .. } => {
                self.indexed_books = *indexed;
                self.stale_books = *stale;
            }
            RuntimeEvent::Incident { incident } => {
                self.incidents.push(incident.clone());
                self.push_system(SystemTone::Error, incident.summary.clone());
            }
            RuntimeEvent::SequentialFallback { reason } => {
                self.push_system(SystemTone::Warning, format!("single Mina lane: {reason}"))
            }
            RuntimeEvent::TurnInterrupted { reason } => {
                self.running = false;
                self.state = ExitState::Cancelled;
                self.status = "interrupted".into();
                self.clear_pending_input();
                self.push_system(SystemTone::Warning, reason.clone());
            }
            RuntimeEvent::RunStopped { reason, detail } => {
                self.termination_reason = Some(*reason);
                self.status = termination_status_label(*reason).into();
                if matches!(
                    reason,
                    TerminationReason::BudgetTarget | TerminationReason::ProviderReserve
                ) {
                    self.running = false;
                    self.state = ExitState::UsagePaused;
                }
                if !detail.is_empty() && !matches!(reason, TerminationReason::Completed) {
                    self.push_system(SystemTone::Info, detail.clone());
                }
            }
            RuntimeEvent::SessionFinished {
                state,
                model,
                agents_used,
                ..
            } => {
                self.active_run = Some(envelope.run_id);
                self.running = false;
                self.state = *state;
                if *state != ExitState::UsagePaused {
                    self.termination_reason = None;
                }
                if *state != ExitState::UsagePaused
                    || !matches!(
                        self.termination_reason,
                        Some(TerminationReason::BudgetTarget | TerminationReason::ProviderReserve)
                    )
                {
                    self.status = state_label(*state).into();
                }
                self.set_phase(RunPhase::Complete);
                // finish_agent_outcome_with_state emits this event for every
                // AgentResult it settles, including a pause: ApprovalRequired
                // and NeedsInput mean "waiting for the answer to the request
                // this very event is arriving alongside," not "over." Wiping
                // pending_request here made an approval/question card flash
                // and vanish the instant it appeared, since a run pausing for
                // input always produces this event too. Only a genuinely
                // final state should drop the pending request.
                if !matches!(*state, ExitState::ApprovalRequired | ExitState::NeedsInput) {
                    self.clear_pending_input();
                }
                if let Some(model) = model {
                    self.model = model.clone();
                }
                self.push_system(
                    if *state == ExitState::Succeeded {
                        SystemTone::Success
                    } else {
                        SystemTone::Info
                    },
                    format!("{} · {} agents used", state_label(*state), agents_used),
                );
            }
            RuntimeEvent::Warning { message } => {
                self.last_warning = Some(message.clone());
                self.push_system(SystemTone::Warning, message.clone());
            }
            RuntimeEvent::RoutingDecision {
                mode,
                reason,
                provider,
                model,
            } => {
                self.last_routing = Some(RouteView {
                    mode: mode.clone(),
                    reason: reason.clone(),
                    provider: provider.clone(),
                    model: model.clone(),
                });
            }
            RuntimeEvent::Error { state, message } => {
                self.running = false;
                self.state = *state;
                self.status = state_label(*state).into();
                self.clear_pending_input();
                self.show_recovery(state_label(*state), message.clone());
                self.push_system(SystemTone::Error, message.clone());
            }
            RuntimeEvent::SessionForked { .. }
            | RuntimeEvent::SessionRenamed { .. }
            | RuntimeEvent::SessionArchived
            | RuntimeEvent::ActivityStarted { .. }
            | RuntimeEvent::ActivityUpdated { .. }
            | RuntimeEvent::ActivityFinished { .. }
            | RuntimeEvent::MemoryChanged { .. }
            | RuntimeEvent::MemoryRetrieved { .. }
            | RuntimeEvent::ProviderState { .. }
            | RuntimeEvent::Legacy { .. } => {}
        }
        if self.scroll_state.auto_follow {
            self.scroll_state.follow();
        }
    }

    pub(crate) fn load_session(&mut self, run: &RunRecord, events: &[EventEnvelope]) {
        self.reset_session();
        self.active_run = Some(run.id);
        self.state = run.state;
        self.status = state_label(run.state).into();
        self.model = run.model.clone().unwrap_or_else(|| "gpt-5.6-luna".into());
        for event in events {
            self.apply_event(event);
        }
        self.input_tokens = run.input_tokens;
        self.output_tokens = run.output_tokens;
        self.running = run.state == ExitState::Running;
    }

    pub(crate) fn set_sessions(&mut self, sessions: Vec<RunRecord>) {
        self.sessions = sessions;
        self.selected_session = self.selected_session.min(self.sessions.len().saturating_sub(1));
    }

    pub(crate) fn set_diagnostics(&mut self, diagnostics: Vec<Diagnostic>) {
        self.diagnostics = diagnostics;
        self.overlay = Some(Overlay::Doctor);
    }

    pub(crate) fn set_board(&mut self, board: Vec<BoardEntryView>) {
        self.board = board;
        self.selected_board = self.selected_board.min(self.board.len().saturating_sub(1));
    }

    pub(crate) fn set_usage_totals(&mut self, usage: UsageTotals) {
        self.input_tokens = usage.session_input;
        self.output_tokens = usage.session_output;
        self.cached_input_tokens = usage.session_cached_input;
        self.cache_write_tokens = usage.session_cache_write;
        self.reasoning_output_tokens = usage.session_reasoning_output;
        self.lifetime_input_tokens = usage.lifetime_input;
        self.lifetime_output_tokens = usage.lifetime_output;
        self.lifetime_cached_input_tokens = usage.lifetime_cached_input;
        self.lifetime_cache_write_tokens = usage.lifetime_cache_write;
        self.lifetime_reasoning_output_tokens = usage.lifetime_reasoning_output;
    }

    pub(crate) fn worker_models_summary(&self) -> String {
        let mut models = self
            .agents
            .iter()
            .filter(|agent| {
                let role = agent.role.to_ascii_lowercase();
                role.contains("worker") || role.contains("auditor") || role.contains("spark")
            })
            .map(|agent| identity_model_label(Some(&agent.role), &agent.model))
            .collect::<Vec<_>>();
        if models.is_empty() {
            models.extend(
                self.dispatch_receipts
                    .iter()
                    .map(|receipt| identity_model_label(Some(&receipt.role), &receipt.model)),
            );
        }
        models.sort();
        models.dedup();
        if models.is_empty() {
            "no worker dispatched yet".into()
        } else {
            models.join(", ")
        }
    }

    pub(crate) fn push_status_card(&mut self, lines: Vec<String>) {
        self.items.push(TranscriptItem::Status { lines });
        self.invalidate_transcript_layout();
        if self.scroll_state.auto_follow {
            self.scroll_state.follow();
        }
    }

    pub(crate) fn set_login_overlay(
        &mut self,
        verification_uri: impl Into<String>,
        user_code: impl Into<String>,
        message: impl Into<String>,
    ) {
        self.overlay = Some(Overlay::Login {
            verification_uri: verification_uri.into(),
            user_code: user_code.into(),
            message: message.into(),
        });
    }

    pub(crate) fn update_login_message(&mut self, message: impl Into<String>) {
        if let Some(Overlay::Login { message: current, .. }) = &mut self.overlay {
            *current = message.into();
        }
    }

    pub(crate) fn push_shell_result(&mut self, display: String, output: String, exit_code: Option<i32>) {
        let id = EventAgentId::new();
        self.items.push(TranscriptItem::Tool {
            agent_id: id,
            call_id: format!("local-{id}"),
            name: "local".into(),
            arguments: display,
            output,
            exit_code,
            running: false,
            expanded: self.details_expanded,
        });
        self.invalidate_transcript_layout();
        if self.scroll_state.auto_follow {
            self.scroll_state.follow();
        }
    }

    pub(crate) fn push_diff(&mut self, diff: String) {
        if diff.trim().is_empty() {
            self.push_system(SystemTone::Info, "working tree has no diff");
        } else {
            self.items.push(TranscriptItem::Diff {
                path: None,
                diff,
                expanded: true,
            });
            self.invalidate_transcript_layout();
        }
        if self.scroll_state.auto_follow {
            self.scroll_state.follow();
        }
    }

    pub(crate) fn push_system(&mut self, tone: SystemTone, text: impl Into<String>) {
        let text = text.into();
        if matches!(
            self.items.last(),
            Some(TranscriptItem::System { tone: previous_tone, text: previous })
                if *previous_tone == tone && previous == &text
        ) {
            return;
        }
        if matches!(tone, SystemTone::Warning | SystemTone::Error) {
            self.toast = Some(Toast::new(tone, text.clone()));
        }
        self.items.push(TranscriptItem::System { tone, text });
        self.invalidate_transcript_layout();
    }

    fn invalidate_transcript_layout(&mut self) {
        self.transcript_revision = self.transcript_revision.wrapping_add(1).max(1);
    }

    pub(crate) fn visible_items(&self) -> impl Iterator<Item = &TranscriptItem> {
        self.items.iter().filter(|item| match self.focused_agent {
            Some(agent_id) => item.agent_id().is_none_or(|id| id == agent_id),
            None => match item.agent_id() {
                Some(agent_id) => self
                    .agents
                    .iter()
                    .find(|agent| agent.id == agent_id)
                    .is_none_or(|agent| !agent.role.starts_with("Spark")),
                None => true,
            },
        })
    }

    pub(crate) fn context_percent(&self) -> f64 {
        (self.current_context_tokens as f64 / self.context_limit as f64 * 100.0).min(100.0)
    }

    pub(crate) fn context_left_percent(&self) -> f64 {
        100.0 - self.context_percent()
    }

    pub(crate) fn transcript_text(&self) -> String {
        let mut out = String::from("# Minha transcript\n\n");
        out.push_str("## Replay metadata\n\n");
        out.push_str(&format!(
            "- Lead: {}\n- State: {}\n- Transcript mode: raw Markdown export\n\n",
            identity_model_label(Some("Mina"), &self.model),
            state_label(self.state),
        ));
        for item in &self.items {
            match item {
                TranscriptItem::User { text, steering } => {
                    out.push_str(if *steering {
                        "## Steering\n\n"
                    } else {
                        "## User\n\n"
                    });
                    out.push_str(text);
                    out.push_str("\n\n");
                }
                TranscriptItem::Assistant {
                    agent_id, role, text, ..
                } => {
                    let model = self
                        .agents
                        .iter()
                        .find(|agent| agent.id == *agent_id)
                        .map(|agent| agent.model.as_str())
                        .unwrap_or("model unavailable");
                    out.push_str(&format!(
                        "## {}\n\n{text}\n\n",
                        identity_model_label(Some(role), model)
                    ));
                }
                TranscriptItem::Tool {
                    name,
                    arguments,
                    output,
                    ..
                } => {
                    out.push_str(&format!(
                        "### Tool: {name} {arguments}\n\n```text\n{output}\n```\n\n"
                    ));
                }
                TranscriptItem::Diff { diff, .. } => {
                    out.push_str(&format!("### Diff\n\n```diff\n{diff}\n```\n\n"));
                }
                TranscriptItem::System { text, .. } => {
                    out.push_str(&format!("> {text}\n\n"));
                }
                TranscriptItem::Status { lines } => {
                    out.push_str("## Status\n\n");
                    for line in lines {
                        out.push_str(&format!("- {line}\n"));
                    }
                    out.push('\n');
                }
                TranscriptItem::Coordination {
                    room_id,
                    sender,
                    recipient,
                    kind,
                    summary,
                } => {
                    out.push_str(&format!(
                        "### Coordination: {kind}\n\n{summary}\n\n- {sender} to {recipient}\n- Room: {room_id}\n\n"
                    ));
                }
            }
        }
        if let Some(clarification) = &self.clarification {
            out.push_str(&format!(
                "## Issue clarification\n\n- Status: {:?}\n",
                clarification.status
            ));
            if let Some(brief) = &clarification.brief {
                out.push_str(&format!("\n{}\n", minha_core::clarify::render_brief(brief)));
            }
            out.push('\n');
        }
        out
    }

    fn reset_session(&mut self) {
        self.items.clear();
        self.invalidate_transcript_layout();
        self.agents.clear();
        self.contexts.clear();
        self.plan.clear();
        self.todos.clear();
        self.todo_active = 0;
        self.todo_blocked = 0;
        self.todo_completed = 0;
        self.todo_stale_agents = 0;
        self.todo_active_goals.clear();
        self.todo_blocked_work.clear();
        self.todo_recently_completed.clear();
        self.board.clear();
        self.catalog.clear();
        self.dispatch_receipts.clear();
        self.last_routing = None;
        self.last_warning = None;
        self.incidents.clear();
        self.pending_request = None;
        self.clarification = None;
        self.clarification_answers.clear();
        self.selected_clarification_question = 0;
        self.selected_clarification_option = 0;
        self.clarification_note_open = false;
        self.active_run = None;
        self.focused_agent = None;
        self.running = false;
        self.state = ExitState::Pending;
        self.termination_reason = None;
        self.status = "ready".into();
        self.input_tokens = 0;
        self.output_tokens = 0;
        self.cached_input_tokens = 0;
        self.cache_write_tokens = 0;
        self.reasoning_output_tokens = 0;
        self.deepseek_estimated_usd = 0.0;
        self.deepseek_cache_savings_usd = 0.0;
        self.mimo_estimated_usd = 0.0;
        self.mimo_cache_savings_usd = 0.0;
        self.active_office_agents = 0;
        self.open_office_tasks = 0;
        self.blocked_office_tasks = 0;
        self.manager_consultations = 0;
        self.current_context_tokens = 0;
        self.compaction_count = 0;
        self.queued_steering = 0;
        self.message_target = None;
        self.scroll_state = ScrollState::default();
        self.overlay = None;
        self.last_sequence.clear();
        self.set_phase(RunPhase::Complete);
        self.selected_agent = 0;
        self.selected_task = 0;
        self.selected_board = 0;
        self.selected_problem = 0;
    }

    pub(crate) fn prepare_fresh_session(&mut self) {
        self.reset_session();
        self.status = "starting fresh retry".into();
    }

    fn set_phase(&mut self, phase: RunPhase) {
        if self.phase != phase {
            self.phase = phase;
            self.phase_started_at = Instant::now();
        }
    }
}

/// Keep human identity and exact vendor/model label together everywhere the
/// TUI introduces an agent.  Roles win because a DeepSeek-backed Spark worker
/// is still Spark; unknown roles fall back to the model's honest provider or
/// slug rather than pretending to be Mina.
pub(crate) fn persona_model_label(persona: &str, model: &str) -> String {
    format!("{persona} · {model}")
}

pub(crate) fn persona_for(role: Option<&str>, model: &str) -> String {
    let role = role.unwrap_or_default();
    for (marker, persona) in [
        ("Spark", "Spark"),
        ("Terra", "Terra"),
        ("Sol", "Sol"),
        ("Mina", "Mina"),
    ] {
        if role.contains(marker) {
            return persona.into();
        }
    }
    let lower = model.to_ascii_lowercase();
    for (marker, persona) in [
        ("spark", "Spark"),
        ("terra", "Terra"),
        ("sol", "Sol"),
        ("luna", "Mina"),
    ] {
        if lower.contains(marker) {
            return persona.into();
        }
    }
    model
        .split('/')
        .next()
        .filter(|provider| !provider.is_empty())
        .unwrap_or("Model")
        .to_owned()
}

pub(crate) fn identity_model_label(role: Option<&str>, model: &str) -> String {
    persona_model_label(&persona_for(role, model), model)
}

fn toggle_overlay(current: &Option<Overlay>, target: Overlay) -> Option<Overlay> {
    if current.as_ref() == Some(&target) {
        None
    } else {
        Some(target)
    }
}

fn move_index(index: usize, length: usize, delta: isize) -> usize {
    if length == 0 {
        return 0;
    }
    if delta < 0 {
        index.saturating_sub(delta.unsigned_abs())
    } else {
        index.saturating_add(delta as usize).min(length - 1)
    }
}

fn compact_json(value: &serde_json::Value) -> String {
    let encoded = value.to_string();
    if encoded.len() > 240 {
        format!("{}…", encoded.chars().take(240).collect::<String>())
    } else {
        encoded
    }
}

fn strip_control_tags(text: &str) -> String {
    let mut visible = text.to_owned();
    for tag in ["mode", "clarification", "memory", "plan", "todos"] {
        let opening = format!("<minha-{tag}>");
        let closing = format!("</minha-{tag}>");
        while let Some(start) = visible.find(&opening) {
            if let Some(relative_end) = visible[start + opening.len()..].find(&closing) {
                let end = start + opening.len() + relative_end + closing.len();
                visible.replace_range(start..end, "");
            } else {
                visible.truncate(start);
                break;
            }
        }
        visible = visible.replace(&closing, "");
    }
    visible.trim().to_owned()
}

fn internal_role(role: &str) -> bool {
    let role = role.to_ascii_lowercase();
    role.contains("intent classifier")
        || role.contains("issue clarifier")
        || role.contains("ambiguity consultant")
        || role.contains("memory extractor")
}

fn split_argv(input: &str) -> Result<Vec<String>, String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escaped = false;
    for character in input.trim().chars() {
        if escaped {
            current.push(character);
            escaped = false;
            continue;
        }
        if character == '\\' {
            escaped = true;
            continue;
        }
        if let Some(active) = quote {
            if character == active {
                quote = None;
            } else {
                current.push(character);
            }
            continue;
        }
        match character {
            '\'' | '"' => quote = Some(character),
            ' ' | '\t' => {
                if !current.is_empty() {
                    args.push(std::mem::take(&mut current));
                }
            }
            '|' | '>' | '<' | ';' | '&' | '`' | '$' => {
                return Err(format!(
                    "shell operator {character:?} is not supported; use executable plus arguments"
                ));
            }
            _ => current.push(character),
        }
    }
    if escaped || quote.is_some() {
        return Err("unterminated quote or escape in direct command".into());
    }
    if !current.is_empty() {
        args.push(current);
    }
    Ok(args)
}

fn terminal_agent_state(state: AgentState) -> bool {
    matches!(
        state,
        AgentState::Completed | AgentState::Failed | AgentState::Cancelled
    )
}

fn word_class(grapheme: &str) -> u8 {
    if grapheme.chars().all(char::is_whitespace) {
        0
    } else if grapheme
        .chars()
        .all(|character| character.is_alphanumeric() || character == '_')
    {
        1
    } else {
        2
    }
}

fn previous_word_boundary(text: &str, cursor: usize) -> usize {
    let graphemes = text[..cursor].grapheme_indices(true).collect::<Vec<_>>();
    let mut index = graphemes.len();
    while index > 0 && word_class(graphemes[index - 1].1) == 0 {
        index -= 1;
    }
    let Some(class) = index.checked_sub(1).map(|index| word_class(graphemes[index].1)) else {
        return 0;
    };
    while index > 0 && word_class(graphemes[index - 1].1) == class {
        index -= 1;
    }
    graphemes.get(index).map_or(0, |(offset, _)| *offset)
}

fn next_word_boundary(text: &str, cursor: usize) -> usize {
    let graphemes = text[cursor..].grapheme_indices(true).collect::<Vec<_>>();
    let mut index = 0;
    while index < graphemes.len() && word_class(graphemes[index].1) == 0 {
        index += 1;
    }
    let Some(class) = graphemes.get(index).map(|(_, grapheme)| word_class(grapheme)) else {
        return text.len();
    };
    while index < graphemes.len() && word_class(graphemes[index].1) == class {
        index += 1;
    }
    graphemes
        .get(index)
        .map_or(text.len(), |(offset, _)| cursor + *offset)
}

/// Vim `w`: advance to the first grapheme of the next non-whitespace word.
/// `word_class` intentionally keeps punctuation runs distinct, matching the
/// editor's existing word-navigation behavior without ever splitting a
/// grapheme cluster.
fn vim_next_word_start(text: &str, cursor: usize) -> usize {
    let graphemes = text[cursor..].grapheme_indices(true).collect::<Vec<_>>();
    let Some((_, first)) = graphemes.first() else {
        return text.len();
    };
    let mut index = 0;
    let class = word_class(first);
    if class != 0 {
        while index < graphemes.len() && word_class(graphemes[index].1) == class {
            index += 1;
        }
    }
    while index < graphemes.len() && word_class(graphemes[index].1) == 0 {
        index += 1;
    }
    graphemes
        .get(index)
        .map_or(text.len(), |(offset, _)| cursor + *offset)
}

/// Vim `e`: land on the final grapheme of the current (or next) word.
fn vim_word_end(text: &str, cursor: usize) -> usize {
    let graphemes = text[cursor..].grapheme_indices(true).collect::<Vec<_>>();
    let mut index = 0;
    while index < graphemes.len() && word_class(graphemes[index].1) == 0 {
        index += 1;
    }
    let Some(class) = graphemes.get(index).map(|(_, grapheme)| word_class(grapheme)) else {
        return text.len();
    };
    let start = index;
    while index < graphemes.len() && word_class(graphemes[index].1) == class {
        index += 1;
    }
    let last = index.saturating_sub(1).max(start);
    cursor + graphemes[last].0
}

fn tool_activity_kind(name: &str) -> &'static str {
    match name {
        "read_files" => "explored",
        "search" => "searched",
        "apply_patch" => "edited",
        "quality" | "exec" => "checks",
        "hive" => "delegated",
        _ => "tools",
    }
}

fn parse_toggle(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "on" | "true" | "enable" | "enabled" => Some(true),
        "off" | "false" | "disable" | "disabled" => Some(false),
        _ => None,
    }
}

pub(crate) fn state_label(state: ExitState) -> &'static str {
    match state {
        ExitState::Pending => "ready",
        ExitState::Running => "working",
        ExitState::Succeeded => "complete",
        ExitState::Failed => "failed",
        ExitState::Cancelled => "interrupted",
        ExitState::Blocked => "blocked",
        ExitState::Inconclusive => "inconclusive",
        ExitState::NeedsInput => "needs input",
        ExitState::UsagePaused => "usage paused",
        ExitState::ApprovalRequired => "approval required",
        ExitState::AuthUnavailable => "login required",
        ExitState::ModelUnavailable => "model unavailable",
    }
}

pub(crate) fn termination_status_label(reason: TerminationReason) -> &'static str {
    match reason {
        TerminationReason::BudgetTarget => "session budget paused — 5% recovery reserve held",
        TerminationReason::ProviderReserve => "provider reserve paused",
        TerminationReason::Completed => "complete",
        TerminationReason::ContextBoundary => "context boundary reached",
        TerminationReason::ToolLimit => "tool limit reached",
        TerminationReason::TurnLimit => "turn limit reached",
        TerminationReason::SafetyPolicy => "stopped by safety policy",
        TerminationReason::Interrupted => "interrupted",
        TerminationReason::UserPaused => "paused",
        TerminationReason::Blocked => "blocked",
        TerminationReason::RetryScheduled => "retry scheduled",
        TerminationReason::Forked => "forked",
        TerminationReason::RecoveryRequired => "recovery required",
        TerminationReason::InvalidEmptyResponse => "empty response",
        TerminationReason::ProviderFailure => "provider failure",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_escape_never_interrupts_and_double_escape_requests_safe_pause() {
        let mut app = App::new(PathBuf::from("/workspace/minha"), 272_000);
        let run_id = RunId::new();
        app.running = true;
        app.active_run = Some(run_id);
        app.update(AppAction::Escape).expect("first escape");
        assert_eq!(app.take_submission(), None);
        app.update(AppAction::Escape).expect("second escape");
        assert_eq!(app.take_submission(), Some(Submission::Pause { run_id }));
        app.update(AppAction::Interrupt).expect("explicit interrupt");
        assert_eq!(app.take_submission(), Some(Submission::Interrupt { run_id }));
    }

    #[test]
    fn escape_protects_a_pending_request_draft_but_still_allows_pausing() {
        let mut app = App::new(PathBuf::from("/workspace/minha"), 272_000);
        let run_id = RunId::new();
        app.running = true;
        app.active_run = Some(run_id);
        app.pending_request = Some(PendingRequest {
            id: RequestId::new(),
            question: "which plan?".into(),
            options: vec!["a".into(), "b".into()],
            approval: false,
            reason: None,
            command: None,
        });
        app.input = "half-typed answer".into();
        app.input_cursor = app.input.len();

        app.update(AppAction::Escape)
            .expect("first escape clears the draft");
        assert_eq!(
            app.input, "",
            "a typed answer must never be silently discarded by the pause shortcut"
        );
        assert!(
            app.pending_request.is_some(),
            "the request itself is untouched by Esc"
        );
        assert_eq!(
            app.take_submission(),
            None,
            "clearing a draft must not pause the run"
        );

        // With no draft left, Esc is free to reach the double-escape-pauses
        // flow below it — the inline card has nothing left to hide, unlike
        // the old centered modal.
        app.update(AppAction::Escape)
            .expect("second escape starts the pause countdown");
        assert_eq!(app.take_submission(), None);
        app.update(AppAction::Escape)
            .expect("third escape confirms the pause");
        assert_eq!(app.take_submission(), Some(Submission::Pause { run_id }));
    }

    #[test]
    fn escape_clears_input_cursor_and_completion_popup() {
        let mut app = App::new(PathBuf::from("/workspace/minha"), 272_000);
        app.input = "hello".into();
        app.input_cursor = 5;
        app.completion_items = vec![CompletionEntry::path("activity".into(), "drawer")];
        app.update(AppAction::Escape).expect("escape clears the draft");
        assert_eq!(app.input, "");
        assert_eq!(app.input_cursor, 0, "cursor must reset with the buffer");
        assert!(app.completion_items.is_empty(), "popup must not float after Esc");
        // The panic trigger from the audit: typing again after Esc must not
        // slice past the buffer end.
        app.update(AppAction::Input('x')).expect("typing after escape");
        assert_eq!(app.input, "x");
        assert_eq!(app.input_cursor, 1);
    }
    use minha_core::clarify::{analyze, apply_answers, make_fallback_batch, prepare_brief};
    use minha_core::protocol::RuntimeEvent;

    fn app() -> App {
        App::new(PathBuf::from("/tmp/project"), 128_000)
    }

    fn clarification_event(run: RunId, reviewing: bool) -> EventEnvelope {
        let mut clarification = analyze("it doesn't work", "auto");
        if reviewing {
            apply_answers(
                &mut clarification,
                &[("$action".into(), "use_best_judgment".into())],
            );
            prepare_brief(&mut clarification, "it doesn't work");
        } else {
            let mut batch = make_fallback_batch(&clarification);
            batch.questions.truncate(1);
            clarification.pending_batch = Some(batch);
        }
        EventEnvelope::new(run, 1, RuntimeEvent::ClarificationUpdated { clarification })
    }

    #[test]
    fn input_stays_active_and_queues_steering() {
        let mut app = app();
        let run_id = RunId::new();
        app.active_run = Some(run_id);
        app.running = true;
        app.input = "focus tests".into();
        app.update(AppAction::Submit)
            .expect("test operation should succeed");
        assert!(matches!(
            app.take_submission(),
            Some(Submission::Steer { run_id: id, .. }) if id == run_id
        ));
    }

    #[test]
    fn streamed_text_reduces_into_one_item() {
        let mut app = app();
        let run = RunId::new();
        let agent = EventAgentId::new();
        let item = ItemId::new();
        for (sequence, delta) in ["hello ", "world"].into_iter().enumerate() {
            app.apply_event(&EventEnvelope::new(
                run,
                sequence as u64,
                RuntimeEvent::TextDelta {
                    agent_id: agent,
                    item_id: item,
                    delta: delta.into(),
                },
            ));
        }
        assert!(matches!(
            &app.items[0],
            TranscriptItem::Assistant { text, .. } if text == "hello world"
        ));
    }

    #[test]
    fn direct_commands_reject_shell_operators() {
        assert!(split_argv("cargo test | tee out").is_err());
        assert_eq!(
            split_argv("cargo test --workspace").expect("test operation should succeed"),
            ["cargo", "test", "--workspace"]
        );
    }

    #[test]
    fn quality_and_github_commands_use_structured_local_tools() {
        let mut app = app();
        app.input = "/lint".into();
        app.update(AppAction::Submit).expect("lint command");
        assert_eq!(
            app.take_submission(),
            Some(Submission::Quality {
                action: "lint".into()
            })
        );

        app.input = "/gh pr 17".into();
        app.update(AppAction::Submit).expect("GitHub command");
        assert_eq!(
            app.take_submission(),
            Some(Submission::GitHub {
                action: "pr".into(),
                number: Some(17)
            })
        );
    }

    #[test]
    fn history_recalls_recent_distinct_inputs() {
        let mut app = app();
        app.input = "/status".into();
        app.update(AppAction::Submit)
            .expect("test operation should succeed");
        app.input = "/usage".into();
        app.update(AppAction::Submit)
            .expect("test operation should succeed");

        app.update(AppAction::HistoryPrevious)
            .expect("test operation should succeed");
        assert_eq!(app.input, "/usage");
        app.update(AppAction::HistoryPrevious)
            .expect("test operation should succeed");
        assert_eq!(app.input, "/status");
    }

    #[test]
    fn editor_moves_and_deletes_whole_graphemes() {
        let mut app = App::new(PathBuf::from("."), 272_000);
        app.update(AppAction::Input('a')).expect("insert a");
        app.update(AppAction::Input('é')).expect("insert e acute");
        app.update(AppAction::Input('界')).expect("insert wide character");
        app.update(AppAction::CursorLeft).expect("move left");
        app.update(AppAction::Backspace).expect("delete grapheme");
        assert_eq!(app.input, "a界");
        assert_eq!(app.input_cursor, 1);
        app.update(AppAction::Delete).expect("delete wide grapheme");
        assert_eq!(app.input, "a");
    }

    #[test]
    fn editor_supports_multiline_columns_words_paste_and_undo() {
        let mut app = app();
        app.update(AppAction::Paste("ab界\nxy\nlong".into()))
            .expect("paste");
        assert_eq!(app.input, "ab界\nxy\nlong");
        app.update(AppAction::CursorUp).expect("up");
        assert_eq!(&app.input[..app.input_cursor], "ab界\nxy");
        app.update(AppAction::CursorUp).expect("up wide line");
        assert_eq!(&app.input[..app.input_cursor], "ab界");
        app.update(AppAction::WordLeft).expect("word left");
        assert_eq!(app.input_cursor, 0);
        app.update(AppAction::WordRight).expect("word right");
        assert_eq!(&app.input[..app.input_cursor], "ab界");
        app.update(AppAction::DeleteWordBackward).expect("delete word");
        assert_eq!(app.input, "\nxy\nlong");
        app.update(AppAction::Undo).expect("undo");
        assert_eq!(app.input, "ab界\nxy\nlong");
        app.update(AppAction::Redo).expect("redo");
        assert_eq!(app.input, "\nxy\nlong");
    }

    #[test]
    fn vim_composer_keeps_edits_local_and_supports_the_bounded_core_commands() {
        let mut app = app();
        app.tui_settings.vim_scroll = true;

        app.input = "ax界".into();
        app.input_cursor = "a".len();
        app.update(AppAction::VimNormal).expect("enter normal");
        app.update(AppAction::VimDeleteChar)
            .expect("x deletes one grapheme");
        assert_eq!(app.input, "a界");
        assert_eq!(app.vim_mode, VimMode::Normal);

        app.input = "first\nsecond".into();
        app.input_cursor = 0;
        app.update(AppAction::VimDeletePending).expect("d pending");
        assert_eq!(app.vim_mode, VimMode::DeletePending);
        app.update(AppAction::VimDeleteLine).expect("dd deletes line");
        assert_eq!(app.input, "second");

        app.input = "abcdef".into();
        app.input_cursor = 2;
        app.update(AppAction::VimDeleteToLineEnd)
            .expect("D deletes to end");
        assert_eq!(app.input, "ab");
        app.update(AppAction::Undo).expect("undo D");
        assert_eq!(app.input, "abcdef");
        app.update(AppAction::Redo).expect("redo D");
        assert_eq!(app.input, "ab");
        app.update(AppAction::Undo).expect("undo D again");
        app.update(AppAction::VimChangeToLineEnd)
            .expect("C changes to end");
        assert_eq!(app.vim_mode, VimMode::Insert);
        app.update(AppAction::Input('z')).expect("insert after C");
        assert_eq!(app.input, "abz");

        app.input = "first\nsecond".into();
        app.input_cursor = 0;
        app.update(AppAction::VimNormal).expect("normal for yy");
        app.update(AppAction::VimYankPending).expect("y pending");
        app.update(AppAction::VimYankLine).expect("yy yanks line");
        app.update(AppAction::CursorDown).expect("move to second line");
        app.update(AppAction::VimPasteLine).expect("p pastes yanked line");
        assert_eq!(app.input, "first\nsecond\nfirst");

        app.input = "first".into();
        app.input_cursor = 0;
        app.update(AppAction::VimOpenBelow).expect("o opens below");
        assert_eq!(app.vim_mode, VimMode::Insert);
        app.update(AppAction::Input('x')).expect("type in opened line");
        assert_eq!(app.input, "first\nx");
        app.update(AppAction::VimNormal).expect("Esc returns to normal");
        app.update(AppAction::VimOpenAbove).expect("O opens above");
        assert_eq!(app.vim_mode, VimMode::Insert);
        assert_eq!(app.input, "first\n\nx");

        app.input = "first\nsecond".into();
        app.input_cursor = "first".len();
        app.update(AppAction::VimAppend)
            .expect("a stays at the current line end");
        assert_eq!(app.input_cursor, "first".len());
        assert_eq!(app.vim_mode, VimMode::Insert);

        app.history = vec!["a prior draft".into()];
        app.input = "current draft".into();
        app.input_cursor = 0;
        app.update(AppAction::VimMoveUp)
            .expect("k stays in the current composer at its top edge");
        assert_eq!(app.input, "current draft");
        app.input_cursor = app.input.len();
        app.update(AppAction::VimMoveDown)
            .expect("j stays in the current composer at its bottom edge");
        assert_eq!(app.input, "current draft");
        assert_eq!(app.take_submission(), None, "Vim actions never dispatch a run");
    }

    #[test]
    fn vim_word_motions_and_insert_undo_stay_grapheme_safe() {
        let mut app = app();
        app.tui_settings.vim_scroll = true;
        app.input = "alpha  界! beta".into();
        app.input_cursor = 0;
        app.update(AppAction::VimNormal).expect("enter Normal mode");

        app.update(AppAction::VimWordForward)
            .expect("w reaches the next word");
        assert_eq!(&app.input[app.input_cursor..], "界! beta");
        app.update(AppAction::VimWordEnd)
            .expect("e reaches a grapheme boundary");
        assert_eq!(&app.input[app.input_cursor..], "界! beta");
        app.update(AppAction::VimWordForward)
            .expect("w crosses punctuation");
        assert_eq!(&app.input[app.input_cursor..], "! beta");
        app.update(AppAction::VimWordBackward)
            .expect("b returns to word start");
        assert_eq!(&app.input[app.input_cursor..], "界! beta");
        app.update(AppAction::CursorHome).expect("0 reaches line start");
        assert_eq!(app.input_cursor, 0);
        app.update(AppAction::CursorEnd).expect("$ reaches line end");
        assert_eq!(app.input_cursor, app.input.len());

        app.input.clear();
        app.input_cursor = 0;
        app.update(AppAction::VimInsert).expect("enter Insert mode");
        for character in ['a', '界', 'b'] {
            app.update(AppAction::Input(character))
                .expect("type into one Vim insert session");
        }
        app.update(AppAction::VimNormal).expect("leave Insert mode");
        app.update(AppAction::Undo)
            .expect("one undo reverts the insert session");
        assert!(app.input.is_empty());
        app.update(AppAction::Redo)
            .expect("redo restores the insert session");
        assert_eq!(app.input, "a界b");
    }

    #[test]
    fn account_usage_events_and_worker_model_hints_use_observed_runtime_data() {
        let mut app = app();
        let run = RunId::new();
        let snapshot = minha_core::usage::RateLimitSnapshot {
            limit_id: "chatgpt".into(),
            limit_name: Some("ChatGPT".into()),
            primary: Some(minha_core::usage::RateLimitWindow {
                used_percent: 42.0,
                window_minutes: Some(60),
                resets_at: None,
            }),
            secondary: None,
            credits: None,
        };
        app.apply_event(&EventEnvelope::new(
            run,
            1,
            RuntimeEvent::AccountUsage {
                snapshot: serde_json::to_value(vec![snapshot]).expect("serialize account usage"),
            },
        ));
        assert_eq!(app.account_usage.len(), 1);
        assert_eq!(
            app.account_usage[0]
                .primary
                .as_ref()
                .map(|window| window.used_percent),
            Some(42.0)
        );
        assert_eq!(app.worker_models_summary(), "no worker dispatched yet");
        app.agents.push(AgentView {
            id: EventAgentId::new(),
            role: "Spark worker".into(),
            model: "local/verified-worker".into(),
            state: AgentState::Working,
            detail: String::new(),
        });
        assert!(app.worker_models_summary().contains("local/verified-worker"));
    }

    #[test]
    fn large_paste_stays_collapsed_until_the_user_expands_it() {
        let mut app = app();
        let pasted = (0..13)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        app.update(AppAction::Paste(pasted)).expect("paste");
        assert!(matches!(
            app.paste_summary,
            Some(PasteSummary {
                lines: 13,
                expanded: false,
                ..
            })
        ));
        app.update(AppAction::ToggleDetails)
            .expect("expand pasted content");
        assert!(app.paste_summary.as_ref().is_some_and(|summary| summary.expanded));
    }

    #[test]
    fn tab_completes_commands_without_opening_the_drawer() {
        let mut app = app();
        app.input = "/statu".into();
        app.input_cursor = app.input.len();
        app.update(AppAction::Complete).expect("complete");
        assert_eq!(app.input, "/status ");
        assert!(!app.drawer_visible);

        app.input = "/m".into();
        app.input_cursor = app.input.len();
        app.update(AppAction::Complete).expect("ambiguous complete");
        assert!(app.completion_items.iter().any(|entry| entry.value == "memory"));
        assert!(app.completion_items.iter().any(|entry| entry.value == "memories"));
    }

    #[test]
    fn details_toggle_only_the_nearest_semantic_activity_group() {
        let mut app = app();
        let agent_id = EventAgentId::new();
        for (name, call_id) in [
            ("read_files", "read-a"),
            ("read_files", "read-b"),
            ("search", "search"),
        ] {
            app.items.push(TranscriptItem::Tool {
                agent_id,
                call_id: call_id.into(),
                name: name.into(),
                arguments: "{}".into(),
                output: "done".into(),
                exit_code: Some(0),
                running: false,
                expanded: false,
            });
        }
        app.update(AppAction::ToggleDetails).expect("toggle details");
        assert!(matches!(
            app.items[2],
            TranscriptItem::Tool { expanded: true, .. }
        ));
        assert!(matches!(
            app.items[0],
            TranscriptItem::Tool { expanded: false, .. }
        ));
        assert!(matches!(
            app.items[1],
            TranscriptItem::Tool { expanded: false, .. }
        ));
    }

    #[test]
    fn compact_command_uses_runtime_control() {
        let mut app = app();
        let run_id = RunId::new();
        app.active_run = Some(run_id);
        app.input = "/compact".into();
        app.update(AppAction::Submit)
            .expect("test operation should succeed");

        assert_eq!(app.take_submission(), Some(Submission::Compact { run_id }));
    }

    #[test]
    fn toggle_drawer_shows_and_hides_without_cycling_tabs() {
        // Shift-Tab used to cycle through every tab before finally closing;
        // it is now a plain show/hide toggle that leaves the tab unchanged.
        let mut app = app();
        app.drawer_tab = DrawerTab::Board;
        app.update(AppAction::ToggleDrawer)
            .expect("test operation should succeed");
        assert!(app.drawer_visible);
        assert_eq!(app.drawer_tab, DrawerTab::Board);
        app.update(AppAction::ToggleDrawer)
            .expect("test operation should succeed");
        assert!(!app.drawer_visible);
        assert_eq!(app.drawer_tab, DrawerTab::Board);
    }

    #[test]
    fn slash_commands_jump_directly_to_each_operational_tab() {
        let mut app = app();
        for (command, expected) in [
            ("/activity", DrawerTab::Activity),
            ("/work", DrawerTab::Work),
            ("/board", DrawerTab::Board),
            ("/problems", DrawerTab::Problems),
            ("/route", DrawerTab::Route),
            ("/usage", DrawerTab::Usage),
            ("/settings", DrawerTab::Settings),
        ] {
            app.set_drawer_visible(false);
            app.handle_slash(command.trim_start_matches('/'));
            assert!(app.drawer_visible, "{command} must open the drawer");
            assert_eq!(app.drawer_tab, expected, "{command} must select its tab");
        }
    }

    #[test]
    fn help_uses_the_anchored_drawer_on_wide_terminals_and_modal_on_narrow_ones() {
        let mut app = app();
        app.sync_drawer_visibility(140);
        app.handle_slash("help");
        assert_eq!(app.drawer_tab, DrawerTab::Help);
        assert!(app.drawer_visible);
        assert!(app.overlay.is_none());
        app.escape();
        assert!(!app.drawer_visible, "Esc closes the wide help drawer");

        app.sync_drawer_visibility(80);
        app.handle_slash("help");
        assert_eq!(app.overlay, Some(Overlay::Help));
    }

    #[test]
    fn drawer_defaults_hidden_until_explicitly_opened() {
        let mut app = app();
        app.sync_drawer_visibility(140);
        assert!(!app.drawer_visible, "wide terminal must begin transcript-first");
        app.sync_drawer_visibility(80);
        assert!(!app.drawer_visible, "narrow terminal defaults to hidden");
    }

    #[test]
    fn drawer_opens_only_when_explicitly_requested_on_a_readable_primary_rail() {
        let mut app = app();
        app.sync_drawer_visibility(WIDE_DRAWER_MIN_WIDTH - 1);
        assert!(
            !app.drawer_visible,
            "the fixed Operations rail must not squeeze the primary surface at the old breakpoint"
        );
        app.sync_drawer_visibility(WIDE_DRAWER_MIN_WIDTH);
        assert!(
            !app.drawer_visible,
            "wide terminals remain transcript-first by default"
        );
        app.set_drawer_visible(true);
        assert!(
            app.drawer_visible,
            "the drawer opens once its primary rail is readable and the user requests it"
        );
    }

    #[test]
    fn drawer_override_is_remembered_per_width_class_independently() {
        let mut app = app();
        app.sync_drawer_visibility(140);
        app.set_drawer_visible(false); // explicit close on wide
        app.sync_drawer_visibility(80); // narrow: still its own default (hidden)
        assert!(!app.drawer_visible);
        app.set_drawer_visible(true); // explicit open on narrow
        app.sync_drawer_visibility(140); // back to wide: wide's override, not narrow's
        assert!(
            !app.drawer_visible,
            "wide's explicit close must survive a narrow-width excursion"
        );
        app.sync_drawer_visibility(80);
        assert!(
            app.drawer_visible,
            "narrow's explicit open must also be remembered"
        );
    }

    #[test]
    fn drawer_interactive_requires_an_explicit_open() {
        let mut app = app();
        app.sync_drawer_visibility(140);
        assert!(!app.drawer_visible, "wide terminals begin with no ambient drawer");
        assert!(
            !app.drawer_interactive(),
            "a hidden drawer cannot steal Up/Down/Enter from the composer"
        );
        app.set_drawer_visible(true);
        assert!(app.drawer_interactive(), "an explicit open must be interactive");
    }

    #[test]
    fn work_drawer_selection_includes_agent_todos_after_plan_rows() {
        let mut app = app();
        app.plan.push(PlanTask {
            id: "plan-1".into(),
            objective: "first visible plan row".into(),
            paths: Vec::new(),
            dependencies: Vec::new(),
            state: minha_core::protocol::PlanTaskState::Pending,
            agent_id: None,
        });
        app.todos.insert(
            EventAgentId::new(),
            vec![TodoItem {
                id: "todo-1".into(),
                objective: "second visible todo row".into(),
                state: minha_core::protocol::TodoState::Pending,
                order: 0,
                blocker: None,
                evidence: Vec::new(),
                revision: 1,
            }],
        );
        app.sync_drawer_visibility(80);
        app.set_drawer_visible(true);
        app.drawer_tab = DrawerTab::Work;

        app.update(AppAction::SelectDown)
            .expect("selection moves into the TODO row");
        assert_eq!(app.selected_task, 1);
        app.update(AppAction::ActivateIndex(99))
            .expect("mouse selection clamps to the final visible row");
        assert_eq!(app.selected_task, 1);
    }

    #[test]
    fn local_inspector_commands_open_without_model_calls() {
        let mut app = app();
        for (command, overlay) in [
            ("/context", Overlay::Context),
            ("/books", Overlay::Books),
            ("/status", Overlay::Status),
        ] {
            app.input = command.into();
            app.update(AppAction::Submit)
                .expect("test operation should succeed");
            assert_eq!(app.overlay, Some(overlay));
            app.overlay = None;
        }
        assert!(!app.library.is_empty());
    }

    #[test]
    fn retry_fresh_requests_a_new_run() {
        let mut app = app();
        let run_id = RunId::new();
        app.active_run = Some(run_id);
        app.input = "/retry --fresh".into();
        app.update(AppAction::Submit)
            .expect("test operation should succeed");
        assert_eq!(
            app.take_submission(),
            Some(Submission::Retry { run_id, fresh: true })
        );
    }

    #[test]
    fn additional_agents_do_not_steal_transcript_focus() {
        let mut app = app();
        let run = RunId::new();
        for sequence in 0..2 {
            app.apply_event(&EventEnvelope::new(
                run,
                sequence,
                RuntimeEvent::AgentStarted {
                    agent_id: EventAgentId::new(),
                    role: format!("Spark worker {sequence}"),
                    model: "gpt-5.3-codex-spark".into(),
                    parent: None,
                },
            ));
        }
        assert!(!app.drawer_visible);
        app.apply_event(&EventEnvelope::new(
            run,
            2,
            RuntimeEvent::AgentStarted {
                agent_id: EventAgentId::new(),
                role: "Spark worker 2".into(),
                model: "gpt-5.3-codex-spark".into(),
                parent: None,
            },
        ));
        assert!(!app.drawer_visible);
    }

    #[test]
    fn direct_agent_target_and_office_delta_stay_transcript_first() {
        let mut app = app();
        let run = RunId::new();
        let agent_id = EventAgentId::new();
        app.active_run = Some(run);
        app.apply_event(&EventEnvelope::new(
            run,
            1,
            RuntimeEvent::AgentStarted {
                agent_id,
                role: "Spark worker".into(),
                model: "gpt-5.3-codex-spark".into(),
                parent: None,
            },
        ));
        app.input = format!("/to {agent_id}");
        app.input_cursor = app.input.len();
        app.update(AppAction::Submit).expect("select recipient");
        assert_eq!(app.message_target, Some(format!("agent:{agent_id}")));

        app.input = "check the parser boundary".into();
        app.input_cursor = app.input.len();
        app.update(AppAction::Submit).expect("send direct request");
        assert!(matches!(
            app.take_submission(),
            Some(Submission::AgentMessage { run_id, recipient, text })
                if run_id == run
                    && recipient == format!("agent:{agent_id}")
                    && text == "check the parser boundary"
        ));

        app.apply_event(&EventEnvelope::new(
            run,
            2,
            RuntimeEvent::OfficeMessageChanged {
                message_id: "m1".into(),
                room_id: "run".into(),
                sender: "user".into(),
                recipient: format!("agent:{agent_id}"),
                kind: "request".into(),
                summary: "check the parser boundary".into(),
                deduplicated: false,
            },
        ));
        assert!(!app.drawer_visible);
        assert!(app.items.iter().any(|item| matches!(
            item,
            TranscriptItem::Coordination { kind, summary, .. }
                if kind == "request" && summary == "check the parser boundary"
        )));
    }

    #[test]
    fn oversized_direct_message_stays_in_the_composer_for_a_compact_office_record() {
        let mut app = app();
        let run = RunId::new();
        app.active_run = Some(run);
        app.message_target = Some("agent:worker-1".into());
        app.input = "x".repeat(MAX_OFFICE_SUMMARY_BYTES + 1);
        app.input_cursor = app.input.len();

        app.update(AppAction::Submit).expect("reject oversize message");

        assert_eq!(app.message_target.as_deref(), Some("agent:worker-1"));
        assert_eq!(app.input.len(), MAX_OFFICE_SUMMARY_BYTES + 1);
        assert_eq!(app.take_submission(), None);
        assert!(matches!(
            app.items.last(),
            Some(TranscriptItem::System {
                tone: SystemTone::Warning,
                ..
            })
        ));
    }

    #[test]
    fn context_meter_uses_current_context_not_billed_session_tokens() {
        let mut app = app();
        app.input_tokens = 100_000;
        app.output_tokens = 10_000;
        app.current_context_tokens = 32_000;
        assert_eq!(app.context_percent(), 25.0);
    }

    #[test]
    fn deepseek_usage_tracks_cost_and_cache_savings() {
        let mut app = app();
        let run = RunId::new();
        app.apply_event(&EventEnvelope::new(
            run,
            1,
            RuntimeEvent::Usage {
                agent_id: None,
                model: "deepseek/deepseek-v4-flash".into(),
                input_tokens: 1_000_000,
                output_tokens: 1_000_000,
                cached_input_tokens: 500_000,
                cache_write_tokens: 500_000,
                reasoning_output_tokens: 0,
            },
        ));
        assert!((app.deepseek_estimated_usd - 0.3514).abs() < f64::EPSILON);
        assert!((app.deepseek_cache_savings_usd - 0.0686).abs() < f64::EPSILON);
    }

    #[test]
    fn clarification_choice_submits_a_typed_batch() {
        let mut app = app();
        let run = RunId::new();
        app.active_run = Some(run);
        app.apply_event(&clarification_event(run, false));

        assert_eq!(app.overlay, None);
        assert!(app.has_active_clarification());
        app.update(AppAction::SelectDown).expect("select option");
        app.update(AppAction::Activate).expect("answer question");

        assert!(matches!(
            app.take_submission(),
            Some(Submission::Clarify { run_id, answers })
                if run_id == run && answers.len() == 1 && answers[0].0 == "goal-1"
        ));
    }

    #[test]
    fn typing_during_a_question_submits_a_direct_custom_answer() {
        let mut app = app();
        let run = RunId::new();
        app.active_run = Some(run);
        app.apply_event(&clarification_event(run, false));
        for character in "only after resize".chars() {
            app.update(AppAction::Input(character)).expect("type answer");
        }
        app.update(AppAction::Submit).expect("submit custom answer");
        assert!(matches!(
            app.take_submission(),
            Some(Submission::Clarify { run_id, answers })
                if run_id == run
                    && answers == vec![("goal-1".into(), "only after resize".into())]
        ));
    }

    #[test]
    fn internal_agent_control_payloads_never_enter_the_transcript() {
        let mut app = app();
        let run = RunId::new();
        let agent = EventAgentId::new();
        let item = ItemId::new();
        app.apply_event(&EventEnvelope::new(
            run,
            1,
            RuntimeEvent::AgentStarted {
                agent_id: agent,
                role: "Mina, issue clarifier".into(),
                model: "gpt-5.6-mina".into(),
                parent: None,
            },
        ));
        app.apply_event(&EventEnvelope::new(
            run,
            2,
            RuntimeEvent::TextDelta {
                agent_id: agent,
                item_id: item,
                delta: "<minha-clarification>{\"questions\":[]}".into(),
            },
        ));
        app.apply_event(&EventEnvelope::new(
            run,
            3,
            RuntimeEvent::AssistantMessage {
                agent_id: agent,
                item_id: item,
                role: "Mina, issue clarifier".into(),
                model: "gpt-5.6-mina".into(),
                text: "<minha-clarification>{\"questions\":[]}</minha-clarification>".into(),
            },
        ));
        assert!(
            !app.items
                .iter()
                .any(|item| matches!(item, TranscriptItem::Assistant { .. }))
        );
        assert_eq!(strip_control_tags("hello <minha-plan>{}</minha-plan>"), "hello");
    }

    #[test]
    fn clarification_review_requires_explicit_confirmation() {
        let mut app = app();
        let run = RunId::new();
        app.active_run = Some(run);
        app.apply_event(&clarification_event(run, true));

        app.update(AppAction::Activate).expect("confirm brief");

        assert_eq!(
            app.take_submission(),
            Some(Submission::Clarify {
                run_id: run,
                answers: vec![("$action".into(), "confirm".into())],
            })
        );
    }

    #[test]
    fn session_end_errors_and_interrupts_clear_outstanding_input() {
        let run = RunId::new();
        for (event, expected_state) in [
            (
                RuntimeEvent::SessionFinished {
                    state: ExitState::Failed,
                    model: None,
                    text: "finished".into(),
                    agents_used: 2,
                },
                ExitState::Failed,
            ),
            (
                RuntimeEvent::Error {
                    state: ExitState::Failed,
                    message: "boom".into(),
                },
                ExitState::Failed,
            ),
            (
                RuntimeEvent::TurnInterrupted {
                    reason: "user interrupt".into(),
                },
                ExitState::Cancelled,
            ),
        ] {
            let mut app = app();
            app.pending_request = Some(PendingRequest {
                id: RequestId::new(),
                question: "answer me".into(),
                options: vec![],
                approval: false,
                reason: None,
                command: None,
            });
            app.clarification = Some(analyze("why", "auto"));
            app.clarification_answers = vec![("$action".into(), "use_best_judgment".into())];
            app.apply_event(&EventEnvelope::new(run, 1, event));
            assert!(
                app.pending_request.is_none(),
                "a dead run must not stay in answer-required state"
            );
            assert!(app.clarification.is_none());
            assert!(app.clarification_answers.is_empty());
            assert_eq!(app.state, expected_state);
        }
    }

    #[test]
    fn question_supersedes_clarification_and_keeps_its_request_id() {
        let mut app = app();
        let run = RunId::new();
        app.apply_event(&clarification_event(run, false));
        assert!(app.clarification.is_some());

        let request_id = RequestId::new();
        app.apply_event(&EventEnvelope::new(
            run,
            2,
            RuntimeEvent::Question {
                request_id,
                agent_id: EventAgentId::new(),
                question: "which plan?".into(),
                options: vec!["a".into(), "b".into()],
                blocking: true,
            },
        ));
        assert!(
            app.clarification.is_none(),
            "a live question must supersede the clarification session"
        );
        let pending = app.pending_request.as_ref().expect("question must be recorded");
        assert_eq!(pending.id, request_id);
        assert_eq!(pending.question, "which plan?");
        assert!(!pending.approval);

        let approval_id = RequestId::new();
        app.apply_event(&EventEnvelope::new(
            run,
            3,
            RuntimeEvent::Approval {
                request_id: approval_id,
                agent_id: EventAgentId::new(),
                reason: "destructive rm".into(),
                command: Some(vec!["rm".into(), "-rf".into()]),
            },
        ));
        let pending = app.pending_request.as_ref().expect("approval must be recorded");
        assert_eq!(pending.id, approval_id);
        assert!(pending.approval);
        assert_eq!(pending.command, Some(vec!["rm".into(), "-rf".into()]));
        assert_eq!(app.state, ExitState::ApprovalRequired);
        assert_eq!(
            app.selected_clarification_option, 1,
            "an approval card must default to the safe (decline) option, never armed to \
             approve on a reflexive Enter"
        );
    }

    #[test]
    fn session_finished_pausing_for_approval_does_not_clear_the_card_it_arrived_with() {
        // finish_agent_outcome_with_state emits SessionFinished for every
        // settled AgentResult, including a pause - so an ApprovalRequired (or
        // NeedsInput) SessionFinished event is expected to arrive right
        // alongside the Approval/Question event it belongs to, not after the
        // interaction is over. Regression for a bug that made the exec- and
        // integration-approval cards flash and disappear immediately.
        let mut app = app();
        let run = RunId::new();
        app.active_run = Some(run);
        app.apply_event(&EventEnvelope::new(
            run,
            1,
            RuntimeEvent::Approval {
                request_id: RequestId::new(),
                agent_id: EventAgentId::new(),
                reason: "integration approval".into(),
                command: None,
            },
        ));
        assert!(app.pending_request.is_some(), "approval must be recorded");
        app.apply_event(&EventEnvelope::new(
            run,
            2,
            RuntimeEvent::SessionFinished {
                state: ExitState::ApprovalRequired,
                model: None,
                text: "waiting".into(),
                agents_used: 1,
            },
        ));
        assert!(
            app.pending_request.is_some(),
            "a pause-for-approval SessionFinished must not clear the very request it is reporting"
        );
    }

    #[test]
    fn budget_stop_reason_survives_the_generic_usage_paused_projection() {
        let mut app = app();
        let run = RunId::new();
        app.apply_event(&EventEnvelope::new(
            run,
            1,
            RuntimeEvent::RunStopped {
                reason: TerminationReason::BudgetTarget,
                detail: "session target reached before another model turn".into(),
            },
        ));
        assert_eq!(app.state, ExitState::UsagePaused);
        assert_eq!(app.termination_reason, Some(TerminationReason::BudgetTarget));
        assert_eq!(app.status, "session budget paused — 5% recovery reserve held");

        app.apply_event(&EventEnvelope::new(
            run,
            2,
            RuntimeEvent::SessionState {
                state: ExitState::UsagePaused,
            },
        ));
        assert_eq!(app.status, "session budget paused — 5% recovery reserve held");

        app.apply_event(&EventEnvelope::new(
            run,
            3,
            RuntimeEvent::SessionFinished {
                state: ExitState::UsagePaused,
                model: None,
                text: "paused".into(),
                agents_used: 1,
            },
        ));
        assert_eq!(app.termination_reason, Some(TerminationReason::BudgetTarget));
        assert_eq!(app.status, "session budget paused — 5% recovery reserve held");
    }

    #[test]
    fn session_finished_that_is_actually_terminal_still_clears_pending_input() {
        let mut app = app();
        let run = RunId::new();
        app.active_run = Some(run);
        app.apply_event(&EventEnvelope::new(
            run,
            1,
            RuntimeEvent::Approval {
                request_id: RequestId::new(),
                agent_id: EventAgentId::new(),
                reason: "destructive rm".into(),
                command: Some(vec!["rm".into(), "-rf".into()]),
            },
        ));
        assert!(app.pending_request.is_some());
        app.apply_event(&EventEnvelope::new(
            run,
            2,
            RuntimeEvent::SessionFinished {
                state: ExitState::Cancelled,
                model: None,
                text: "interrupted".into(),
                agents_used: 1,
            },
        ));
        assert!(
            app.pending_request.is_none(),
            "a truly final state must still clear a stale pending request"
        );
    }

    #[test]
    fn pending_request_enter_answers_the_highlighted_option() {
        let mut app = app();
        let run = RunId::new();
        app.active_run = Some(run);
        let request_id = RequestId::new();
        app.apply_event(&EventEnvelope::new(
            run,
            1,
            RuntimeEvent::Approval {
                request_id,
                agent_id: EventAgentId::new(),
                reason: "destructive rm".into(),
                command: Some(vec!["rm".into(), "-rf".into()]),
            },
        ));
        // Defaults to "no"; Enter with no typed text answers the highlighted
        // option, matching the clarification card's grammar.
        app.update(AppAction::Activate)
            .expect("answer with default selection");
        assert_eq!(
            app.take_submission(),
            Some(Submission::Answer {
                run_id: run,
                text: "no".into(),
            })
        );

        let request_id = RequestId::new();
        app.apply_event(&EventEnvelope::new(
            run,
            2,
            RuntimeEvent::Approval {
                request_id,
                agent_id: EventAgentId::new(),
                reason: "destructive rm".into(),
                command: Some(vec!["rm".into(), "-rf".into()]),
            },
        ));
        app.update(AppAction::SelectUp)
            .expect("move selection to approve");
        app.update(AppAction::Activate)
            .expect("answer with moved selection");
        assert_eq!(
            app.take_submission(),
            Some(Submission::Answer {
                run_id: run,
                text: "yes".into(),
            })
        );
    }

    #[test]
    fn pending_request_typed_text_overrides_the_selected_option() {
        let mut app = app();
        let run = RunId::new();
        app.active_run = Some(run);
        app.apply_event(&EventEnvelope::new(
            run,
            1,
            RuntimeEvent::Question {
                request_id: RequestId::new(),
                agent_id: EventAgentId::new(),
                question: "which plan?".into(),
                options: vec!["a".into(), "b".into()],
                blocking: true,
            },
        ));
        app.input = "a custom answer".into();
        app.input_cursor = app.input.len();
        app.update(AppAction::Submit).expect("submit custom answer");
        assert_eq!(
            app.take_submission(),
            Some(Submission::Answer {
                run_id: run,
                text: "a custom answer".into(),
            })
        );
    }

    #[test]
    fn single_candidate_completion_replaces_only_the_typed_prefix() {
        let mut app = app();
        app.input = "/status".into();
        app.input_cursor = 4;
        app.update(AppAction::Complete).expect("complete the prefix");
        assert_eq!(
            app.input, "/status tus",
            "text after the cursor must survive slash completion"
        );
        assert_eq!(app.input_cursor, 8);

        app.input = "/statu".into();
        app.input_cursor = 6;
        app.update(AppAction::Complete).expect("complete the prefix");
        assert_eq!(app.input, "/status ");
    }
}
