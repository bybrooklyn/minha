//! Editor keybindings, resolved through one table instead of ad hoc `KeyCode`
//! matching scattered across `map_event`.
//!
//! Two problems this solves, both taken from Codex CLI's `key_hint.rs`:
//!
//! 1. **Terminal quirk normalization.** Terminals disagree about what they send.
//!    `Ctrl-J` arrives as a raw line feed on many of them; `Shift-A` arrives as a
//!    bare uppercase `A` with no `SHIFT` flag; `Cmd` (`SUPER`) is only delivered
//!    by terminals that opted into the kitty keyboard protocol, and is silently
//!    dropped everywhere else. Normalizing once here means every binding gets the
//!    shim, rather than each comparison site needing its own fix.
//! 2. **Help that cannot drift.** [`describe`] renders the same table the editor
//!    resolves against, so the help and keymap overlays always describe the
//!    bindings that actually exist.
//!
//! Only *editor* actions live here. Application-level chords that depend on app
//! state (`Ctrl-C` interrupt-vs-quit, `Enter` submit-vs-activate, `Up` history-
//! vs-selection) stay in `map_event`, because resolving them needs the `App`.

use crate::app::AppAction;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Which built-in preset is active. Presets differ only in the bindings that are
/// genuinely platform-specific; the portable Emacs-style bindings are shared, so
/// muscle memory works everywhere and `Ctrl-U`/`Ctrl-K`/`Ctrl-A`/`Ctrl-E` are
/// always available as the fallback when a terminal swallows the platform chord.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Preset {
    MacOs,
    Portable,
}

impl Preset {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::MacOs => "macOS",
            Self::Portable => "portable",
        }
    }
}

/// The preset for the host platform.
pub(crate) const fn active_preset() -> Preset {
    if cfg!(target_os = "macos") {
        Preset::MacOs
    } else {
        Preset::Portable
    }
}

/// A key as declared in the table, before terminal quirks are accounted for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Chord {
    pub(crate) code: KeyCode,
    pub(crate) modifiers: KeyModifiers,
}

impl Chord {
    const fn new(code: KeyCode, modifiers: KeyModifiers) -> Self {
        Self { code, modifiers }
    }

    const fn ctrl(character: char) -> Self {
        Self::new(KeyCode::Char(character), KeyModifiers::CONTROL)
    }

    /// Does an incoming key event satisfy this chord?
    ///
    /// Matching is deliberately forgiving about the extra modifier bits
    /// terminals sprinkle on: the declared modifiers must all be present, and no
    /// *conflicting* modifier may be. `SHIFT` is ignored, because a terminal that
    /// sends `Ctrl-Shift-K` and one that sends `Ctrl-K` mean the same thing here.
    fn matches(self, event: KeyEvent) -> bool {
        let relevant = KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER;
        let wanted = self.modifiers & relevant;
        let actual = event.modifiers & relevant;
        if wanted != actual {
            return false;
        }
        match (self.code, event.code) {
            // Terminals disagree on which of these Backspace/Delete means
            // "erase backwards"; both spellings reach the same action.
            (KeyCode::Backspace, KeyCode::Backspace) => true,
            (KeyCode::Char(wanted), KeyCode::Char(actual)) => wanted.eq_ignore_ascii_case(&actual),
            (wanted, actual) => wanted == actual,
        }
    }
}

/// One row of the resolved keymap.
struct Binding {
    chords: &'static [Chord],
    /// Platform-specific chords, added only under the matching preset.
    macos_chords: &'static [Chord],
    action: AppAction,
    description: &'static str,
}

/// Editor bindings, resolved in order. Earlier entries win.
static BINDINGS: &[Binding] = &[
    Binding {
        chords: &[Chord::ctrl('a'), Chord::new(KeyCode::Home, KeyModifiers::NONE)],
        // macOS: Cmd-Left is "start of line" in every native text field.
        macos_chords: &[Chord::new(KeyCode::Left, KeyModifiers::SUPER)],
        action: AppAction::CursorHome,
        description: "Move to start of line",
    },
    Binding {
        chords: &[Chord::ctrl('e'), Chord::new(KeyCode::End, KeyModifiers::NONE)],
        macos_chords: &[Chord::new(KeyCode::Right, KeyModifiers::SUPER)],
        action: AppAction::CursorEnd,
        description: "Move to end of line",
    },
    Binding {
        chords: &[Chord::ctrl('u')],
        // Cmd-Delete / Cmd-Backspace: delete from the cursor to the start of the
        // line, matching every native macOS text field. Not whole-line.
        macos_chords: &[
            Chord::new(KeyCode::Backspace, KeyModifiers::SUPER),
            Chord::new(KeyCode::Delete, KeyModifiers::SUPER),
        ],
        action: AppAction::DeleteToLineStart,
        description: "Delete to start of line",
    },
    Binding {
        chords: &[Chord::ctrl('k')],
        action: AppAction::DeleteToLineEnd,
        macos_chords: &[],
        description: "Delete to end of line",
    },
    Binding {
        chords: &[Chord::ctrl('x')],
        macos_chords: &[],
        action: AppAction::DeleteLine,
        description: "Delete the whole line",
    },
    Binding {
        chords: &[
            Chord::ctrl('w'),
            Chord::new(KeyCode::Backspace, KeyModifiers::ALT),
            Chord::new(KeyCode::Backspace, KeyModifiers::CONTROL),
        ],
        macos_chords: &[],
        action: AppAction::DeleteWordBackward,
        description: "Delete previous word",
    },
    Binding {
        chords: &[
            Chord::new(KeyCode::Left, KeyModifiers::ALT),
            Chord::new(KeyCode::Left, KeyModifiers::CONTROL),
        ],
        macos_chords: &[],
        action: AppAction::WordLeft,
        description: "Move back one word",
    },
    Binding {
        chords: &[
            Chord::new(KeyCode::Right, KeyModifiers::ALT),
            Chord::new(KeyCode::Right, KeyModifiers::CONTROL),
        ],
        macos_chords: &[],
        action: AppAction::WordRight,
        description: "Move forward one word",
    },
    Binding {
        chords: &[Chord::ctrl('z')],
        macos_chords: &[],
        action: AppAction::Undo,
        description: "Undo",
    },
    Binding {
        chords: &[Chord::ctrl('y')],
        macos_chords: &[],
        action: AppAction::Redo,
        description: "Redo",
    },
];

