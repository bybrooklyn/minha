use crate::editor::EditorLayout;
use anyhow::Result;
use minha_core::books::{ManifestEntry, SignedRegistryManifest};
use minha_core::deepseek::{estimate_cost_usd, pricing_for_model};
use minha_core::protocol::{
    AgentState, BoardEntryView, CatalogModel, ClarificationStatus, EventAgentId, EventEnvelope, ExitState,
    IncidentView, IssueClarificationView, ItemId, PlanTask, RequestId, RunId, RunPhase, RuntimeEvent,
    TodoItem,
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
    pub(crate) lines: Vec<Line<'static>>,
    pub(crate) builds: u64,
    pub(crate) last_viewport_lines: usize,
    pub(crate) max_scroll: u16,
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

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PendingRequest {
    pub(crate) id: RequestId,
    pub(crate) question: String,
    pub(crate) options: Vec<String>,
    pub(crate) approval: bool,
    pub(crate) reason: Option<String>,
    pub(crate) command: Option<Vec<String>>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Overlay {
    Palette,
    Help,
    Sessions,
    Status,
    Context,
    Books,
    Doctor,
    Request,
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

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum DrawerTab {
    #[default]
    Activity,
    Work,
    Board,
    Problems,
}

impl DrawerTab {
    pub(crate) fn next(self) -> Option<Self> {
        match self {
            Self::Activity => Some(Self::Work),
            Self::Work => Some(Self::Board),
            Self::Board => Some(Self::Problems),
            Self::Problems => None,
        }
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
    PageUp,
    PageDown,
    SelectUp,
    SelectDown,
    Activate,
    ActivateClarificationOption(usize),
    ActivateIndex(usize),
    None,
}

pub struct App {
    pub(crate) root: PathBuf,
    pub(crate) mode: WorkMode,
    pub(crate) input: String,
    pub(crate) input_cursor: usize,
    pub(crate) completion_items: Vec<(String, String)>,
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
    pub(crate) selected_palette: usize,
    pub(crate) focused_agent: Option<EventAgentId>,
    pub(crate) drawer_visible: bool,
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
    pub(crate) model: String,
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) cached_input_tokens: u64,
    pub(crate) cache_write_tokens: u64,
    pub(crate) reasoning_output_tokens: u64,
    pub(crate) deepseek_estimated_usd: f64,
    pub(crate) deepseek_cache_savings_usd: f64,
    pub(crate) deepseek_balance: Option<String>,
    pub(crate) deepseek_reserve_percent: Option<f64>,
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
    pub(crate) incidents: Vec<IncidentView>,
    pub(crate) current_context_tokens: u64,
    pub(crate) compaction_count: u64,
    pub(crate) compact_at_tokens: u64,
    pub(crate) context_limit: u64,
    pub(crate) queued_steering: usize,
    pub(crate) message_target: Option<String>,
    pub(crate) scroll: u16,
    pub(crate) auto_follow: bool,
    pub(crate) status: String,
    pub(crate) phase: RunPhase,
    pub(crate) phase_started_at: Instant,
    pub(crate) theme: String,
    pub(crate) surface_renderer: String,
    pub(crate) active_surface_renderer: String,
    pub(crate) reduced_motion: bool,
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
            selected_palette: 0,
            focused_agent: None,
            drawer_visible: false,
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
            model: "gpt-5.6-luna".into(),
            input_tokens: 0,
            output_tokens: 0,
            cached_input_tokens: 0,
            cache_write_tokens: 0,
            reasoning_output_tokens: 0,
            deepseek_estimated_usd: 0.0,
            deepseek_cache_savings_usd: 0.0,
            deepseek_balance: None,
            deepseek_reserve_percent: None,
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
            incidents: Vec::new(),
            current_context_tokens: 0,
            compaction_count: 0,
            compact_at_tokens: (context_limit as f64 * 0.72) as u64,
            context_limit: context_limit.max(1),
            queued_steering: 0,
            message_target: None,
            scroll: 0,
            auto_follow: true,
            status: "ready".into(),
            phase: RunPhase::Complete,
            phase_started_at: Instant::now(),
            theme: "dark".into(),
            surface_renderer: "auto".into(),
            active_surface_renderer: "quadrant".into(),
            reduced_motion: false,
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

    pub fn update(&mut self, action: AppAction) -> Result<bool> {
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
                    self.completion_items.clear();
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
            AppAction::CursorHome => {
                self.input_cursor = self.input[..self.input_cursor]
                    .rfind('\n')
                    .map_or(0, |index| index + 1);
            }
            AppAction::CursorEnd => {
                self.input_cursor += self.input[self.input_cursor..]
                    .find('\n')
                    .unwrap_or(self.input.len() - self.input_cursor);
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
            AppAction::Undo => self.undo(),
            AppAction::Redo => self.redo(),
            AppAction::Complete => self.complete_input(),
            AppAction::Submit => self.submit_input(),
            AppAction::HistoryPrevious => self.history_previous(),
            AppAction::CommandPalette => {
                self.selected_palette = 0;
                self.overlay = toggle_overlay(&self.overlay, Overlay::Palette);
            }
            AppAction::Escape => self.escape(),
            AppAction::Interrupt => {
                if let Some(run_id) = self.running.then_some(self.active_run).flatten() {
                    self.submission = Some(Submission::Interrupt { run_id });
                }
            }
            AppAction::ToggleDrawer => {
                if !self.drawer_visible {
                    self.drawer_visible = true;
                    self.drawer_tab = DrawerTab::Activity;
                } else if let Some(next) = self.drawer_tab.next() {
                    self.drawer_tab = next;
                    if next == DrawerTab::Board {
                        self.submission = Some(Submission::ShowBoard);
                    }
                } else {
                    self.drawer_visible = false;
                }
            }
            AppAction::ToggleDetails => self.toggle_selected_item(),
            AppAction::ToggleTasks => self.tasks_visible = !self.tasks_visible,
            AppAction::Help => self.overlay = toggle_overlay(&self.overlay, Overlay::Help),
            AppAction::ScrollUp => self.scroll_up(3),
            AppAction::ScrollDown => self.scroll_down(3),
            AppAction::PageUp => self.scroll_up(12),
            AppAction::PageDown => self.scroll_down(12),
            AppAction::SelectUp => self.select(-1),
            AppAction::SelectDown => self.select(1),
            AppAction::Activate => self.activate_selection(),
            AppAction::ActivateClarificationOption(index) => {
                self.selected_clarification_option =
                    index.min(self.clarification_option_count().saturating_sub(1));
                self.activate_selection();
            }
            AppAction::ActivateIndex(index) => {
                match self.drawer_tab {
                    DrawerTab::Activity => {
                        self.selected_agent = index.min(self.agents.len().saturating_sub(1))
                    }
                    DrawerTab::Work => self.selected_task = index.min(self.plan.len().saturating_sub(1)),
                    DrawerTab::Board => self.selected_board = index.min(self.board.len().saturating_sub(1)),
                    DrawerTab::Problems => {
                        self.selected_problem = index.min(self.incidents.len().saturating_sub(1));
                    }
                }
                self.activate_selection();
            }
            AppAction::None | AppAction::Input(_) => {}
        }
        Ok(false)
    }

    fn submit_input(&mut self) {
        let text = self.input.trim().to_owned();
        if text.is_empty() && !self.has_active_clarification() {
            return;
        }
        self.input.clear();
        self.input_cursor = 0;
        self.history_cursor = None;
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
        if let (Some(recipient), Some(run_id)) = (self.message_target.take(), self.active_run) {
            self.submission = Some(Submission::AgentMessage {
                run_id,
                recipient,
                text,
            });
            return;
        }
        if self.pending_request.is_some()
            && let Some(run_id) = self.active_run
        {
            self.submission = Some(Submission::Answer { run_id, text });
            self.overlay = None;
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
        self.scroll = u16::MAX;
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
        self.input.clear();
        self.input_cursor = 0;
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

    fn handle_slash(&mut self, command: &str) {
        let (name, args) = command
            .split_once(char::is_whitespace)
            .map_or((command, ""), |(name, args)| (name, args.trim()));
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
            "rename" if !args.is_empty() => {
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
            "agents" | "activity" => {
                self.drawer_tab = DrawerTab::Activity;
                self.drawer_visible = true;
            }
            "work" => {
                self.drawer_tab = DrawerTab::Work;
                self.drawer_visible = true;
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
                self.drawer_visible = true;
                self.submission = Some(Submission::ShowBoard);
            }
            "problems" => {
                self.drawer_tab = DrawerTab::Problems;
                self.drawer_visible = true;
            }
            "note" if !args.is_empty() => {
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
                format!("lead {} · workers gpt-5.3-codex-spark", self.model),
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
            "usage" => self.push_system(
                SystemTone::Info,
                format!(
                    "{} input + {} output tokens · {:.1}% context estimate",
                    self.input_tokens,
                    self.output_tokens,
                    self.context_percent()
                ),
            ),
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
            "gh" | "github" => {
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
            "help" => self.overlay = Some(Overlay::Help),
            "quit" | "exit" => self.submission = Some(Submission::Quit),
            "auto" => self.mode = WorkMode::Auto,
            _ => self.push_system(SystemTone::Warning, format!("unknown command /{name}; use /help")),
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

    fn escape(&mut self) {
        if self.has_active_clarification() {
            if !self.input.is_empty() {
                self.input.clear();
                self.input_cursor = 0;
            }
            self.clarification_note_open = false;
            return;
        }
        if self.overlay.take().is_some() {
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
        self.input.clear();
    }

    fn select(&mut self, delta: isize) {
        if self.has_active_clarification() && !self.clarification_note_open {
            let count = self.clarification_option_count();
            self.selected_clarification_option = move_index(self.selected_clarification_option, count, delta);
        } else if matches!(self.overlay, Some(Overlay::Palette)) {
            self.selected_palette = move_index(self.selected_palette, COMMAND_PALETTE.len(), delta);
        } else if matches!(self.overlay, Some(Overlay::Sessions)) {
            self.selected_session = move_index(self.selected_session, self.sessions.len(), delta);
        } else if matches!(self.overlay, Some(Overlay::Books)) {
            self.selected_book = move_index(self.selected_book, self.library.len(), delta);
        } else if self.drawer_visible || self.focused_agent.is_some() {
            match self.drawer_tab {
                DrawerTab::Activity => {
                    self.selected_agent = move_index(self.selected_agent, self.agents.len(), delta)
                }
                DrawerTab::Work => {
                    self.selected_task = move_index(self.selected_task, self.plan.len(), delta)
                }
                DrawerTab::Board => {
                    self.selected_board = move_index(self.selected_board, self.board.len(), delta)
                }
                DrawerTab::Problems => {
                    self.selected_problem = move_index(self.selected_problem, self.incidents.len(), delta)
                }
            }
        } else {
            self.scroll = if delta < 0 {
                self.scroll.saturating_sub(1)
            } else {
                self.scroll.saturating_add(1)
            };
        }
    }

    fn activate_selection(&mut self) {
        if self.has_active_clarification() {
            self.submit_clarification(String::new());
        } else if matches!(self.overlay, Some(Overlay::Palette)) {
            if let Some((command, _)) = COMMAND_PALETTE.get(self.selected_palette) {
                let command = command.trim_start_matches('/').to_owned();
                self.overlay = None;
                self.handle_slash(&command);
            }
        } else if matches!(self.overlay, Some(Overlay::Sessions)) {
            if let Some(run) = self.sessions.get(self.selected_session) {
                self.submission = Some(Submission::Resume { run_id: run.id });
                self.overlay = None;
            }
        } else if self.drawer_visible
            && self.drawer_tab == DrawerTab::Activity
            && let Some(agent) = self.agents.get(self.selected_agent)
        {
            self.focused_agent = Some(agent.id);
            self.drawer_visible = false;
            self.scroll = 0;
        }
    }

    fn toggle_selected_item(&mut self) {
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
        let current = if self.scroll == u16::MAX {
            max_scroll
        } else {
            self.scroll
        };
        let next = current.saturating_add(amount).min(max_scroll);
        self.auto_follow = next >= max_scroll;
        self.scroll = if self.auto_follow { u16::MAX } else { next };
    }

    fn scroll_up(&mut self, amount: u16) {
        let max_scroll = self.transcript_layout.borrow().max_scroll;
        let current = if self.scroll == u16::MAX {
            max_scroll
        } else {
            self.scroll.min(max_scroll)
        };
        self.scroll = current.saturating_sub(amount);
        self.auto_follow = false;
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
            self.completion_items.clear();
        }
    }

    fn redo(&mut self) {
        if let Some(snapshot) = self.redo_stack.pop() {
            self.undo_stack.push((self.input.clone(), self.input_cursor));
            (self.input, self.input_cursor) = snapshot;
            self.preferred_column = None;
            self.completion_items.clear();
        }
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
            self.completion_items.clear();
        }
    }

    fn move_cursor_vertical(&mut self, direction: isize) {
        let layout = EditorLayout::new(&self.input, self.input_cursor, self.composer_inner_width.get());
        let column = self.preferred_column.unwrap_or(layout.cursor_column);
        self.preferred_column = Some(column);
        if direction < 0 {
            if layout.cursor_row == 0 {
                if self.input_cursor == 0 {
                    self.history_previous();
                }
                return;
            }
            self.input_cursor = layout.byte_at_column(&self.input, layout.cursor_row - 1, column);
        } else if layout.cursor_row + 1 >= layout.lines.len() {
            if self.input_cursor == self.input.len() {
                self.history_next();
            }
        } else {
            self.input_cursor = layout.byte_at_column(&self.input, layout.cursor_row + 1, column);
        }
    }

    fn complete_input(&mut self) {
        self.completion_items.clear();
        if let Some(prefix) = self.input.strip_prefix('/')
            && !prefix.chars().any(char::is_whitespace)
        {
            let candidates = slash_commands()
                .iter()
                .filter(|(name, _)| name.starts_with(prefix))
                .map(|(name, description)| ((*name).to_owned(), (*description).to_owned()))
                .collect::<Vec<_>>();
            self.completion_items = candidates.clone();
            if let [candidate] = candidates.as_slice() {
                self.checkpoint_editor();
                self.input = format!("/{} ", candidate.0);
                self.input_cursor = self.input.len();
                self.completion_items.clear();
            }
            return;
        }
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
                    (completed, description.to_owned())
                })
            })
            .take(8)
            .collect::<Vec<_>>();
        paths.sort_by(|left, right| left.0.cmp(&right.0));
        self.completion_items = paths.clone();
        if let [candidate] = paths.as_slice() {
            self.checkpoint_editor();
            self.input
                .replace_range(token_start..self.input_cursor, &candidate.0);
            self.input_cursor = token_start + candidate.0.len();
            self.completion_items.clear();
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
                self.status = "working".into();
                self.pending_request = None;
                self.overlay = None;
            }
            RuntimeEvent::SessionState { state } => {
                self.state = *state;
                self.status = state_label(*state).into();
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
                if role.contains("Luna") {
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
                self.pending_request = Some(PendingRequest {
                    id: *request_id,
                    question: question.clone(),
                    options: options.clone(),
                    approval: false,
                    reason: None,
                    command: None,
                });
                self.overlay = Some(Overlay::Request);
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
                self.pending_request = Some(PendingRequest {
                    id: *request_id,
                    question: "Approve this risky action?".into(),
                    options: vec!["yes".into(), "no".into()],
                    approval: true,
                    reason: Some(reason.clone()),
                    command: command.clone(),
                });
                self.overlay = Some(Overlay::Request);
                self.running = false;
                self.state = ExitState::ApprovalRequired;
                self.status = "approval required".into();
            }
            RuntimeEvent::RequestResolved { .. } => {
                self.pending_request = None;
                self.overlay = None;
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
                currency,
                total,
                reserve_percent,
                ..
            } => {
                self.deepseek_balance = Some(format!("{currency} {total}"));
                self.deepseek_reserve_percent = *reserve_percent;
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
                self.push_system(SystemTone::Warning, format!("single Luna lane: {reason}"))
            }
            RuntimeEvent::TurnInterrupted { reason } => {
                self.running = false;
                self.state = ExitState::Cancelled;
                self.status = "interrupted".into();
                self.push_system(SystemTone::Warning, reason.clone());
            }
            RuntimeEvent::RunStopped { reason, detail } => {
                self.status = format!("{reason:?}").to_ascii_lowercase();
                if !detail.is_empty() && !matches!(reason, minha_core::protocol::TerminationReason::Completed)
                {
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
                self.status = state_label(*state).into();
                self.set_phase(RunPhase::Complete);
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
                self.push_system(SystemTone::Warning, message.clone());
            }
            RuntimeEvent::Error { state, message } => {
                self.running = false;
                self.state = *state;
                self.status = state_label(*state).into();
                self.push_system(SystemTone::Error, message.clone());
            }
            RuntimeEvent::SessionForked { .. }
            | RuntimeEvent::SessionRenamed { .. }
            | RuntimeEvent::SessionArchived
            | RuntimeEvent::RoutingDecision { .. }
            | RuntimeEvent::ActivityStarted { .. }
            | RuntimeEvent::ActivityUpdated { .. }
            | RuntimeEvent::ActivityFinished { .. }
            | RuntimeEvent::MemoryChanged { .. }
            | RuntimeEvent::MemoryRetrieved { .. }
            | RuntimeEvent::ProviderState { .. }
            | RuntimeEvent::AccountUsage { .. }
            | RuntimeEvent::Legacy { .. } => {}
        }
        if self.auto_follow {
            self.scroll = u16::MAX;
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

    pub(crate) fn push_status_card(&mut self, lines: Vec<String>) {
        self.items.push(TranscriptItem::Status { lines });
        self.invalidate_transcript_layout();
        if self.auto_follow {
            self.scroll = u16::MAX;
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
        if self.auto_follow {
            self.scroll = u16::MAX;
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
        if self.auto_follow {
            self.scroll = u16::MAX;
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
                TranscriptItem::Assistant { role, text, .. } => {
                    out.push_str(&format!("## {role}\n\n{text}\n\n"));
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
        self.status = "ready".into();
        self.input_tokens = 0;
        self.output_tokens = 0;
        self.cached_input_tokens = 0;
        self.cache_write_tokens = 0;
        self.reasoning_output_tokens = 0;
        self.deepseek_estimated_usd = 0.0;
        self.deepseek_cache_savings_usd = 0.0;
        self.active_office_agents = 0;
        self.open_office_tasks = 0;
        self.blocked_office_tasks = 0;
        self.manager_consultations = 0;
        self.current_context_tokens = 0;
        self.compaction_count = 0;
        self.queued_steering = 0;
        self.message_target = None;
        self.scroll = 0;
        self.auto_follow = true;
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

pub(crate) const COMMAND_PALETTE: &[(&str, &str)] = &[
    ("/activity", "Open semantic activity"),
    ("/work", "Open tasks and agent TODOs"),
    ("/problems", "Open failures and recovery"),
    ("/status", "Inspect models, context, usage, and cost"),
    ("/context", "Inspect per-agent context"),
    ("/memories", "Review durable memory"),
    ("/books", "Browse verified references"),
    ("/doctor", "Run local diagnostics"),
    ("/new", "Start a fresh conversation"),
    ("/help", "Show keyboard and command help"),
];

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

fn slash_commands() -> &'static [(&'static str, &'static str)] {
    &[
        ("activity", "Open agent activity"),
        ("audit", "Run read-only audit lenses"),
        ("compact", "Compact at the next model boundary"),
        ("context", "Inspect per-agent context"),
        ("diff", "Show the current workspace diff"),
        ("doctor", "Run local diagnostics"),
        ("help", "Show shortcuts and commands"),
        ("implement", "Start implementation mode"),
        ("memory", "Search or manage durable memory"),
        ("memories", "Show memory controls"),
        ("model", "Show provider-aware models"),
        ("new", "Start a new session"),
        ("plan", "Start read-only planning mode"),
        ("problems", "Open incidents and blockers"),
        ("review", "Review the workspace"),
        ("status", "Show runtime status"),
        ("to", "Target the next message to an active agent"),
        ("usage", "Show token and context usage"),
        ("work", "Open tasks and TODOs"),
    ]
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
        assert!(app.completion_items.iter().any(|(value, _)| value == "memory"));
        assert!(app.completion_items.iter().any(|(value, _)| value == "memories"));
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
    fn drawer_cycles_through_operational_tabs() {
        let mut app = app();
        for expected in [
            DrawerTab::Activity,
            DrawerTab::Work,
            DrawerTab::Board,
            DrawerTab::Problems,
        ] {
            app.update(AppAction::ToggleDrawer)
                .expect("test operation should succeed");
            assert!(app.drawer_visible);
            assert_eq!(app.drawer_tab, expected);
        }
        app.update(AppAction::ToggleDrawer)
            .expect("test operation should succeed");
        assert!(!app.drawer_visible);
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
                role: "issue clarifier Luna lead".into(),
                model: "gpt-5.6-luna".into(),
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
                role: "issue clarifier Luna lead".into(),
                model: "gpt-5.6-luna".into(),
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
}
