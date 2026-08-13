//! Explainable, token-light issue clarification.

use crate::protocol::{
    AmbiguityDimension, AmbiguityMeter, ClarificationBatch, ClarificationOption, ClarificationQuestion,
    ClarificationStatus, DimensionStatus, IssueBrief, IssueClarificationView, IssueKind,
};
use serde::Deserialize;

pub const INTAKE_SCHEMA_VERSION: u16 = 1;
pub const READY_THRESHOLD: u8 = 25;
/// Maximum clarification batches before remaining uncertainty is delegated.
pub const MAX_CLARIFICATION_ROUNDS: u32 = 4;

const GOAL: &str = "goal";
const REPRODUCTION: &str = "reproduction";
const SCOPE: &str = "scope";
const CONSTRAINTS: &str = "constraints";
const SUCCESS: &str = "success";

pub fn analyze(goal: &str, requested_kind: &str) -> IssueClarificationView {
    let issue_kind = classify_issue_kind(goal, requested_kind);
    let lower = goal.to_ascii_lowercase();
    let words = goal.split_whitespace().count();
    let vague = is_vague(&lower, words);
    let has_path = goal.split_whitespace().any(looks_like_path)
        || [
            "tui", "cli", "parser", "runtime", "cache", "auth", "database", "api",
        ]
        .iter()
        .any(|term| lower.contains(term));
    let has_reproduction = [
        " when ",
        " after ",
        " before ",
        "every time",
        "sometimes",
        "steps",
        "repro",
        "error:",
        "panic",
        "crash",
        "fails",
        "failed",
    ]
    .iter()
    .any(|term| lower.contains(term));
    let has_constraints = [
        "do not", "don't", "must not", "preserve", "only", "without", "keep ",
    ]
    .iter()
    .any(|term| lower.contains(term));
    let has_success = [
        "should",
        "expected",
        "acceptance",
        "test",
        "passes",
        "works when",
        "done when",
    ]
    .iter()
    .any(|term| lower.contains(term));
    let simple_scoped_edit = has_path
        && words <= 10
        && ["typo", "spelling", "wording", "format", "formatting", "docs"]
            .iter()
            .any(|term| lower.contains(term));
    let explicit_broad_workflow = matches!(requested_kind, "audit" | "review")
        && ["repository", "workspace", "current diff", "codebase"]
            .iter()
            .any(|term| lower.contains(term));

    let goal_status = if matches!(issue_kind, IssueKind::Question) || explicit_broad_workflow {
        DimensionStatus::Confirmed
    } else if vague {
        DimensionStatus::Unknown
    } else if words >= 8 {
        DimensionStatus::Confirmed
    } else {
        DimensionStatus::Partial
    };
    let reproduction_status =
        if simple_scoped_edit || !matches!(issue_kind, IssueKind::Defect | IssueKind::Unknown) {
            DimensionStatus::NotApplicable
        } else if has_reproduction {
            DimensionStatus::Confirmed
        } else if lower.contains("bug") || lower.contains("broken") || lower.contains("doesn't work") {
            DimensionStatus::Partial
        } else {
            DimensionStatus::Unknown
        };
    let scope_status = if has_path || explicit_broad_workflow {
        DimensionStatus::Confirmed
    } else if matches!(issue_kind, IssueKind::Question) || words >= 10 {
        DimensionStatus::Partial
    } else {
        DimensionStatus::Unknown
    };
    let constraints_status = if matches!(issue_kind, IssueKind::Question) {
        DimensionStatus::NotApplicable
    } else if has_constraints {
        DimensionStatus::Confirmed
    } else if high_risk(&lower) {
        DimensionStatus::Unknown
    } else {
        DimensionStatus::Inferred
    };
    let success_status = if matches!(issue_kind, IssueKind::Question) {
        DimensionStatus::NotApplicable
    } else if has_success {
        DimensionStatus::Confirmed
    } else if simple_scoped_edit || explicit_broad_workflow {
        DimensionStatus::Inferred
    } else if words >= 12 {
        DimensionStatus::Partial
    } else {
        DimensionStatus::Unknown
    };

    let dimensions = vec![
        dimension(GOAL, "goal", 25, goal_status, (!vague).then_some(goal)),
        dimension(REPRODUCTION, "reproduction", 25, reproduction_status, None),
        dimension(SCOPE, "scope", 20, scope_status, None),
        dimension(CONSTRAINTS, "constraints", 15, constraints_status, None),
        dimension(SUCCESS, "success criteria", 15, success_status, None),
    ];
    let meter = score(dimensions);
    IssueClarificationView {
        schema_version: INTAKE_SCHEMA_VERSION,
        status: ClarificationStatus::Collecting,
        issue_kind,
        round: 0,
        meter,
        pending_batch: None,
        brief: None,
    }
}

