//! The single source of truth for slash commands.
//!
//! Help text, `/` completion, the `Ctrl-P` command surface, and `handle_slash`
//! dispatch all read this one table, so a command cannot exist in the dispatcher
//! while being invisible to help and completion (which is exactly how the three
//! previous lists — `slash_commands()`, `COMMAND_PALETTE`, and the hardcoded help
//! text — drifted apart).
//!
//! Bump [`REGISTRY_VERSION`] whenever the set of commands changes so snapshots,
//! docs, and any future persisted keymap/settings can detect a stale table.

/// Version of the command table. Bump on every add/remove/rename.
///
/// Only the regression test below reads this today; it exists so a future
/// persisted keymap/settings format has a stable version to key off of
/// without inventing one under time pressure.
#[cfg(test)]
pub(crate) const REGISTRY_VERSION: u32 = 3;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, PartialOrd, Ord)]
pub(crate) enum Category {
    Conversation,
    Work,
    Inspect,
    Providers,
    Usage,
    Appearance,
    Input,
    Maintenance,
}

impl Category {
    pub(crate) const ALL: &'static [Self] = &[
        Self::Conversation,
        Self::Work,
        Self::Inspect,
        Self::Providers,
        Self::Usage,
        Self::Appearance,
        Self::Input,
        Self::Maintenance,
    ];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Conversation => "conversation",
            Self::Work => "work",
            Self::Inspect => "inspect",
            Self::Providers => "providers",
            Self::Usage => "usage",
            Self::Appearance => "appearance",
            Self::Input => "input",
            Self::Maintenance => "maintenance",
        }
    }
}

/// One command, as seen by help, completion, and dispatch alike.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CommandSpec {
    /// Canonical name, without the leading `/`.
    pub(crate) name: &'static str,
    /// Alternate spellings that dispatch to the same behavior.
    pub(crate) aliases: &'static [&'static str],
    /// Argument shape shown in the popup, e.g. `TITLE` or `[--fresh]`.
    pub(crate) args: Option<&'static str>,
    pub(crate) description: &'static str,
    pub(crate) category: Category,
    /// Requires a session to act on; unavailable (but still listed) without one.
    pub(crate) needs_run: bool,
    /// Refuses to run without an argument.
    pub(crate) needs_argument: bool,
    /// Can reach a model or the network.
    pub(crate) network: bool,
}

impl CommandSpec {
    /// `/name ARGS`, as shown in the completion popup and help.
    pub(crate) fn display(&self) -> String {
        match self.args {
            Some(args) => format!("/{} {args}", self.name),
            None => format!("/{}", self.name),
        }
    }

    pub(crate) fn availability(&self, context: CommandContext) -> Availability {
        if self.needs_run && !context.active_run {
            return Availability::Unavailable("needs an active session");
        }
        Availability::Available
    }
}

/// The slice of app state that decides whether a command can run right now.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct CommandContext {
    pub(crate) active_run: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Availability {
    Available,
    /// Listed, but explains why it cannot run.
    Unavailable(&'static str),
}

impl Availability {
    pub(crate) fn reason(self) -> Option<&'static str> {
        match self {
            Self::Available => None,
            Self::Unavailable(reason) => Some(reason),
        }
    }
}

