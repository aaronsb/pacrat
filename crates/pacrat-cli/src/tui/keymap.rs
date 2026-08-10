//! Keys to actions — one table, read by both the dispatcher and the help
//! overlay.
//!
//! Two representations of a keymap (a `match` for behaviour, a list of
//! strings for `?`) drift apart the first time somebody adds a binding, and
//! the drift is invisible: the help lies and nothing fails. So each row here
//! carries its own matcher, [`action_for`] walks the rows in order, and the
//! overlay renders the same rows. A binding that is not in the table does
//! not work, and one that is in the table is documented.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::viewport::Scroll;
use super::Tab;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Quit,
    ToggleHelp,
    Screen(Tab),
    /// Move focus to the next region (`true`) or the previous one.
    Focus(bool),
    Scroll(Scroll),
    /// Re-run whatever this screen asked the system, showing it happening.
    Reload,
}

pub struct Binding {
    /// How the keys are written in the help overlay.
    pub keys: &'static str,
    pub what: &'static str,
    /// The matcher *and* the action: a row that can answer for a key
    /// returns what pressing it does.
    pub of: fn(KeyEvent) -> Option<Action>,
}

/// In order. The first row that answers wins, so the modified keys come
/// before the plain ones they share a letter with.
pub const BINDINGS: &[Binding] = &[
    Binding {
        keys: "1-6",
        what: "screen",
        of: |key| match key.code {
            KeyCode::Char(c) if c.is_ascii_digit() && !modified(key) => {
                Tab::from_digit(c).map(Action::Screen)
            }
            _ => None,
        },
    },
    Binding {
        keys: "ctrl-d / ctrl-u",
        what: "half page",
        of: |key| {
            if ctrl(key, 'd') {
                Some(Action::Scroll(Scroll::HalfPageDown))
            } else if ctrl(key, 'u') {
                Some(Action::Scroll(Scroll::HalfPageUp))
            } else {
                None
            }
        },
    },
    Binding {
        keys: "ctrl-f / ctrl-b · pgdn / pgup",
        what: "page",
        of: |key| match key.code {
            KeyCode::PageDown => Some(Action::Scroll(Scroll::PageDown)),
            KeyCode::PageUp => Some(Action::Scroll(Scroll::PageUp)),
            _ if ctrl(key, 'f') => Some(Action::Scroll(Scroll::PageDown)),
            _ if ctrl(key, 'b') => Some(Action::Scroll(Scroll::PageUp)),
            _ => None,
        },
    },
    Binding {
        keys: "j / k · ↓ / ↑",
        what: "line",
        of: |key| match key.code {
            KeyCode::Down => Some(Action::Scroll(Scroll::LineDown)),
            KeyCode::Up => Some(Action::Scroll(Scroll::LineUp)),
            KeyCode::Char('j') if !modified(key) => Some(Action::Scroll(Scroll::LineDown)),
            KeyCode::Char('k') if !modified(key) => Some(Action::Scroll(Scroll::LineUp)),
            _ => None,
        },
    },
    Binding {
        keys: "g / G",
        what: "top / bottom",
        of: |key| match key.code {
            KeyCode::Char('g') if !modified(key) => Some(Action::Scroll(Scroll::Top)),
            KeyCode::Char('G') if !modified(key) => Some(Action::Scroll(Scroll::Bottom)),
            _ => None,
        },
    },
    Binding {
        keys: "tab / shift-tab",
        what: "move focus — the focused region scrolls",
        of: |key| match key.code {
            KeyCode::Tab if !modified(key) => Some(Action::Focus(true)),
            KeyCode::BackTab => Some(Action::Focus(false)),
            _ => None,
        },
    },
    Binding {
        keys: "r",
        what: "re-run this screen's queries",
        of: |key| (key.code == KeyCode::Char('r') && !modified(key)).then_some(Action::Reload),
    },
    Binding {
        keys: "?",
        what: "this help",
        of: |key| (key.code == KeyCode::Char('?')).then_some(Action::ToggleHelp),
    },
    Binding {
        keys: "q / esc",
        what: "quit",
        of: |key| {
            let quit = matches!(key.code, KeyCode::Esc)
                || (matches!(key.code, KeyCode::Char('q')) && !modified(key));
            quit.then_some(Action::Quit)
        },
    },
];