pub fn needs_clarification(view: &IssueClarificationView) -> bool {
    let unknown = |id: &str| {
        view.meter
            .dimensions
            .iter()
            .find(|dimension| dimension.id == id)
            .is_some_and(|dimension| dimension.status == DimensionStatus::Unknown)
    };
    // Automatic clarification is a safety boundary, not a confidence meter.
    // Ordinary underspecification is resolved through inspection and explicit
    // assumptions; stop only when a high-impact constraint and the target of
    // that action are both unknown.
    unknown(CONSTRAINTS) && (unknown(GOAL) || unknown(SCOPE))
}

pub fn should_consult_terra(view: &IssueClarificationView, goal: &str) -> bool {
    if view.round < 2 || view.meter.overall < 50 {
        return false;
    }
    let lower = goal.to_ascii_lowercase();
    [
        "security",
        "credential",
        "production",
        "database",
        "migration",
        "delete",
        "data loss",
        "architecture",
    ]
    .iter()
    .any(|term| lower.contains(term))
}

pub fn apply_answers(view: &mut IssueClarificationView, answers: &[(String, String)]) {
    for (question_id, raw_answer) in answers {
        if question_id.starts_with("$note:") {
            continue;
        }
        let answer = raw_answer.trim();
        if question_id == "$action" {
            apply_action(view, answer);
            continue;
        }
        let question = view.pending_batch.as_ref().and_then(|batch| {
            batch
                .questions
                .iter()
                .find(|question| question.id == *question_id)
        });
        let dimension_id = question
            .map(|question| question.dimension.clone())
            .unwrap_or_else(|| question_id.split('-').next().unwrap_or(question_id).to_owned());
        let Some(dimension) = view
            .meter
            .dimensions
            .iter_mut()
            .find(|dimension| dimension.id == dimension_id)
        else {
            continue;
        };
        if answer.eq_ignore_ascii_case("not sure") || answer.is_empty() {
            continue;
        }
        if answer.eq_ignore_ascii_case("use your best judgment")
            || answer.eq_ignore_ascii_case("best judgment")
        {
            dimension.status = DimensionStatus::Delegated;
            dimension.detail = "Delegated to Minha; use the safest repository-supported assumption.".into();
        } else {
            dimension.status = DimensionStatus::Confirmed;
            dimension.detail = question
                .and_then(|question| {
                    question
                        .options
                        .iter()
                        .find(|option| option.value.eq_ignore_ascii_case(answer))
                })
                .map(|option| format!("{}: {}", option.label, option.description))
                .unwrap_or_else(|| answer.chars().take(2_000).collect());
        }
    }
    for (question_id, note) in answers {
        let Some(answered_question) = question_id.strip_prefix("$note:") else {
            continue;
        };
        let Some(dimension_id) = answered_question.split('-').next() else {
            continue;
        };
        if note.trim().is_empty() {
            continue;
        }
        if let Some(dimension) = view
            .meter
            .dimensions
            .iter_mut()
            .find(|dimension| dimension.id == dimension_id)
        {
            dimension.detail.push_str(" Note: ");
            dimension.detail.extend(note.trim().chars().take(2_000));
        }
    }
    view.meter = score(std::mem::take(&mut view.meter.dimensions));
    view.pending_batch = None;
    if view.status == ClarificationStatus::Collecting && ready_for_review(view) {
        view.status = ClarificationStatus::Reviewing;
    }
}