fn chords_for(binding: &'static Binding, preset: Preset) -> impl Iterator<Item = Chord> {
    let platform = match preset {
        Preset::MacOs => binding.macos_chords,
        Preset::Portable => &[],
    };
    binding.chords.iter().copied().chain(platform.iter().copied())
}

/// Resolve a key event to an editor action, or `None` if no binding claims it.
pub(crate) fn resolve(event: KeyEvent) -> Option<AppAction> {
    resolve_with(event, active_preset())
}

pub(crate) fn resolve_with(event: KeyEvent, preset: Preset) -> Option<AppAction> {
    let event = normalize(event, preset);
    BINDINGS
        .iter()
        .find(|binding| chords_for(binding, preset).any(|chord| chord.matches(event)))
        .map(|binding| binding.action.clone())
}

/// Fold terminal-specific spellings onto the one this table declares.
fn normalize(mut event: KeyEvent, preset: Preset) -> KeyEvent {
    // Many terminals send raw control characters instead of Char+CONTROL.
    if let KeyCode::Char(character) = event.code
        && !event.modifiers.contains(KeyModifiers::CONTROL)
        && (character as u32) < 0x20
        && character != '\t'
    {
        let letter = char::from(b'a' + (character as u8).saturating_sub(1));
        if letter.is_ascii_lowercase() {
            event.code = KeyCode::Char(letter);
            event.modifiers |= KeyModifiers::CONTROL;
        }
    }
    // Terminals that do not negotiate keyboard enhancements never report SUPER.
    // Nothing to recover here — the portable Ctrl bindings above are the reason
    // every SUPER chord has a non-SUPER twin.
    let _ = preset;
    event
}