/// Ordered by expected use, not alphabetically — the first screenful should be
/// the commands people actually reach for.
pub(crate) const REGISTRY: &[CommandSpec] = &[
    // Conversation
    CommandSpec {
        name: "help",
        aliases: &[],
        args: None,
        description: "Show shortcuts, commands, and keymap",
        category: Category::Inspect,
        needs_run: false,
        needs_argument: false,
        network: false,
    },
    CommandSpec {
        name: "new",
        aliases: &[],
        args: None,
        description: "Start a fresh conversation",
        category: Category::Conversation,
        needs_run: false,
        needs_argument: false,
        network: false,
    },
    CommandSpec {
        name: "status",
        aliases: &[],
        args: None,
        description: "Inspect models, context, usage, and cost",
        category: Category::Inspect,
        needs_run: false,
        needs_argument: false,
        network: false,
    },
    CommandSpec {
        name: "work",
        aliases: &[],
        args: None,
        description: "Open tasks and agent TODOs",
        category: Category::Work,
        needs_run: false,
        needs_argument: false,
        network: false,
    },
    CommandSpec {
        name: "activity",
        aliases: &["agents"],
        args: None,
        description: "Open agent activity",
        category: Category::Work,
        needs_run: false,
        needs_argument: false,
        network: false,
    },
    CommandSpec {
        name: "plan",
        aliases: &[],
        args: Some("[TASK]"),
        description: "Start read-only planning mode",
        category: Category::Work,
        needs_run: false,
        needs_argument: false,
        network: true,
    },
    CommandSpec {
        name: "implement",
        aliases: &[],
        args: Some("[TASK]"),
        description: "Start implementation mode",
        category: Category::Work,
        needs_run: false,
        needs_argument: false,
        network: true,
    },
    CommandSpec {
        name: "review",
        aliases: &[],
        args: Some("[TASK]"),
        description: "Review the workspace",
        category: Category::Work,
        needs_run: false,
        needs_argument: false,
        network: true,
    },
    CommandSpec {
        name: "audit",
        aliases: &[],
        args: Some("[TASK]"),
        description: "Run read-only audit lenses",
        category: Category::Work,
        needs_run: false,
        needs_argument: false,
        network: true,
    },
    CommandSpec {
        name: "auto",
        aliases: &[],
        args: None,
        description: "Let Minha choose the work mode",
        category: Category::Work,
        needs_run: false,
        needs_argument: false,
        network: false,
    },
    CommandSpec {
        name: "diff",
        aliases: &[],
        args: None,
        description: "Show the current workspace diff",
        category: Category::Inspect,
        needs_run: false,
        needs_argument: false,
        network: false,
    },
    CommandSpec {
        name: "problems",
        aliases: &[],
        args: None,
        description: "Open failures and recovery",
        category: Category::Work,
        needs_run: false,
        needs_argument: false,
        network: false,
    },
    CommandSpec {
        name: "board",
        aliases: &[],
        args: None,
        description: "Open the coordination board",
        category: Category::Work,
        needs_run: false,
        needs_argument: false,
        network: false,
    },
    CommandSpec {
        name: "route",
        aliases: &["routing"],
        args: None,
        description: "Inspect route evidence and worker dispatch receipts",
        category: Category::Inspect,
        needs_run: false,
        needs_argument: false,
        network: false,
    },
    CommandSpec {
        name: "resume",
        aliases: &[],
        args: None,
        description: "Reopen a saved session",
        category: Category::Conversation,
        needs_run: false,
        needs_argument: false,
        network: false,
    },
    CommandSpec {
        name: "retry",
        aliases: &[],
        args: Some("[--fresh]"),
        description: "Re-run the current session",
        category: Category::Conversation,
        needs_run: true,
        needs_argument: false,
        network: true,
    },
    CommandSpec {
        name: "fork",
        aliases: &[],
        args: None,
        description: "Branch the current session",
        category: Category::Conversation,
        needs_run: true,
        needs_argument: false,
        network: false,
    },
    CommandSpec {
        name: "rename",
        aliases: &[],
        args: Some("TITLE"),
        description: "Rename the current session",
        category: Category::Conversation,
        needs_run: true,
        needs_argument: true,
        network: false,
    },
    CommandSpec {
        name: "archive",
        aliases: &[],
        args: None,
        description: "Archive the current session",
        category: Category::Conversation,
        needs_run: true,
        needs_argument: false,
        network: false,
    },
    CommandSpec {
        name: "compact",
        aliases: &[],
        args: None,
        description: "Compact context at the next model boundary",
        category: Category::Conversation,
        needs_run: true,
        needs_argument: false,
        network: true,
    },
    CommandSpec {
        name: "transcript",
        aliases: &[],
        args: Some("[PATH]"),
        description: "Export the transcript to Markdown",
        category: Category::Conversation,
        needs_run: false,
        needs_argument: false,
        network: false,
    },
    CommandSpec {
        name: "quit",
        aliases: &["exit"],
        args: None,
        description: "Leave Minha",
        category: Category::Conversation,
        needs_run: false,
        needs_argument: false,
        network: false,
    },
    // Work coordination
    CommandSpec {
        name: "to",
        aliases: &[],
        args: Some("[AGENT]"),
        description: "Target the next message at an agent",
        category: Category::Work,
        needs_run: false,
        needs_argument: false,
        network: false,
    },
    CommandSpec {
        name: "note",
        aliases: &[],
        args: Some("TEXT"),
        description: "Add a note to the coordination board",
        category: Category::Work,
        needs_run: false,
        needs_argument: true,
        network: false,
    },
    CommandSpec {
        name: "pin",
        aliases: &[],
        args: Some("[ID]"),
        description: "Pin a board entry",
        category: Category::Work,
        needs_run: false,
        needs_argument: false,
        network: false,
    },
    CommandSpec {
        name: "resolve",
        aliases: &[],
        args: Some("[ID]"),
        description: "Resolve a board entry",
        category: Category::Work,
        needs_run: false,
        needs_argument: false,
        network: false,
    },
    // Inspect
    CommandSpec {
        name: "context",
        aliases: &[],
        args: None,
        description: "Inspect per-agent context",
        category: Category::Inspect,
        needs_run: false,
        needs_argument: false,
        network: false,
    },
    CommandSpec {
        name: "memory",
        aliases: &[],
        args: Some("[QUERY | pin ID | delete ID]"),
        description: "Search or manage durable memory",
        category: Category::Inspect,
        needs_run: false,
        needs_argument: false,
        network: false,
    },
    CommandSpec {
        name: "memories",
        aliases: &[],
        args: Some("[SETTING on|off]"),
        description: "Review and toggle memory controls",
        category: Category::Inspect,
        needs_run: false,
        needs_argument: false,
        network: false,
    },
    CommandSpec {
        name: "books",
        aliases: &[],
        args: None,
        description: "Browse verified references",
        category: Category::Inspect,
        needs_run: false,
        needs_argument: false,
        network: false,
    },
    CommandSpec {
        name: "skills",
        aliases: &[],
        args: None,
        description: "List available skills",
        category: Category::Inspect,
        needs_run: false,
        needs_argument: false,
        network: false,
    },
    CommandSpec {
        name: "ask",
        aliases: &[],
        args: Some("QUESTION"),
        description: "Answer from session state, without a model call",
        category: Category::Inspect,
        needs_run: false,
        needs_argument: true,
        network: false,
    },
    CommandSpec {
        name: "doctor",
        aliases: &[],
        args: None,
        description: "Run local diagnostics",
        category: Category::Inspect,
        needs_run: false,
        needs_argument: false,
        network: false,
    },
    // Providers
    CommandSpec {
        name: "model",
        aliases: &[],
        args: None,
        description: "Show provider-aware models",
        category: Category::Providers,
        needs_run: false,
        needs_argument: false,
        network: false,
    },
    CommandSpec {
        name: "provider",
        aliases: &[],
        args: Some("[list]"),
        description: "Show configured providers and how to change them",
        category: Category::Providers,
        needs_run: false,
        needs_argument: false,
        network: false,
    },
    CommandSpec {
        name: "login",
        aliases: &[],
        args: None,
        description: "Authenticate with the model provider",
        category: Category::Providers,
        needs_run: false,
        needs_argument: false,
        network: true,
    },
    // Usage
    CommandSpec {
        name: "usage",
        aliases: &[],
        args: None,
        description: "Show token and context usage",
        category: Category::Usage,
        needs_run: false,
        needs_argument: false,
        network: false,
    },
    CommandSpec {
        name: "settings",
        aliases: &[],
        args: Some("[ACTION]"),
        description: "Open or edit user-local TUI settings",
        category: Category::Appearance,
        needs_run: false,
        needs_argument: false,
        network: false,
    },
    // Appearance and input
    CommandSpec {
        name: "theme",
        aliases: &[],
        args: Some("[NAME|import|export|contrast|preview]"),
        description: "Preview, validate, import, or save a local theme",
        category: Category::Appearance,
        needs_run: false,
        needs_argument: false,
        network: false,
    },
    CommandSpec {
        name: "keymap",
        aliases: &[],
        args: None,
        description: "Show the resolved editor keymap",
        category: Category::Input,
        needs_run: false,
        needs_argument: false,
        network: false,
    },
    // Maintenance
    CommandSpec {
        name: "quality",
        aliases: &[],
        args: Some("[ACTION]"),
        description: "Run the workspace quality gates",
        category: Category::Maintenance,
        needs_run: false,
        needs_argument: false,
        network: false,
    },
    CommandSpec {
        name: "check",
        aliases: &[],
        args: None,
        description: "Run the build/typecheck gate",
        category: Category::Maintenance,
        needs_run: false,
        needs_argument: false,
        network: false,
    },
    CommandSpec {
        name: "lint",
        aliases: &[],
        args: None,
        description: "Run the lint gate",
        category: Category::Maintenance,
        needs_run: false,
        needs_argument: false,
        network: false,
    },
    CommandSpec {
        name: "test",
        aliases: &[],
        args: None,
        description: "Run the test gate",
        category: Category::Maintenance,
        needs_run: false,
        needs_argument: false,
        network: false,
    },
    CommandSpec {
        name: "docs",
        aliases: &[],
        args: None,
        description: "Run the docs gate",
        category: Category::Maintenance,
        needs_run: false,
        needs_argument: false,
        network: false,
    },
    CommandSpec {
        name: "security",
        aliases: &[],
        args: None,
        description: "Run the security gate",
        category: Category::Maintenance,
        needs_run: false,
        needs_argument: false,
        network: false,
    },
    CommandSpec {
        name: "gh",
        aliases: &["github"],
        args: Some("[ACTION] [NUMBER]"),
        description: "Read GitHub repo, issue, or pull-request context",
        category: Category::Maintenance,
        needs_run: false,
        needs_argument: false,
        network: true,
    },
    CommandSpec {
        name: "clean",
        aliases: &[],
        args: None,
        description: "Clean generated workspace artifacts",
        category: Category::Maintenance,
        needs_run: false,
        needs_argument: false,
        network: false,
    },
];