pub fn reopen(view: &mut IssueClarificationView, note: Option<&str>) {
    view.status = ClarificationStatus::Collecting;
    view.brief = None;
    view.pending_batch = None;
    let note = note.filter(|note| !note.trim().is_empty());
    let target_index = if note.is_some() {
        view.meter
            .dimensions
            .iter()
            .position(|dimension| dimension.id == GOAL)
    } else {
        view.meter
            .dimensions
            .iter()
            .position(|dimension| {
                matches!(
                    dimension.status,
                    DimensionStatus::Inferred | DimensionStatus::Delegated
                )
            })
            .or_else(|| {
                view.meter
                    .dimensions
                    .iter()
                    .position(|dimension| dimension.id == SUCCESS)
            })
    };
    if let Some(target) = target_index.and_then(|index| view.meter.dimensions.get_mut(index)) {
        target.status = DimensionStatus::Partial;
        if let Some(note) = note {
            target.detail = note.trim().chars().take(2_000).collect();
        }
    }
    view.meter = score(std::mem::take(&mut view.meter.dimensions));
}

pub fn prepare_brief(view: &mut IssueClarificationView, original_goal: &str) {
    view.status = ClarificationStatus::Reviewing;
    view.pending_batch = None;
    view.brief = Some(build_brief(view, original_goal));
}

pub fn confirm(view: &mut IssueClarificationView) {
    view.status = ClarificationStatus::Confirmed;
    view.pending_batch = None;
}

/// After a bounded number of clarification rounds, every still-unknown
/// dimension is delegated so a user who never picks an option cannot loop
/// forever (each round spends model tokens).
pub fn exhaust_rounds(view: &mut IssueClarificationView, original_goal: &str) {
    for dimension in view.meter.dimensions.iter_mut() {
        if dimension.status == DimensionStatus::Unknown {
            dimension.status = DimensionStatus::Delegated;
            dimension.detail =
                "Clarification rounds exhausted; Minha used the safest repository-supported assumption."
                    .into();
        }
    }
    view.meter = score(std::mem::take(&mut view.meter.dimensions));
    prepare_brief(view, original_goal);
}

pub fn make_fallback_batch(view: &IssueClarificationView) -> ClarificationBatch {
    let questions = unresolved_dimensions(view)
        .into_iter()
        .take(3)
        .map(|dimension| fallback_question(dimension, view.round.saturating_add(1)))
        .collect();
    ClarificationBatch {
        round: view.round.saturating_add(1),
        questions,
        actions: vec!["use_best_judgment".into(), "summarize".into(), "cancel".into()],
    }
}

pub fn sanitize_model_batch(text: &str, view: &IssueClarificationView) -> Option<ClarificationBatch> {
    #[derive(Deserialize)]
    struct ModelBatch {
        questions: Vec<ClarificationQuestion>,
    }
    let payload = tagged_payload(text, "minha-clarification")?;
    let decoded: ModelBatch = serde_json::from_str(payload).ok()?;
    let unresolved = unresolved_dimensions(view)
        .into_iter()
        .map(|dimension| dimension.id.as_str())
        .collect::<Vec<_>>();
    let mut questions = Vec::new();
    for mut question in decoded.questions {
        if questions.len() == 3
            || !unresolved.contains(&question.dimension.as_str())
            || questions
                .iter()
                .any(|existing: &ClarificationQuestion| existing.dimension == question.dimension)
        {
            continue;
        }
        question.id = format!("{}-{}", question.dimension, view.round.saturating_add(1));
        question.header = question.header.chars().take(24).collect();
        question.question = question.question.chars().take(300).collect();
        question.options.truncate(3);
        for option in &mut question.options {
            option.value = option.value.chars().take(80).collect();
            option.label = option.label.chars().take(80).collect();
            option.description = option.description.chars().take(180).collect();
        }
        if question.options.len() >= 2 {
            questions.push(question);
        }
    }
    (!questions.is_empty()).then_some(ClarificationBatch {
        round: view.round.saturating_add(1),
        questions,
        actions: vec!["use_best_judgment".into(), "summarize".into(), "cancel".into()],
    })
}

