//! Keys to actions — one table, read by both the dispatcher and the help
//! overlay.
//!
//! Two representations of a keymap (a `match` for behaviour, a list of
//! strings for `?`) drift apart the first time somebody adds a binding, and
//! the drift is invisible: the help lies and nothing fails. So each row here
//! carries its own matcher, [`action_for`] walks the rows in order, and the
//! overlay renders the same rows. A binding that is not in the table does
//! not work, and one that is in the table is documented.
//!
//! ## Screens claim letters
//!
//! The mockup gives each screen its own verbs on bare letters, and some of
//! them are letters the global rows already use: `a` on updates is *adopt*
//! while nothing global answers it, and `x`, `o`, `v`, `t`, `i`, `s`, `p`
//! and `R` are claimed the same way. Twenty-six letters and seven screens
//! leaves no way to avoid collisions that does not end in chords nobody can
//! remember, so the mechanism has to exist whether or not today's table
//! happens to exercise it.
//!
//! (The mockup also asks for `g` regrade on updates and `r` drop on hosts,
//! which *would* shadow the global `g` and `r`. Neither is bound: `pacrat
//! grade` reads the store tree rather than the candidate on screen, so the
//! key would grade the wrong bytes, and dropping a rung has no CLI verb yet.
//! When they land they are one row each, and the scope below is what makes
//! them possible.)
//!
//! So a row carries a [`Scope`], and the screen's rows are consulted before
//! the global ones. Two consequences worth stating, because they are the
//! cost of the choice: a screen that claims a letter *takes* it — the global
//! meaning is unreachable there, not merely shadowed — and the help overlay
//! shows the current screen's rows above the global ones so the reader can
//! see which meaning is in force where they are standing.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::viewport::Scroll;
use super::Tab;

/// A screen's own verbs, from the mockup's footer bars.
///
/// One enum rather than an action per key so the table stays the only place
/// a key is named, and so a screen's `match` is exhaustive: a verb added
/// here and forgotten in a screen is a compile error rather than a key that
/// does nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Local {
    /// browse `/` — type a search term.
    Search,
    /// browse `enter` — everything pacrat knows about the selected package.
    Detail,
    /// browse `v` · `t` · `i` — the three rungs, as the command that climbs
    /// them. The TUI shows it rather than running it; [`super`] says why.
    Vendor,
    Track,
    Install,
    /// updates `enter` — fetch the candidate, diff it, read its gradings.
    Review,
    /// updates `a` — the adopt+build command for the reviewed candidate.
    Adopt,
    /// updates `x` — the reject command for the selected candidate.
    Reject,
    /// updates `o` — reveal the override childlock for a held candidate.
    Override,
    /// hosts `space` — mark the cursor row, or unmark it.
    Select,
    /// hosts `*` · `-` · `!` — the whole-screen selection ops: mark all,
    /// mark none, invert the marks. ADR-002's candidate keys, adopted as-is:
    /// nothing global answers any of the three, and the decision there is
    /// that a future selecting screen claims the same trio — one selection
    /// language, spoken per screen.
    SelectAll,
    SelectNone,
    SelectInvert,
    /// hosts `A` · `s` — adopt the selection · plan this host.
    AdoptSelection,
    Sync,
    /// jobs `p` — probe the AUR write path.
    Probe,
    /// jobs `R` — the command that works the publish queue.
    Retry,
    /// config `←`/`→` — move across the presets.
    PresetLeft,
    PresetRight,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// `q`/`esc` — the mockup's "back/quit": it closes an overlay if one is
    /// open, and leaves only when there is nothing left to back out of.
    Quit,
    /// `ctrl-c` — out, from wherever, no questions.
    ///
    /// Distinct from `Quit` because the two answer different questions. In
    /// raw mode the terminal's ISIG processing is off, so this keystroke is
    /// never a SIGINT: it arrives as an ordinary key and means nothing
    /// unless something here gives it meaning. A reflex key that does
    /// nothing sends the user to `kill`, which is the one exit with no
    /// restoration on it.
    Interrupt,
    ToggleHelp,
    Screen(Tab),
    /// Move focus to the next region (`true`) or the previous one.
    Focus(bool),
    Scroll(Scroll),
    /// Re-run whatever this screen asked the system, showing it happening.
    Reload,
    /// A verb belonging to the screen in front.
    Local(Local),
}