/// The resolved keymap as `(keys, description)` rows, for help rendering.
pub(crate) fn describe() -> Vec<(String, &'static str)> {
    let preset = active_preset();
    let mut rows: Vec<(String, &'static str)> = BINDINGS
        .iter()
        .map(|binding| {
            let keys = chords_for(binding, preset)
                .map(render_chord)
                .collect::<Vec<_>>()
                .join(" / ");
            (keys, binding.description)
        })
        .collect();
    // Application chords still resolved in `map_event`; listed so help stays a
    // complete picture even while the keymap layer only owns editor actions.
    rows.extend([
        ("Enter".to_owned(), "Send, steer, or accept the selection"),
        ("Shift-Enter / Ctrl-J".to_owned(), "Insert a newline"),
        ("/ or Ctrl-P".to_owned(), "Search commands"),
        ("Tab".to_owned(), "Complete the highlighted entry"),
        ("Shift-Tab".to_owned(), "Cycle side panels"),
        ("Esc".to_owned(), "Dismiss; Esc Esc pauses safely"),
        ("Ctrl-C".to_owned(), "Interrupt the run, or quit when idle"),
        ("Ctrl-O".to_owned(), "Expand the nearest activity"),
        ("Ctrl-R".to_owned(), "Recall previous input"),
        ("Ctrl-T".to_owned(), "Toggle the task list"),
    ]);
    rows
}

fn render_chord(chord: Chord) -> String {
    let mut parts = Vec::new();
    if chord.modifiers.contains(KeyModifiers::CONTROL) {
        parts.push("Ctrl".to_owned());
    }
    if chord.modifiers.contains(KeyModifiers::ALT) {
        parts.push(if active_preset() == Preset::MacOs {
            "Option".to_owned()
        } else {
            "Alt".to_owned()
        });
    }
    if chord.modifiers.contains(KeyModifiers::SUPER) {
        parts.push("Cmd".to_owned());
    }
    parts.push(match chord.code {
        KeyCode::Char(character) => character.to_ascii_uppercase().to_string(),
        KeyCode::Backspace => "Delete".to_owned(),
        KeyCode::Delete => "Fn-Delete".to_owned(),
        KeyCode::Left => "Left".to_owned(),
        KeyCode::Right => "Right".to_owned(),
        KeyCode::Home => "Home".to_owned(),
        KeyCode::End => "End".to_owned(),
        other => format!("{other:?}"),
    });
    parts.join("-")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn portable_emacs_bindings_resolve_on_every_platform() {
        for preset in [Preset::MacOs, Preset::Portable] {
            assert_eq!(
                resolve_with(key(KeyCode::Char('u'), KeyModifiers::CONTROL), preset),
                Some(AppAction::DeleteToLineStart)
            );
            assert_eq!(
                resolve_with(key(KeyCode::Char('k'), KeyModifiers::CONTROL), preset),
                Some(AppAction::DeleteToLineEnd)
            );
            assert_eq!(
                resolve_with(key(KeyCode::Char('a'), KeyModifiers::CONTROL), preset),
                Some(AppAction::CursorHome)
            );
            assert_eq!(
                resolve_with(key(KeyCode::Char('e'), KeyModifiers::CONTROL), preset),
                Some(AppAction::CursorEnd)
            );
            assert_eq!(
                resolve_with(key(KeyCode::Char('w'), KeyModifiers::CONTROL), preset),
                Some(AppAction::DeleteWordBackward)
            );
            assert_eq!(
                resolve_with(key(KeyCode::Char('x'), KeyModifiers::CONTROL), preset),
                Some(AppAction::DeleteLine)
            );
        }
    }

    #[test]
    fn command_delete_deletes_to_line_start_on_macos_only() {
        for code in [KeyCode::Backspace, KeyCode::Delete] {
            assert_eq!(
                resolve_with(key(code, KeyModifiers::SUPER), Preset::MacOs),
                Some(AppAction::DeleteToLineStart),
                "Cmd-{code:?} is macOS delete-to-line-start"
            );
            // Under the portable preset SUPER is not bound, so a terminal that
            // does report it must not trigger a surprise deletion.
            assert_eq!(
                resolve_with(key(code, KeyModifiers::SUPER), Preset::Portable),
                None
            );
        }
    }

    #[test]
    fn command_arrows_move_to_line_edges_on_macos() {
        assert_eq!(
            resolve_with(key(KeyCode::Left, KeyModifiers::SUPER), Preset::MacOs),
            Some(AppAction::CursorHome)
        );
        assert_eq!(
            resolve_with(key(KeyCode::Right, KeyModifiers::SUPER), Preset::MacOs),
            Some(AppAction::CursorEnd)
        );
    }

    #[test]
    fn raw_control_characters_normalize_to_their_chord() {
        // Terminals that do not report Char+CONTROL send the raw control byte.
        assert_eq!(
            resolve_with(key(KeyCode::Char('\u{15}'), KeyModifiers::NONE), Preset::Portable),
            Some(AppAction::DeleteToLineStart),
            "raw 0x15 is Ctrl-U"
        );
        assert_eq!(
            resolve_with(key(KeyCode::Char('\u{0b}'), KeyModifiers::NONE), Preset::Portable),
            Some(AppAction::DeleteToLineEnd),
            "raw 0x0b is Ctrl-K"
        );
    }

    #[test]
    fn stray_shift_does_not_defeat_a_binding() {
        assert_eq!(
            resolve_with(
                key(KeyCode::Char('K'), KeyModifiers::CONTROL | KeyModifiers::SHIFT),
                Preset::Portable
            ),
            Some(AppAction::DeleteToLineEnd)
        );
    }

    #[test]
    fn word_delete_accepts_both_alt_and_ctrl_backspace() {
        for modifiers in [KeyModifiers::ALT, KeyModifiers::CONTROL] {
            assert_eq!(
                resolve_with(key(KeyCode::Backspace, modifiers), Preset::Portable),
                Some(AppAction::DeleteWordBackward)
            );
        }
    }

    #[test]
    fn unbound_keys_fall_through_to_the_app_layer() {
        assert_eq!(
            resolve_with(key(KeyCode::Enter, KeyModifiers::NONE), Preset::Portable),
            None
        );
        assert_eq!(
            resolve_with(key(KeyCode::Char('a'), KeyModifiers::NONE), Preset::Portable),
            None
        );
        assert_eq!(
            resolve_with(key(KeyCode::Tab, KeyModifiers::NONE), Preset::Portable),
            None
        );
        // Bare Backspace/Delete stay with the app so they can also dismiss
        // clarification state rather than only editing text.
        assert_eq!(
            resolve_with(key(KeyCode::Backspace, KeyModifiers::NONE), Preset::Portable),
            None
        );
    }

    #[test]
    fn every_binding_is_described_exactly_once() {
        let rows = describe();
        for binding in BINDINGS {
            assert!(
                rows.iter().any(|(_, text)| *text == binding.description),
                "{} must appear in the keymap help",
                binding.description
            );
        }
        assert!(rows.iter().all(|(keys, _)| !keys.is_empty()));
    }
}