pub fn render_brief(brief: &IssueBrief) -> String {
    fn list(values: &[String]) -> String {
        if values.is_empty() {
            "- none supplied".into()
        } else {
            values
                .iter()
                .map(|value| format!("- {value}"))
                .collect::<Vec<_>>()
                .join("\n")
        }
    }
    format!(
        "Confirmed issue brief\n\nObserved:\n{}\n\nExpected:\n{}\n\nReproduction:\n{}\n\nEvidence:\n{}\n\nScope:\n{}\n\nConstraints:\n{}\n\nSuccess criteria:\n{}\n\nAssumptions:\n{}\n\nRecommended workflow: {}\nAmbiguity at confirmation: {}/100",
        brief.observed,
        brief.expected,
        list(&brief.reproduction),
        list(&brief.evidence),
        list(&brief.scope),
        list(&brief.constraints),
        list(&brief.success_criteria),
        list(&brief.assumptions),
        brief.recommended_workflow,
        brief.meter.overall,
    )
}

fn apply_action(view: &mut IssueClarificationView, action: &str) {
    match action {
        "use_best_judgment" => {
            for dimension in &mut view.meter.dimensions {
                if matches!(
                    dimension.status,
                    DimensionStatus::Unknown | DimensionStatus::Partial
                ) {
                    dimension.status = DimensionStatus::Delegated;
                    if dimension.detail.is_empty() {
                        dimension.detail = "Delegated to Minha.".into();
                    }
                }
            }
        }
        "summarize" => view.status = ClarificationStatus::Reviewing,
        "cancel" => view.status = ClarificationStatus::Cancelled,
        "confirm" => view.status = ClarificationStatus::Confirmed,
        "keep_clarifying" => view.status = ClarificationStatus::Collecting,
        _ => {}
    }
}

fn ready_for_review(view: &IssueClarificationView) -> bool {
    view.meter.overall <= READY_THRESHOLD
        && !view
            .meter
            .dimensions
            .iter()
            .any(|dimension| dimension.status == DimensionStatus::Unknown)
}

fn build_brief(view: &IssueClarificationView, original_goal: &str) -> IssueBrief {
    let detail = |id: &str| {
        view.meter
            .dimensions
            .iter()
            .find(|dimension| dimension.id == id)
            .map(|dimension| dimension.detail.trim())
            .filter(|detail| !detail.is_empty())
            .unwrap_or("")
            .to_owned()
    };
    let reproduction = detail(REPRODUCTION);
    let scope = detail(SCOPE);
    let constraints = detail(CONSTRAINTS);
    let success = detail(SUCCESS);
    let assumptions = view
        .meter
        .dimensions
        .iter()
        .filter(|dimension| {
            matches!(
                dimension.status,
                DimensionStatus::Inferred | DimensionStatus::Delegated | DimensionStatus::Unknown
            )
        })
        .map(|dimension| {
            if dimension.detail.is_empty() {
                format!(
                    "Use the safest repository-supported assumption for {}.",
                    dimension.label
                )
            } else {
                format!("{}: {}", dimension.label, dimension.detail)
            }
        })
        .collect();
    IssueBrief {
        issue_kind: view.issue_kind,
        observed: if original_goal.trim().is_empty() {
            detail(GOAL)
        } else {
            original_goal.trim().to_owned()
        },
        expected: {
            let value = detail(GOAL);
            if value.is_empty() {
                "Resolve the observed problem without widening scope.".into()
            } else {
                value
            }
        },
        reproduction: nonempty_vec(reproduction),
        evidence: referenced_paths(original_goal),
        scope: nonempty_vec(scope),
        constraints: nonempty_vec(constraints),
        success_criteria: nonempty_vec(success),
        assumptions,
        recommended_workflow: match view.issue_kind {
            IssueKind::Audit => "audit",
            IssueKind::Review => "review",
            IssueKind::Question => "chat",
            IssueKind::Defect | IssueKind::Feature | IssueKind::Unknown => "implement",
        }
        .into(),
        meter: view.meter.clone(),
    }
}