pub fn action_for(key: KeyEvent) -> Option<Action> {
    BINDINGS.iter().find_map(|binding| (binding.of)(key))
}

/// Shift is not a modifier for this purpose — it is how `G` is typed. The
/// others change what a key means, so a row that wants a bare letter has to
/// say it does not want them.
fn modified(key: KeyEvent) -> bool {
    key.modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
}

fn ctrl(key: KeyEvent, wanted: char) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char(c) if c.eq_ignore_ascii_case(&wanted))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(code: KeyCode) -> Option<Action> {
        action_for(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn with_ctrl(c: char) -> Option<Action> {
        action_for(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL))
    }

    #[test]
    fn the_number_keys_reach_every_screen_and_nothing_else() {
        for (digit, tab) in [
            ('1', Tab::Overview),
            ('2', Tab::Browse),
            ('3', Tab::Updates),
            ('4', Tab::Hosts),
            ('5', Tab::Jobs),
            ('6', Tab::Config),
        ] {
            assert_eq!(press(KeyCode::Char(digit)), Some(Action::Screen(tab)));
        }
        // There is no seventh screen; `?` is where the about tab will live.
        assert_eq!(press(KeyCode::Char('7')), None);
        assert_eq!(press(KeyCode::Char('0')), None);
    }

    #[test]
    fn the_scroll_vocabulary_is_the_mockups() {
        assert_eq!(
            press(KeyCode::Char('j')),
            Some(Action::Scroll(Scroll::LineDown))
        );
        assert_eq!(press(KeyCode::Up), Some(Action::Scroll(Scroll::LineUp)));
        assert_eq!(with_ctrl('d'), Some(Action::Scroll(Scroll::HalfPageDown)));
        assert_eq!(with_ctrl('u'), Some(Action::Scroll(Scroll::HalfPageUp)));
        assert_eq!(with_ctrl('f'), Some(Action::Scroll(Scroll::PageDown)));
        assert_eq!(with_ctrl('b'), Some(Action::Scroll(Scroll::PageUp)));
        assert_eq!(
            press(KeyCode::PageDown),
            Some(Action::Scroll(Scroll::PageDown))
        );
        assert_eq!(press(KeyCode::Char('g')), Some(Action::Scroll(Scroll::Top)));
        assert_eq!(
            press(KeyCode::Char('G')),
            Some(Action::Scroll(Scroll::Bottom))
        );
    }

    /// The reason plain-letter rows check their modifiers: `ctrl-b` is a
    /// page, and a `b` row that ignored modifiers would swallow it. Nothing
    /// here is bound to a bare letter that also has a ctrl meaning, and this
    /// test is what keeps that true when somebody adds one.
    #[test]
    fn a_control_chord_never_falls_through_to_the_bare_letter() {
        for c in ['j', 'k', 'g', 'q', 'r', '1'] {
            let chord = with_ctrl(c);
            let bare = press(KeyCode::Char(c));
            assert!(
                chord.is_none() || chord != bare,
                "ctrl-{c} was handled as if it were {c}"
            );
        }
    }

    #[test]
    fn focus_quit_and_help() {
        assert_eq!(press(KeyCode::Tab), Some(Action::Focus(true)));
        assert_eq!(press(KeyCode::BackTab), Some(Action::Focus(false)));
        assert_eq!(press(KeyCode::Char('r')), Some(Action::Reload));
        assert_eq!(press(KeyCode::Char('?')), Some(Action::ToggleHelp));
        assert_eq!(press(KeyCode::Char('q')), Some(Action::Quit));
        assert_eq!(press(KeyCode::Esc), Some(Action::Quit));
    }

    #[test]
    fn an_unbound_key_does_nothing_at_all() {
        for code in [KeyCode::Char('z'), KeyCode::F(4), KeyCode::Insert] {
            assert_eq!(press(code), None);
        }
    }

    /// The overlay renders these rows verbatim, so an empty one is a blank
    /// line in the help rather than a compile error.
    #[test]
    fn every_documented_row_says_what_it_does() {
        for binding in BINDINGS {
            assert!(!binding.keys.is_empty(), "a binding has no keys");
            assert!(
                !binding.what.is_empty(),
                "{} has no description",
                binding.keys
            );
        }
    }
}
