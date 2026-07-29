use anyhow::Result;
use minha_core::books::{ManifestEntry, SignedRegistryManifest};
use minha_core::protocol::{
    AgentState, BoardEntryView, CatalogModel, EventAgentId, EventEnvelope, ExitState, IncidentView, ItemId,
    PlanTask, RequestId, RunId, RunPhase, RuntimeEvent,
};
use minha_core::runtime::RunKind;
use minha_core::store::{RunRecord, UsageTotals};
use std::collections::HashMap;
use std::path::PathBuf;

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
}

impl TranscriptItem {
    pub(crate) fn agent_id(&self) -> Option<EventAgentId> {
        match self {
            Self::Assistant { agent_id, .. } | Self::Tool { agent_id, .. } => Some(*agent_id),
            Self::User { .. } | Self::Diff { .. } | Self::System { .. } | Self::Status { .. } => None,
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
    Start { kind: RunKind, text: String },
    Continue { run_id: RunId, text: String },
    Steer { run_id: RunId, text: String },
    Answer { run_id: RunId, text: String },
    Interrupt { run_id: RunId },
    Shell { argv: Vec<String>, display: String },
    Quality { action: String },
    GitHub { action: String, number: Option<u64> },
    Resume { run_id: RunId },
    Fork { run_id: RunId },
    Rename { run_id: RunId, title: String },
    Archive { run_id: RunId },
    Compact { run_id: RunId },
    Retry { run_id: RunId, fresh: bool },
    Clean,
    Doctor,
    Login,
    ShowStatus,
    ShowBoard,
    AddNote { text: String },
    PinBoard { id: String },
    ResolveBoard { id: String },
    Export { path: Option<PathBuf> },
    RefreshSessions,
    ShowDiff,
    ShowSkills,
    Quit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AppAction {
    Quit,
    Submit,
    Newline,
    Backspace,
    Escape,
    ToggleDrawer,
    ToggleDetails,
    ToggleTasks,
    HistoryPrevious,
    Help,
    Input(char),
    ScrollUp,
    ScrollDown,
    PageUp,
    PageDown,
    SelectUp,
    SelectDown,
    Activate,
    None,
}

pub struct App {
    pub(crate) root: PathBuf,
    pub(crate) mode: WorkMode,
    pub(crate) input: String,
    pub(crate) items: Vec<TranscriptItem>,
    pub(crate) agents: Vec<AgentView>,
    pub(crate) plan: Vec<PlanTask>,
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
    pub(crate) drawer_tab: DrawerTab,
    pub(crate) tasks_visible: bool,
    pub(crate) details_expanded: bool,
    pub(crate) overlay: Option<Overlay>,
    pub(crate) pending_request: Option<PendingRequest>,
    pub(crate) active_run: Option<RunId>,
    pub(crate) running: bool,
    pub(crate) state: ExitState,
    pub(crate) model: String,
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) cached_input_tokens: u64,
    pub(crate) cache_write_tokens: u64,
    pub(crate) reasoning_output_tokens: u64,
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
    pub(crate) scroll: u16,
    pub(crate) auto_follow: bool,
    pub(crate) status: String,
    pub(crate) phase: RunPhase,
    pub(crate) last_sequence: HashMap<RunId, u64>,
    history: Vec<String>,
    history_cursor: Option<usize>,
    submission: Option<Submission>,
    hive_auto_opened: bool,
}

impl App {
    pub fn new(root: PathBuf, context_limit: u64) -> Self {
        Self {
            root,
            mode: WorkMode::Auto,
            input: String::new(),
            items: Vec::new(),
            agents: Vec::new(),
            plan: Vec::new(),
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
            drawer_tab: DrawerTab::Activity,
            tasks_visible: true,
            details_expanded: false,
            overlay: None,
            pending_request: None,
            active_run: None,
            running: false,
            state: ExitState::Pending,
            model: "gpt-5.6-luna".into(),
            input_tokens: 0,
            output_tokens: 0,
            cached_input_tokens: 0,
            cache_write_tokens: 0,
            reasoning_output_tokens: 0,
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
            scroll: 0,
            auto_follow: true,
            status: "ready".into(),
            phase: RunPhase::Complete,
            last_sequence: HashMap::new(),
            history: Vec::new(),
            history_cursor: None,
            submission: None,
            hive_auto_opened: false,
        }
    }

    pub fn update(&mut self, action: AppAction) -> Result<bool> {
        match action {
            AppAction::Quit => return Ok(true),
            AppAction::Input(character) if !character.is_control() => {
                if self.input.len() < INPUT_LIMIT {
                    self.input.push(character);
                    self.history_cursor = None;
                }
            }
            AppAction::Backspace => {
                self.input.pop();
                self.history_cursor = None;
            }
            AppAction::Newline => {
                if self.input.len() < INPUT_LIMIT {
                    self.input.push('\n');
                    self.history_cursor = None;
                }
            }
            AppAction::Submit => self.submit_input(),
            AppAction::HistoryPrevious => self.history_previous(),
            AppAction::Escape => self.escape(),
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
            AppAction::ScrollUp => {
                self.scroll = self.scroll.saturating_sub(3);
                self.auto_follow = false;
            }
            AppAction::ScrollDown => self.scroll_down(3),
            AppAction::PageUp => {
                self.scroll = self.scroll.saturating_sub(12);
                self.auto_follow = false;
            }
            AppAction::PageDown => self.scroll_down(12),
            AppAction::SelectUp => self.select(-1),
            AppAction::SelectDown => self.select(1),
            AppAction::Activate => self.activate_selection(),
            AppAction::None | AppAction::Input(_) => {}
        }
        Ok(false)
    }

    fn submit_input(&mut self) {
        let text = self.input.trim().to_owned();
        if text.is_empty() {
            return;
        }
        self.input.clear();
        self.history_cursor = None;
        if self.history.last() != Some(&text) {
            self.history.push(text.clone());
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
        self.submission = Some(match self.active_run {
            Some(run_id) => Submission::Continue { run_id, text },
            None => Submission::Start {
                kind: self.mode.run_kind(),
                text,
            },
        });
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
        if self.overlay.take().is_some() {
            return;
        }
        if self.focused_agent.take().is_some() {
            return;
        }
        if self.running
            && let Some(run_id) = self.active_run
        {
            self.submission = Some(Submission::Interrupt { run_id });
            return;
        }
        self.input.clear();
    }

    fn select(&mut self, delta: isize) {
        if matches!(self.overlay, Some(Overlay::Sessions)) {
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
        if matches!(self.overlay, Some(Overlay::Sessions)) {
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
        self.details_expanded = !self.details_expanded;
        for item in &mut self.items {
            match item {
                TranscriptItem::Tool { expanded, .. } | TranscriptItem::Diff { expanded, .. } => {
                    *expanded = self.details_expanded;
                }
                _ => {}
            }
        }
    }

    fn scroll_down(&mut self, amount: u16) {
        self.scroll = self.scroll.saturating_add(amount);
        self.auto_follow = true;
    }

    fn history_previous(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let index = self
            .history_cursor
            .map_or(self.history.len() - 1, |index| index.saturating_sub(1));
        self.history_cursor = Some(index);
        self.input.clone_from(&self.history[index]);
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
        match &envelope.event {
            RuntimeEvent::SessionStarted { .. } => {
                self.active_run = Some(envelope.run_id);
                self.running = true;
                self.state = ExitState::Running;
                self.status = "working".into();
                self.pending_request = None;
                self.phase = RunPhase::Preflight;
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
                    if self.agents.len() == 2 && !self.hive_auto_opened {
                        self.drawer_tab = DrawerTab::Activity;
                        self.drawer_visible = true;
                        self.hive_auto_opened = true;
                    }
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
                if let Some(TranscriptItem::Assistant { text, .. }) = self.items.iter_mut().find(
                    |item| matches!(item, TranscriptItem::Assistant { item_id: id, .. } if id == item_id),
                ) {
                    text.push_str(delta);
                } else {
                    let role = self
                        .agents
                        .iter()
                        .find(|agent| agent.id == *agent_id)
                        .map(|agent| agent.role.clone())
                        .unwrap_or_else(|| "agent".into());
                    self.items.push(TranscriptItem::Assistant {
                        item_id: *item_id,
                        agent_id: *agent_id,
                        role,
                        text: delta.clone(),
                        streaming: true,
                    });
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
            }
            RuntimeEvent::ContextUsage {
                estimated_tokens,
                context_limit,
                compact_at_tokens,
                ..
            } => {
                self.current_context_tokens = *estimated_tokens;
                self.context_limit = (*context_limit).max(1);
                self.compact_at_tokens = *compact_at_tokens;
            }
            RuntimeEvent::RunPhase { phase, detail } => {
                self.phase = *phase;
                self.status = detail.clone();
            }
            RuntimeEvent::ModelCatalog { models, .. } => {
                self.catalog = models.clone();
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
                self.phase = RunPhase::Complete;
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
        self.scroll = u16::MAX;
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
        self.scroll = u16::MAX;
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
        }
        self.scroll = u16::MAX;
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
            }
        }
        out
    }

    fn reset_session(&mut self) {
        self.items.clear();
        self.agents.clear();
        self.plan.clear();
        self.board.clear();
        self.catalog.clear();
        self.incidents.clear();
        self.pending_request = None;
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
        self.active_office_agents = 0;
        self.open_office_tasks = 0;
        self.blocked_office_tasks = 0;
        self.manager_consultations = 0;
        self.current_context_tokens = 0;
        self.compaction_count = 0;
        self.queued_steering = 0;
        self.scroll = 0;
        self.auto_follow = true;
        self.overlay = None;
        self.last_sequence.clear();
        self.phase = RunPhase::Complete;
        self.selected_agent = 0;
        self.selected_task = 0;
        self.selected_board = 0;
        self.selected_problem = 0;
        self.hive_auto_opened = false;
    }

    pub(crate) fn prepare_fresh_session(&mut self) {
        self.reset_session();
        self.status = "starting fresh retry".into();
    }
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
    for mode in ["chat", "implement", "plan", "audit", "review"] {
        visible = visible.replace(&format!("<minha-mode>{mode}</minha-mode>"), "");
    }
    visible.trim().to_owned()
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
    use minha_core::protocol::RuntimeEvent;

    fn app() -> App {
        App::new(PathBuf::from("/tmp/project"), 128_000)
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
    fn second_real_agent_auto_opens_hive_once() {
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
        assert!(app.drawer_visible);
        assert_eq!(app.drawer_tab, DrawerTab::Activity);
        app.drawer_visible = false;
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
    fn context_meter_uses_current_context_not_billed_session_tokens() {
        let mut app = app();
        app.input_tokens = 100_000;
        app.output_tokens = 10_000;
        app.current_context_tokens = 32_000;
        assert_eq!(app.context_percent(), 25.0);
    }
}