fn score(dimensions: Vec<AmbiguityDimension>) -> AmbiguityMeter {
    let mut possible = 0_u32;
    let mut uncertain = 0_u32;
    for dimension in &dimensions {
        if dimension.status == DimensionStatus::NotApplicable {
            continue;
        }
        possible += u32::from(dimension.weight) * 4;
        let factor = match dimension.status {
            DimensionStatus::Unknown => 4,
            DimensionStatus::Partial => 2,
            DimensionStatus::Inferred | DimensionStatus::Delegated => 1,
            DimensionStatus::Confirmed | DimensionStatus::NotApplicable => 0,
        };
        uncertain += u32::from(dimension.weight) * factor;
    }
    let overall = if possible == 0 {
        0
    } else {
        uncertain.saturating_mul(100).div_ceil(possible).min(100) as u8
    };
    AmbiguityMeter { overall, dimensions }
}

fn dimension(
    id: &str,
    label: &str,
    weight: u8,
    status: DimensionStatus,
    detail: Option<&str>,
) -> AmbiguityDimension {
    AmbiguityDimension {
        id: id.into(),
        label: label.into(),
        weight,
        status,
        detail: detail.unwrap_or_default().chars().take(2_000).collect(),
    }
}

fn unresolved_dimensions(view: &IssueClarificationView) -> Vec<&AmbiguityDimension> {
    let mut dimensions = view
        .meter
        .dimensions
        .iter()
        .filter(|dimension| {
            matches!(
                dimension.status,
                DimensionStatus::Unknown | DimensionStatus::Partial
            )
        })
        .collect::<Vec<_>>();
    dimensions.sort_by_key(|dimension| std::cmp::Reverse(dimension.weight));
    dimensions
}

fn fallback_question(dimension: &AmbiguityDimension, round: u32) -> ClarificationQuestion {
    let (header, question, options) = match dimension.id.as_str() {
        GOAL => (
            "What happened?",
            "Which description is closest to the problem you noticed?",
            vec![
                option(
                    "fails",
                    "Something fails",
                    "An action errors, crashes, or never completes.",
                    true,
                ),
                option(
                    "wrong",
                    "The result is wrong",
                    "It completes, but the output or behavior is incorrect.",
                    false,
                ),
                option(
                    "missing",
                    "Something is missing",
                    "You expected a capability or control that is not there.",
                    false,
                ),
            ],
        ),
        REPRODUCTION => (
            "When?",
            "When are you able to notice the problem?",
            vec![
                option(
                    "every_time",
                    "Every time",
                    "The same action reliably causes it.",
                    true,
                ),
                option(
                    "specific",
                    "After a specific action",
                    "It depends on a command, file, or sequence.",
                    false,
                ),
                option(
                    "sometimes",
                    "Only sometimes",
                    "It appears intermittent or timing-dependent.",
                    false,
                ),
            ],
        ),
        SCOPE => (
            "Where?",
            "Where do you notice the problem most clearly?",
            vec![
                option(
                    "tui",
                    "Terminal interface",
                    "The visible TUI, input, navigation, or rendering.",
                    false,
                ),
                option(
                    "agents",
                    "Agent work",
                    "Planning, tools, branches, questions, or completion.",
                    true,
                ),
                option(
                    "cli",
                    "Command line",
                    "A Minha command, JSON response, login, or local operation.",
                    false,
                ),
            ],
        ),
        CONSTRAINTS => (
            "Keep safe",
            "What is most important not to disturb while fixing this?",
            vec![
                option(
                    "behavior",
                    "Existing behavior",
                    "Keep unrelated commands and workflows compatible.",
                    true,
                ),
                option(
                    "data",
                    "Local data",
                    "Protect sessions, credentials, configuration, and recovery state.",
                    false,
                ),
                option(
                    "nothing",
                    "No special constraint",
                    "Use normal repository safety rules.",
                    false,
                ),
            ],
        ),
        _ => (
            "Done looks like",
            "What would make you confident the issue is fixed?",
            vec![
                option(
                    "works",
                    "The action works",
                    "The original workflow completes normally.",
                    true,
                ),
                option(
                    "test",
                    "A regression test passes",
                    "The failure is reproduced and permanently covered.",
                    false,
                ),
                option(
                    "clear",
                    "The result is clear",
                    "The output or interface explains the state correctly.",
                    false,
                ),
            ],
        ),
    };
    ClarificationQuestion {
        id: format!("{}-{round}", dimension.id),
        dimension: dimension.id.clone(),
        header: header.into(),
        question: question.into(),
        options,
        allow_free_text: true,
        allow_not_sure: true,
    }
}