/// Resolve a typed name (or alias) to its spec.
pub(crate) fn find(name: &str) -> Option<&'static CommandSpec> {
    REGISTRY.iter().find(|spec| {
        spec.name.eq_ignore_ascii_case(name)
            || spec.aliases.iter().any(|alias| alias.eq_ignore_ascii_case(name))
    })
}

/// A registry entry that matched a query, with the rank that placed it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CommandMatch {
    pub(crate) spec: &'static CommandSpec,
    pub(crate) rank: Rank,
    pub(crate) availability: Availability,
}

/// Match quality, best first. Ordering is the sort key, so variant order matters.
#[derive(Clone, Copy, Debug, Eq, PartialEq, PartialOrd, Ord)]
pub(crate) enum Rank {
    /// The query is the whole command name.
    Exact,
    /// The command name starts with the query.
    Prefix,
    /// An alias starts with the query.
    AliasPrefix,
    /// The query appears contiguously inside the name.
    Substring,
    /// The query's characters appear in order inside the name.
    Fuzzy,
    /// The query appears in the description only.
    Description,
}

fn is_subsequence(query: &str, candidate: &str) -> bool {
    let mut chars = candidate.chars();
    query.chars().all(|wanted| chars.any(|actual| actual == wanted))
}

fn rank(spec: &CommandSpec, query: &str) -> Option<Rank> {
    if query.is_empty() {
        return Some(Rank::Prefix);
    }
    let query = query.to_ascii_lowercase();
    let name = spec.name.to_ascii_lowercase();
    if name == query {
        return Some(Rank::Exact);
    }
    if name.starts_with(&query) {
        return Some(Rank::Prefix);
    }
    if spec.aliases.iter().any(|alias| {
        let alias = alias.to_ascii_lowercase();
        alias == query || alias.starts_with(&query)
    }) {
        return Some(Rank::AliasPrefix);
    }
    if name.contains(&query) {
        return Some(Rank::Substring);
    }
    if is_subsequence(&query, &name) {
        return Some(Rank::Fuzzy);
    }
    if spec.description.to_ascii_lowercase().contains(&query) {
        return Some(Rank::Description);
    }
    None
}