/// Where a binding is in force.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Every screen, unless one has claimed the key.
    Global,
    /// This screen only, and here it wins.
    On(Tab),
}

pub struct Binding {
    /// How the keys are written in the help overlay.
    pub keys: &'static str,
    pub what: &'static str,
    pub scope: Scope,
    /// The matcher *and* the action: a row that can answer for a key
    /// returns what pressing it does.
    pub of: fn(KeyEvent) -> Option<Action>,
}

/// In order. The first row that answers wins, so the modified keys come
/// before the plain ones they share a letter with.
pub const BINDINGS: &[Binding] = &[
    // ---- the screens' own verbs, which outrank the global rows below ----
    Binding {
        keys: "/",
        what: "search the repos and the AUR",
        scope: Scope::On(Tab::Browse),
        of: |key| bare(key, '/').then_some(Action::Local(Local::Search)),
    },
    Binding {
        keys: "enter",
        what: "full detail for the selected package",
        scope: Scope::On(Tab::Browse),
        of: |key| (key.code == KeyCode::Enter).then_some(Action::Local(Local::Detail)),
    },
    Binding {
        keys: "v / t / i",
        what: "vendor · track · install — the command to run",
        scope: Scope::On(Tab::Browse),
        of: |key| match key.code {
            _ if bare(key, 'v') => Some(Action::Local(Local::Vendor)),
            _ if bare(key, 't') => Some(Action::Local(Local::Track)),
            _ if bare(key, 'i') => Some(Action::Local(Local::Install)),
            _ => None,
        },
    },
    Binding {
        keys: "enter",
        what: "review: fetch the candidate, diff it, read its gradings",
        scope: Scope::On(Tab::Updates),
        of: |key| (key.code == KeyCode::Enter).then_some(Action::Local(Local::Review)),
    },
    Binding {
        keys: "a",
        what: "adopt+build — the command to run",
        scope: Scope::On(Tab::Updates),
        of: |key| bare(key, 'a').then_some(Action::Local(Local::Adopt)),
    },
    Binding {
        keys: "x",
        what: "reject this candidate — the command to run",
        scope: Scope::On(Tab::Updates),
        of: |key| bare(key, 'x').then_some(Action::Local(Local::Reject)),
    },
    Binding {
        keys: "o",
        what: "override a held candidate (hold-to-confirm)",
        scope: Scope::On(Tab::Updates),
        of: |key| bare(key, 'o').then_some(Action::Local(Local::Override)),
    },
    Binding {
        keys: "space",
        what: "mark a row",
        scope: Scope::On(Tab::Hosts),
        of: |key| bare(key, ' ').then_some(Action::Local(Local::Select)),
    },
    Binding {
        keys: "* / - / !",
        what: "mark all · none · invert the marks",
        scope: Scope::On(Tab::Hosts),
        of: |key| match key.code {
            _ if bare(key, '*') => Some(Action::Local(Local::SelectAll)),
            _ if bare(key, '-') => Some(Action::Local(Local::SelectNone)),
            _ if bare(key, '!') => Some(Action::Local(Local::SelectInvert)),
            _ => None,
        },
    },
    Binding {
        keys: "A / s",
        what: "adopt the selection · sync this host — the command to run",
        scope: Scope::On(Tab::Hosts),
        of: |key| match key.code {
            _ if bare(key, 'A') => Some(Action::Local(Local::AdoptSelection)),
            _ if bare(key, 's') => Some(Action::Local(Local::Sync)),
            _ => None,
        },
    },
    Binding {
        keys: "p",
        what: "probe the AUR write path",
        scope: Scope::On(Tab::Jobs),
        of: |key| bare(key, 'p').then_some(Action::Local(Local::Probe)),
    },
    Binding {
        keys: "R",
        what: "retry the publish queue — the command to run",
        scope: Scope::On(Tab::Jobs),
        of: |key| bare(key, 'R').then_some(Action::Local(Local::Retry)),
    },
    Binding {
        keys: "← / →",
        what: "move across the flow presets",
        scope: Scope::On(Tab::Config),
        of: |key| match key.code {
            KeyCode::Left => Some(Action::Local(Local::PresetLeft)),
            KeyCode::Right => Some(Action::Local(Local::PresetRight)),
            _ => None,
        },
    },
    // ---- global ----
    Binding {
        keys: "1-7",
        what: "screen",
        scope: Scope::Global,
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
        scope: Scope::Global,
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
        scope: Scope::Global,
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
        scope: Scope::Global,
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
        scope: Scope::Global,
        of: |key| match key.code {
            KeyCode::Char('g') if !modified(key) => Some(Action::Scroll(Scroll::Top)),
            KeyCode::Char('G') if !modified(key) => Some(Action::Scroll(Scroll::Bottom)),
            _ => None,
        },
    },
    Binding {
        keys: "tab / shift-tab",
        what: "move focus — the focused region scrolls",
        scope: Scope::Global,
        of: |key| match key.code {
            KeyCode::Tab if !modified(key) => Some(Action::Focus(true)),
            KeyCode::BackTab => Some(Action::Focus(false)),
            _ => None,
        },
    },
    Binding {
        keys: "r",
        what: "re-run this screen's queries",
        scope: Scope::Global,
        of: |key| (key.code == KeyCode::Char('r') && !modified(key)).then_some(Action::Reload),
    },
    Binding {
        keys: "?",
        what: "this help",
        scope: Scope::Global,
        of: |key| {
            (matches!(key.code, KeyCode::Char('?')) && !modified(key)).then_some(Action::ToggleHelp)
        },
    },
    Binding {
        keys: "q / esc",
        what: "back, then quit",
        scope: Scope::Global,
        of: |key| {
            let quit = matches!(key.code, KeyCode::Esc)
                || (matches!(key.code, KeyCode::Char('q')) && !modified(key));
            quit.then_some(Action::Quit)
        },
    },
    Binding {
        keys: "ctrl-c",
        what: "quit from anywhere",
        scope: Scope::Global,
        of: |key| ctrl(key, 'c').then_some(Action::Interrupt),
    },
];