fn option(value: &str, label: &str, description: &str, recommended: bool) -> ClarificationOption {
    ClarificationOption {
        value: value.into(),
        label: label.into(),
        description: description.into(),
        recommended,
    }
}

fn classify_issue_kind(goal: &str, requested: &str) -> IssueKind {
    let lower = goal.to_ascii_lowercase();
    if requested == "audit" || lower.contains("audit") {
        IssueKind::Audit
    } else if requested == "review" || lower.contains("review") {
        IssueKind::Review
    } else if [
        "bug",
        "fix ",
        "broken",
        "error",
        "fail",
        "crash",
        "panic",
        "doesn't work",
        "not working",
    ]
    .iter()
    .any(|term| lower.contains(term))
    {
        IssueKind::Defect
    } else if ["add ", "build ", "implement", "feature", "support "]
        .iter()
        .any(|term| lower.contains(term))
    {
        IssueKind::Feature
    } else if requested == "auto" && (lower.ends_with('?') || lower.starts_with("how ")) {
        IssueKind::Question
    } else {
        IssueKind::Unknown
    }
}

fn is_vague(lower: &str, words: usize) -> bool {
    words < 4
        || [
            "it is broken",
            "it's broken",
            "it doesnt work",
            "it doesn't work",
            "fix it",
            "help",
            "bad",
        ]
        .iter()
        .any(|phrase| lower.trim() == *phrase)
}

fn high_risk(lower: &str) -> bool {
    [
        "security",
        "credential",
        "production",
        "database",
        "migration",
        "delete",
        "data loss",
    ]
    .iter()
    .any(|term| lower.contains(term))
}

fn looks_like_path(word: &str) -> bool {
    let trimmed = word.trim_matches(|character: char| ",:;()[]{}'\"`".contains(character));
    if trimmed.contains('/') {
        return true;
    }
    let lower = trimmed.to_ascii_lowercase();
    [
        ".rs", ".toml", ".md", ".json", ".yaml", ".yml", ".log", ".heic", ".png", ".jpg", ".jpeg", ".webp",
    ]
    .iter()
    .any(|extension| lower.ends_with(extension))
}

fn referenced_paths(text: &str) -> Vec<String> {
    let mut paths = absolute_spaced_paths(text);
    for token in shellish_tokens(text) {
        let token = token
            .trim_matches(|character: char| ",:;()[]{}'\"`".contains(character))
            .to_owned();
        if looks_like_path(&token)
            && !paths.contains(&token)
            && !paths.iter().any(|path| path.contains(&token))
        {
            paths.push(token);
        }
        if paths.len() == 12 {
            break;
        }
    }
    paths.truncate(12);
    paths
}