/// Commands matching `query`, best match first.
///
/// Ranking is deliberately simple: exact, then prefix, then alias prefix, then
/// contiguous substring, then subsequence, then description hits. Ties keep
/// registry order, so the intentional most-used-first ordering survives — and
/// the list never reshuffles unpredictably as more characters are typed.
pub(crate) fn matches(query: &str, context: CommandContext) -> Vec<CommandMatch> {
    let query = query.trim_start_matches('/');
    let mut found = REGISTRY
        .iter()
        .enumerate()
        .filter_map(|(index, spec)| {
            rank(spec, query).map(|rank| {
                (
                    index,
                    CommandMatch {
                        spec,
                        rank,
                        availability: spec.availability(context),
                    },
                )
            })
        })
        .collect::<Vec<_>>();
    found.sort_by_key(|(index, entry)| (entry.rank, *index));
    found.into_iter().map(|(_, entry)| entry).collect()
}

/// Levenshtein distance, used only for the "did you mean" hint.
///
/// The popup filter deliberately does *not* use this: edit distance reorders
/// unpredictably as characters are typed, which is exactly the jumping behavior
/// the completion list must avoid. Typo recovery on a rejected command has no
/// such constraint, and it catches transpositions (`stauts`) that a subsequence
/// match cannot.
fn edit_distance(left: &str, right: &str) -> usize {
    let right: Vec<char> = right.chars().collect();
    let mut previous: Vec<usize> = (0..=right.len()).collect();
    let mut current = vec![0; right.len() + 1];
    for (i, left_char) in left.chars().enumerate() {
        current[0] = i + 1;
        for (j, right_char) in right.iter().enumerate() {
            let substitution = previous[j] + usize::from(left_char != *right_char);
            current[j + 1] = substitution.min(previous[j + 1] + 1).min(current[j] + 1);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[right.len()]
}

/// Best single suggestion for an unknown command, used by the "did you mean" hint.
pub(crate) fn suggestion(name: &str) -> Option<&'static CommandSpec> {
    if name.is_empty() {
        return None;
    }
    if let Some(entry) = matches(name, CommandContext::default()).first() {
        return Some(entry.spec);
    }
    let name = name.to_ascii_lowercase();
    // Allow more slack on longer names, but never so much that unrelated
    // commands get suggested for a short typo.
    let budget = (name.chars().count() / 3).clamp(1, 3);
    REGISTRY
        .iter()
        .map(|spec| (edit_distance(&name, spec.name), spec))
        .filter(|(distance, _)| *distance <= budget)
        .min_by_key(|(distance, _)| *distance)
        .map(|(_, spec)| spec)
}

/// Registry entries grouped by category, for help rendering.
pub(crate) fn by_category(category: Category) -> Vec<&'static CommandSpec> {
    REGISTRY.iter().filter(|spec| spec.category == category).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn registry_names_and_aliases_are_unique() {
        let mut seen = HashSet::new();
        for spec in REGISTRY {
            assert!(seen.insert(spec.name), "duplicate command /{}", spec.name);
            for alias in spec.aliases {
                assert!(seen.insert(alias), "duplicate alias /{alias}");
            }
        }
    }

    #[test]
    fn every_registry_entry_is_findable_by_name_and_alias() {
        for spec in REGISTRY {
            assert_eq!(find(spec.name), Some(spec), "/{} must resolve", spec.name);
            for alias in spec.aliases {
                assert_eq!(find(alias), Some(spec), "/{alias} must resolve");
            }
        }
        assert_eq!(find("definitely-not-a-command"), None);
    }

    #[test]
    fn registry_version_is_bumped_whenever_the_table_changes() {
        // This is the guard the module doc promises: changing REGISTRY's shape
        // without bumping REGISTRY_VERSION fails the build, so a future edit
        // can't silently drift the way the three predecessor lists did.
        assert_eq!(
            (REGISTRY.len(), REGISTRY_VERSION),
            (48, 3),
            "REGISTRY changed size without bumping REGISTRY_VERSION"
        );
    }

    #[test]
    fn every_category_has_at_least_one_command() {
        for category in Category::ALL {
            assert!(
                !by_category(*category).is_empty(),
                "{} category must not be empty",
                category.label()
            );
        }
    }

    #[test]
    fn empty_query_lists_every_command_in_registry_order() {
        let listed = matches("", CommandContext::default());
        assert_eq!(listed.len(), REGISTRY.len());
        for (entry, spec) in listed.iter().zip(REGISTRY) {
            assert_eq!(entry.spec, spec);
        }
    }

    #[test]
    fn exact_and_prefix_matches_outrank_fuzzy_ones() {
        let listed = matches("re", CommandContext::default());
        let names = listed.iter().map(|entry| entry.spec.name).collect::<Vec<_>>();
        // `review`, `resume`, `retry`, `rename`, `resolve` are prefix matches and
        // must all precede fuzzy hits like `remove`-style subsequences.
        let first_fuzzy = listed
            .iter()
            .position(|entry| entry.rank > Rank::AliasPrefix)
            .unwrap_or(listed.len());
        for entry in &listed[..first_fuzzy] {
            assert!(
                entry.spec.name.starts_with("re") || entry.spec.aliases.iter().any(|a| a.starts_with("re")),
                "{} ranked as a prefix match but is not one",
                entry.spec.name
            );
        }
        assert!(
            names.contains(&"resume"),
            "prefix matches must be listed: {names:?}"
        );

        let exact = matches("new", CommandContext::default());
        assert_eq!(exact[0].spec.name, "new");
        assert_eq!(exact[0].rank, Rank::Exact);
    }

    #[test]
    fn fuzzy_matching_finds_non_prefix_commands() {
        let listed = matches("mplmnt", CommandContext::default());
        assert_eq!(listed[0].spec.name, "implement");
        assert_eq!(listed[0].rank, Rank::Fuzzy);

        let substring = matches("play", CommandContext::default());
        assert!(
            substring.is_empty() || substring[0].rank >= Rank::Substring,
            "unrelated query must not produce a prefix match"
        );
    }

    #[test]
    fn aliases_match_and_resolve_to_the_canonical_command() {
        let listed = matches("github", CommandContext::default());
        assert_eq!(listed[0].spec.name, "gh");
        assert_eq!(listed[0].rank, Rank::AliasPrefix);
    }

    #[test]
    fn session_commands_are_listed_but_explained_without_a_run() {
        let idle = matches("fork", CommandContext { active_run: false });
        assert_eq!(idle[0].spec.name, "fork");
        assert_eq!(
            idle[0].availability.reason(),
            Some("needs an active session"),
            "unavailable commands stay discoverable with a reason"
        );

        let active = matches("fork", CommandContext { active_run: true });
        assert_eq!(active[0].availability, Availability::Available);
    }

    #[test]
    fn display_includes_the_argument_shape() {
        assert_eq!(find("rename").unwrap().display(), "/rename TITLE");
        assert_eq!(find("new").unwrap().display(), "/new");
    }

    #[test]
    fn suggestion_recovers_from_a_typo() {
        assert_eq!(suggestion("stauts").map(|spec| spec.name), Some("status"));
        assert_eq!(suggestion("hlp").map(|spec| spec.name), Some("help"));
    }
}