/// What this key means on this screen.
///
/// The screen's own rows are asked first and their answer is final. That is
/// what makes `g` mean *regrade* on updates and *top* everywhere else, and
/// it is why [`for_screen`] exists for the overlay: a reader on a screen
/// that has taken a letter has to be able to see that it has.
pub fn action_for(key: KeyEvent, tab: Tab) -> Option<Action> {
    for_screen(tab)
        .find_map(|binding| (binding.of)(key))
        .or_else(|| global().find_map(|binding| (binding.of)(key)))
}

/// The rows this screen has claimed, in table order.
pub fn for_screen(tab: Tab) -> impl Iterator<Item = &'static Binding> {
    BINDINGS.iter().filter(move |b| b.scope == Scope::On(tab))
}

/// The rows that hold everywhere they have not been claimed.
pub fn global() -> impl Iterator<Item = &'static Binding> {
    BINDINGS.iter().filter(|b| b.scope == Scope::Global)
}

/// Shift is not a modifier for this purpose — it is how `G` is typed. The
/// others change what a key means, so a row that wants a bare letter has to
/// say it does not want them.
///
/// All five of the others, `META` and `HYPER` included. Those two are only
/// ever reported by terminals speaking the kitty keyboard protocol, which is
/// exactly why they were missed and exactly why they matter: on such a
/// terminal `meta-o` arrived with the bare `o` row answering for it, which
/// on the updates screen is the key that opens the override. A modifier this
/// code has not heard of should never make a key mean *more* than it does
/// unmodified.
fn modified(key: KeyEvent) -> bool {
    key.modifiers.intersects(
        KeyModifiers::CONTROL
            | KeyModifiers::ALT
            | KeyModifiers::SUPER
            | KeyModifiers::META
            | KeyModifiers::HYPER,
    )
}