fn absolute_spaced_paths(text: &str) -> Vec<String> {
    const EXTENSIONS: [&str; 6] = [".heic", ".png", ".jpg", ".jpeg", ".webp", ".log"];
    let mut paths = Vec::new();
    for line in text.lines() {
        let lower = line.to_ascii_lowercase();
        for extension in EXTENSIONS {
            let mut search_from = 0;
            while let Some(offset) = lower[search_from..].find(extension) {
                let end = search_from + offset + extension.len();
                let start = line[..end]
                    .char_indices()
                    .filter(|(index, character)| {
                        *character == '/'
                            && (*index == 0
                                || line[..*index]
                                    .chars()
                                    .next_back()
                                    .is_some_and(char::is_whitespace))
                    })
                    .map(|(index, _)| index)
                    .next_back();
                if let Some(start) = start {
                    let path = line[start..end].replace("\\ ", " ");
                    if !paths.contains(&path) {
                        paths.push(path);
                    }
                }
                search_from = end;
            }
        }
    }
    paths
}

fn shellish_tokens(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut token = String::new();
    let mut quote = None;
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        if let Some(active_quote) = quote {
            if character == active_quote {
                quote = None;
            } else {
                token.push(character);
            }
        } else if matches!(character, '\'' | '"') {
            quote = Some(character);
        } else if character == '\\'
            && characters
                .peek()
                .is_some_and(|next| next.is_whitespace() || matches!(*next, '\\' | '\'' | '"'))
        {
            if let Some(escaped) = characters.next() {
                token.push(escaped);
            }
        } else if character.is_whitespace() {
            if !token.is_empty() {
                tokens.push(std::mem::take(&mut token));
            }
        } else {
            token.push(character);
        }
    }
    if !token.is_empty() {
        tokens.push(token);
    }
    tokens
}

fn nonempty_vec(value: String) -> Vec<String> {
    (!value.trim().is_empty()).then_some(value).into_iter().collect()
}

fn tagged_payload<'a>(text: &'a str, tag: &str) -> Option<&'a str> {
    let opening = format!("<{tag}>");
    let closing = format!("</{tag}>");
    let start = text.find(&opening)? + opening.len();
    let tail = &text[start..];
    let end = tail.find(&closing)?;
    Some(tail[..end].trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_conversation_and_vague_defects_do_not_trigger_intake() {
        let vague = analyze("it doesn't work", "auto");
        assert!(!needs_clarification(&vague));

        let greeting = analyze("hello", "auto");
        assert!(!needs_clarification(&greeting));

        let unsafe_unknown = analyze("delete it", "implement");
        assert!(needs_clarification(&unsafe_unknown));

        let clear = analyze(
            "Fix the panic in src/parser.rs when input is empty; preserve syntax and add a regression test.",
            "implement",
        );
        assert!(!needs_clarification(&clear));

        let simple = analyze("fix typo in README.md", "auto");
        assert!(!needs_clarification(&simple));

        let question = analyze("How do I run the tests?", "auto");
        assert!(!needs_clarification(&question));

        let audit = analyze("audit the repository", "audit");
        assert!(!needs_clarification(&audit));
    }

    #[test]
    fn exhausted_rounds_delegate_unknowns_and_prepare_a_brief() {
        let mut view = analyze("delete it", "implement");
        assert!(needs_clarification(&view));
        assert!(
            view.meter
                .dimensions
                .iter()
                .any(|dimension| dimension.status == DimensionStatus::Unknown)
        );
        exhaust_rounds(&mut view, "delete it");
        assert_eq!(view.status, ClarificationStatus::Reviewing);
        assert!(view.brief.is_some());
        assert!(
            view.meter
                .dimensions
                .iter()
                .all(|dimension| dimension.status != DimensionStatus::Unknown),
            "every unknown dimension must be delegated"
        );
        assert!(view.pending_batch.is_none());
    }

    #[test]
    fn not_applicable_weights_are_normalized_and_answers_move_the_meter() {
        let mut view = analyze("add a better status screen", "implement");
        assert_eq!(
            view.meter
                .dimensions
                .iter()
                .find(|dimension| dimension.id == REPRODUCTION)
                .map(|dimension| dimension.status),
            Some(DimensionStatus::NotApplicable)
        );
        let batch = make_fallback_batch(&view);
        let first = batch.questions[0].id.clone();
        let before = view.meter.overall;
        view.pending_batch = Some(batch);
        apply_answers(
            &mut view,
            &[(first, "The status view hides cache failures".into())],
        );
        assert!(view.meter.overall < before);
    }

    #[test]
    fn delegation_preserves_an_explicit_assumption() {
        let mut view = analyze("broken", "auto");
        view.pending_batch = Some(make_fallback_batch(&view));
        apply_answers(&mut view, &[("$action".into(), "use_best_judgment".into())]);
        prepare_brief(&mut view, "broken");
        let brief = view.brief.as_ref().expect("brief");
        assert!(!brief.assumptions.is_empty());
        assert!(brief.meter.overall <= 25);
    }

    #[test]
    fn option_values_become_human_readable_brief_details() {
        let mut view = analyze("broken", "auto");
        let batch = make_fallback_batch(&view);
        let question = batch.questions[0].clone();
        let option = question.options[0].clone();
        view.pending_batch = Some(batch);

        apply_answers(&mut view, &[(question.id, option.value)]);

        let detail = view
            .meter
            .dimensions
            .iter()
            .find(|dimension| dimension.id == question.dimension)
            .map(|dimension| dimension.detail.as_str());
        assert_eq!(
            detail,
            Some(format!("{}: {}", option.label, option.description).as_str())
        );
    }

    #[test]
    fn model_batches_are_bounded_to_unresolved_dimensions() {
        let view = analyze("broken", "auto");
        let payload = r#"<minha-clarification>{"questions":[
          {"id":"x","dimension":"goal","header":"Goal","question":"What happened?","options":[{"value":"a","label":"A","description":"First","recommended":true},{"value":"b","label":"B","description":"Second","recommended":false}],"allow_free_text":true,"allow_not_sure":true},
          {"id":"x","dimension":"bogus","header":"No","question":"Ignore me","options":[{"value":"a","label":"A","description":"First","recommended":true},{"value":"b","label":"B","description":"Second","recommended":false}],"allow_free_text":true,"allow_not_sure":true}
        ]}</minha-clarification>"#;

        let batch = sanitize_model_batch(payload, &view).expect("valid bounded batch");
        assert_eq!(batch.questions.len(), 1);
        assert_eq!(batch.questions[0].dimension, GOAL);
        assert_eq!(batch.questions[0].id, "goal-1");
    }

    #[test]
    fn terra_is_reserved_for_persistent_high_impact_ambiguity() {
        let mut view = analyze("production database migration is broken", "auto");
        assert!(!should_consult_terra(&view, "production database migration"));
        view.round = 2;
        assert!(should_consult_terra(&view, "production database migration"));
        assert!(!should_consult_terra(&view, "a local color is wrong"));
    }

    #[test]
    fn reopening_a_review_always_creates_something_useful_to_clarify() {
        let mut view = analyze("broken", "auto");
        apply_answers(&mut view, &[("$action".into(), "use_best_judgment".into())]);
        prepare_brief(&mut view, "broken");

        reopen(&mut view, None);

        assert_eq!(view.status, ClarificationStatus::Collecting);
        assert!(!make_fallback_batch(&view).questions.is_empty());
    }

    #[test]
    fn screenshot_paths_with_spaces_remain_whole_evidence_references() {
        let report = "/var/tmp/Screenshot\\ 2026-07-29\\ at\\ 1.33.54 PM.heic\nThe colors look wrong.";
        assert_eq!(
            referenced_paths(report),
            ["/var/tmp/Screenshot 2026-07-29 at 1.33.54 PM.heic"]
        );
    }
}