/// An unmodified letter, as the mockup's footers write them.
///
/// Shift is not a modifier here for the same reason it is not in
/// [`modified`] — it is how `A` and `R` are typed, and those are two
/// different keys from `a` and `r`, which is precisely why the mockup gives
/// them different jobs.
fn bare(key: KeyEvent, wanted: char) -> bool {
    !modified(key) && key.code == KeyCode::Char(wanted)
}

fn ctrl(key: KeyEvent, wanted: char) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char(c) if c.eq_ignore_ascii_case(&wanted))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The overview claims no letters, so it is where the global table can
    /// be tested as the global table.
    fn press(code: KeyCode) -> Option<Action> {
        on(Tab::Overview, code)
    }

    fn on(tab: Tab, code: KeyCode) -> Option<Action> {
        action_for(KeyEvent::new(code, KeyModifiers::NONE), tab)
    }

    fn with_ctrl(c: char) -> Option<Action> {
        action_for(
            KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL),
            Tab::Overview,
        )
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
            ('7', Tab::About),
        ] {
            assert_eq!(press(KeyCode::Char(digit)), Some(Action::Screen(tab)));
        }
        // There is no eighth screen.
        assert_eq!(press(KeyCode::Char('8')), None);
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
        for c in ['j', 'k', 'g', 'q', 'r', '1', 'c', '?'] {
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

    /// Raw mode turns off the terminal's ISIG processing, so this keystroke
    /// reaches the application instead of becoming a SIGINT. If nothing here
    /// answers it, the user's reflex for "get me out" does nothing at all
    /// and their next move is `kill` — the one exit that runs no
    /// restoration and leaves the terminal wrecked.
    #[test]
    fn ctrl_c_is_bound_because_raw_mode_makes_it_a_key_and_not_a_signal() {
        assert_eq!(with_ctrl('c'), Some(Action::Interrupt));
        // Its own action, not a synonym for `q`: `q` backs out of an
        // overlay first, and ctrl-c must not have to be pressed twice.
        assert_ne!(with_ctrl('c'), press(KeyCode::Char('q')));
        // A bare `c` is not an exit — nothing is bound to it at all.
        assert_eq!(press(KeyCode::Char('c')), None);
    }

    /// **Every** modifier makes a key not-bare, including the two only a
    /// kitty-protocol terminal ever reports.
    ///
    /// `META` and `HYPER` were missing, and the consequence was not academic:
    /// on such a terminal `meta-o` matched the bare `o` row, which on the
    /// updates screen is the key that opens the override. A chord nobody
    /// bound must never mean *more* than the letter in it.
    #[test]
    fn an_unbound_modifier_never_makes_a_key_mean_more() {
        for modifier in [
            KeyModifiers::CONTROL,
            KeyModifiers::ALT,
            KeyModifiers::SUPER,
            KeyModifiers::META,
            KeyModifiers::HYPER,
        ] {
            for (tab, c) in [
                (Tab::Updates, 'o'),
                (Tab::Updates, 'a'),
                (Tab::Updates, 'x'),
                (Tab::Hosts, 'A'),
                (Tab::Hosts, 's'),
                (Tab::Browse, 'v'),
                (Tab::Jobs, 'p'),
                (Tab::Overview, 'q'),
                (Tab::Overview, 'j'),
                (Tab::Overview, '1'),
            ] {
                let chord = action_for(KeyEvent::new(KeyCode::Char(c), modifier), tab);
                let bare = on(tab, KeyCode::Char(c));
                assert!(
                    chord.is_none() || chord != bare,
                    "{modifier:?}-{c} was handled as a bare {c} on {}",
                    tab.title()
                );
            }
        }
        // Shift stays not-a-modifier: it is how `A` and `R` are typed, and
        // those are the keys the mockup gives their own jobs.
        assert_eq!(
            action_for(
                KeyEvent::new(KeyCode::Char('A'), KeyModifiers::SHIFT),
                Tab::Hosts
            ),
            Some(Action::Local(Local::AdoptSelection))
        );
    }

    /// Chords are not accidental synonyms for the letters in them.
    #[test]
    fn a_modified_question_mark_is_not_the_help_key() {
        assert_eq!(with_ctrl('?'), None);
        assert_eq!(press(KeyCode::Char('?')), Some(Action::ToggleHelp));
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

    /// A screen's letter beats the global one, and only on that screen. This
    /// is the whole point of the scope, so it is the thing to break loudly:
    /// `a` is adopt on updates and unbound anywhere else, `space` selects on
    /// hosts and nowhere else.
    #[test]
    fn a_screen_takes_a_letter_only_where_it_claimed_it() {
        assert_eq!(
            on(Tab::Updates, KeyCode::Char('a')),
            Some(Action::Local(Local::Adopt))
        );
        assert_eq!(on(Tab::Hosts, KeyCode::Char('a')), None);
        assert_eq!(press(KeyCode::Char('a')), None);

        assert_eq!(
            on(Tab::Hosts, KeyCode::Char(' ')),
            Some(Action::Local(Local::Select))
        );
        assert_eq!(on(Tab::Updates, KeyCode::Char(' ')), None);

        // The selection trio belongs to the screen that selects, and to no
        // other — ADR-002's rule is that the keys are the *same* wherever
        // selection exists, so a second selecting screen extends this test
        // rather than choosing new letters.
        for (key, action) in [
            ('*', Local::SelectAll),
            ('-', Local::SelectNone),
            ('!', Local::SelectInvert),
        ] {
            assert_eq!(
                on(Tab::Hosts, KeyCode::Char(key)),
                Some(Action::Local(action))
            );
            for tab in Tab::ALL {
                if tab != Tab::Hosts {
                    assert_eq!(
                        on(tab, KeyCode::Char(key)),
                        None,
                        "{key} leaked onto {}",
                        tab.title()
                    );
                }
            }
        }
        // `*` and `!` arrive shifted on most keyboards, and shift is not a
        // modifier here — it is how those characters are typed.
        assert_eq!(
            action_for(
                KeyEvent::new(KeyCode::Char('*'), KeyModifiers::SHIFT),
                Tab::Hosts
            ),
            Some(Action::Local(Local::SelectAll))
        );

        // `enter` means two different things on two screens, which is the
        // case the scope exists for.
        assert_eq!(
            on(Tab::Browse, KeyCode::Enter),
            Some(Action::Local(Local::Detail))
        );
        assert_eq!(
            on(Tab::Updates, KeyCode::Enter),
            Some(Action::Local(Local::Review))
        );
        assert_eq!(on(Tab::Jobs, KeyCode::Enter), None);
    }

    /// Everything a screen has not claimed still works there — a screen that
    /// accidentally swallowed `j` or `q` would be a screen nobody can leave.
    ///
    /// Every screen, every time. Screen rows are consulted *first* and win,
    /// so this is not a property of the table that holds once — it is one a
    /// future screen-local row can break by claiming a letter, and the only
    /// way to notice is to ask on all six.
    #[test]
    fn the_global_keys_survive_on_every_screen() {
        for tab in Tab::ALL {
            let where_ = tab.title();
            assert_eq!(
                on(tab, KeyCode::Char('q')),
                Some(Action::Quit),
                "{where_} swallowed the way out"
            );
            // `esc` is the other half of "back, then quit", and it is the
            // one an overlay is dismissed with — a screen that took it would
            // trap a reader inside a prompt.
            assert_eq!(
                on(tab, KeyCode::Esc),
                Some(Action::Quit),
                "{where_} took esc"
            );
            assert_eq!(
                on(tab, KeyCode::Char('j')),
                Some(Action::Scroll(Scroll::LineDown)),
                "{where_} swallowed j"
            );
            assert_eq!(
                on(tab, KeyCode::Char('1')),
                Some(Action::Screen(Tab::Overview)),
                "{where_} swallowed the screen keys"
            );
            assert_eq!(on(tab, KeyCode::Char('?')), Some(Action::ToggleHelp));
        }
    }

    /// ctrl-c is the reflex, and it must reach [`Action::Interrupt`] from
    /// every screen.
    ///
    /// Its own test because its failure mode is the worst one here. Raw mode
    /// means this keystroke is not a SIGINT — if a screen-local row ever
    /// shadowed it, "get me out" would silently do something else, and the
    /// user's next move is `kill -9`, which is the one exit that restores
    /// nothing and leaves the terminal wrecked.
    #[test]
    fn ctrl_c_escapes_from_every_screen() {
        for tab in Tab::ALL {
            assert_eq!(
                action_for(
                    KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
                    tab
                ),
                Some(Action::Interrupt),
                "{} shadowed ctrl-c",
                tab.title()
            );
        }
    }

    /// The overlay is built from these two iterators, and a row that fell out
    /// of both would be a key that works and is documented nowhere.
    #[test]
    fn every_row_is_reachable_from_the_help_overlay() {
        let listed = Tab::ALL
            .into_iter()
            .map(|tab| for_screen(tab).count())
            .sum::<usize>()
            + global().count();
        assert_eq!(listed, BINDINGS.len());
    }

    /// Every row can actually win somewhere.
    ///
    /// Counting rows proves they are *listed*; it does not prove any of them
    /// does anything. A global row whose keys were all claimed by screens, or
    /// a screen row placed after another that answers the same key, would be
    /// documented in the overlay and dead in practice — the exact drift the
    /// one-table design exists to prevent, arriving by a different door.
    ///
    /// So each row is asked to beat the dispatcher on at least one screen,
    /// using a key it itself claims. Rows are matchers rather than key lists,
    /// so the probe is the printable keyboard plus the named keys the table
    /// uses; a row that answers none of those has no way to be pressed.
    #[test]
    fn every_row_wins_for_at_least_one_key_somewhere() {
        let mut probes: Vec<KeyEvent> = (b' '..=b'~')
            .map(|c| KeyEvent::new(KeyCode::Char(c as char), KeyModifiers::NONE))
            .collect();
        probes.extend(
            [
                KeyCode::Enter,
                KeyCode::Esc,
                KeyCode::Tab,
                KeyCode::BackTab,
                KeyCode::Up,
                KeyCode::Down,
                KeyCode::Left,
                KeyCode::Right,
                KeyCode::PageUp,
                KeyCode::PageDown,
            ]
            .map(|code| KeyEvent::new(code, KeyModifiers::NONE)),
        );
        probes.extend(
            ['d', 'u', 'f', 'b', 'c']
                .map(|c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)),
        );

        for binding in BINDINGS {
            let reachable = probes.iter().any(|key| {
                let mine = (binding.of)(*key);
                mine.is_some()
                    && Tab::ALL.into_iter().any(|tab| match binding.scope {
                        Scope::On(t) if t != tab => false,
                        _ => action_for(*key, tab) == mine,
                    })
            });
            assert!(
                reachable,
                "the row documented as {:?} ({}) never wins — it is shadowed \
                 everywhere, or answers no key a keyboard can send",
                binding.keys, binding.what
            );
        }
    }
}
