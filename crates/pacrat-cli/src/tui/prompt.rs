//! A line of text the reader types: the browse search term, and the
//! justification behind an override.
//!
//! Modal by construction. While a prompt is open every printable key is
//! *text*, including the ones that are verbs elsewhere — a search for
//! "qgis" that quit on the `q` would be the kind of bug nobody reports
//! because they assume they did it wrong. Only `enter`, `esc` and
//! `backspace` mean anything else, and [`Prompt::key`] is the only place
//! that decides which is which.
//!
//! The cap is not decoration. The justification lands in
//! `aur/decisions.toml`, which every host in the fleet parses, and
//! `pacrat_core::decisions` refuses an entry whose reason is over
//! [`REASON_MAX`] characters — so a prompt that let one be typed would be a
//! prompt that ends in a refusal after the friction was already paid.
//! Refusing the keystroke is the same rule enforced where the reader can
//! still do something about it.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// What a keystroke did to an open prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Typed {
    /// Still typing.
    Editing,
    /// `enter`. The caller reads [`Prompt::value`] and acts.
    Submitted,
    /// `esc`. Nothing was asked for.
    Cancelled,
    /// A key with no meaning in a prompt.
    Ignored,
}

pub struct Prompt {
    /// The question, shown above the line.
    pub label: String,
    /// A word about what a good answer looks like, or empty.
    pub hint: String,
    value: String,
    max: usize,
}

impl Prompt {
    pub fn new(label: impl Into<String>, hint: impl Into<String>, max: usize) -> Self {
        Self {
            label: label.into(),
            hint: hint.into(),
            value: String::new(),
            max,
        }
    }

    pub fn value(&self) -> &str {
        &self.value
    }

    /// Non-blank. Trimmed, because the ledger's validator trims before it
    /// checks and a prompt that accepted three spaces would hand it an
    /// entry it is about to refuse.
    pub fn answered(&self) -> bool {
        !self.value.trim().is_empty()
    }

    pub fn remaining(&self) -> usize {
        self.max.saturating_sub(self.value.chars().count())
    }

    /// Feed the prompt a key.
    pub fn key(&mut self, key: KeyEvent) -> Typed {
        match key.code {
            KeyCode::Enter => Typed::Submitted,
            KeyCode::Esc => Typed::Cancelled,
            KeyCode::Backspace => {
                self.value.pop();
                Typed::Editing
            }
            // ctrl-u, the readline reflex for "start this line again".
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.value.clear();
                Typed::Editing
            }
            KeyCode::Char(c) if key.modifiers.contains(KeyModifiers::CONTROL) => {
                let _ = c;
                Typed::Ignored
            }
            // Nothing starts with a blank. Neither of this prompt's two uses
            // has a meaning for leading whitespace — a search term is
            // trimmed and a justification is trimmed by the ledger's own
            // validator — and refusing it here fixes something specific: the
            // justification prompt opens the instant the childlock's hold is
            // satisfied, while the reader's finger is still on the space
            // bar, and every repeat until they let go would otherwise land
            // in the field as text. Twenty-two spaces and a character budget
            // spent on them is what that looked like.
            KeyCode::Char(c) if c.is_whitespace() && self.value.is_empty() => Typed::Ignored,
            // Control characters are dropped at the door rather than
            // rendered as stand-ins. Everything else pacrat shows was
            // authored elsewhere and has to be neutered on the way out; this
            // string is being authored *here*, and the honest thing is to
            // refuse the keystroke rather than store a byte and show
            // something else.
            KeyCode::Char(c) if !c.is_control() && self.remaining() > 0 => {
                self.value.push(c);
                Typed::Editing
            }
            _ => Typed::Ignored,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(prompt: &mut Prompt, c: char) -> Typed {
        prompt.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
    }

    fn code(prompt: &mut Prompt, code: KeyCode) -> Typed {
        prompt.key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn prompt() -> Prompt {
        Prompt::new("why", "", 20)
    }

    /// The bug this modality exists to prevent: every letter is a letter,
    /// including the ones bound to verbs on the screen underneath.
    #[test]
    fn the_keys_that_are_verbs_elsewhere_are_text_here() {
        let mut p = prompt();
        for c in "qgis".chars() {
            assert_eq!(press(&mut p, c), Typed::Editing);
        }
        assert_eq!(p.value(), "qgis");
        assert!(p.answered());
    }

    #[test]
    fn enter_submits_and_escape_cancels() {
        let mut p = prompt();
        press(&mut p, 'x');
        assert_eq!(code(&mut p, KeyCode::Enter), Typed::Submitted);
        assert_eq!(code(&mut p, KeyCode::Esc), Typed::Cancelled);
        // Neither key changes the text, so a caller may read it after either.
        assert_eq!(p.value(), "x");
    }

    #[test]
    fn backspace_and_ctrl_u_take_it_back() {
        let mut p = prompt();
        for c in "abc".chars() {
            press(&mut p, c);
        }
        code(&mut p, KeyCode::Backspace);
        assert_eq!(p.value(), "ab");
        p.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(p.value(), "");
        // Backspace on an empty line is not an error.
        assert_eq!(code(&mut p, KeyCode::Backspace), Typed::Editing);
        assert!(!p.answered());
    }

    /// The cap is the ledger's, enforced where the reader can still react to
    /// it rather than as a refusal after the friction was paid.
    #[test]
    fn typing_stops_at_the_cap_rather_than_growing_an_entry_the_ledger_refuses() {
        let mut p = Prompt::new("why", "", 4);
        for c in "abcdefgh".chars() {
            press(&mut p, c);
        }
        assert_eq!(p.value(), "abcd");
        assert_eq!(p.remaining(), 0);
        assert_eq!(press(&mut p, 'z'), Typed::Ignored);
        // And there is room again once something is taken back.
        code(&mut p, KeyCode::Backspace);
        assert_eq!(p.remaining(), 1);
        assert_eq!(press(&mut p, 'z'), Typed::Editing);
        assert_eq!(p.value(), "abcz");
    }

    /// Blank is empty — the same rule `Decision::validate` applies, applied
    /// here so the two cannot disagree about what an answer is.
    #[test]
    fn whitespace_is_not_an_answer() {
        let mut p = prompt();
        for c in "   ".chars() {
            press(&mut p, c);
        }
        assert!(!p.answered());
        press(&mut p, '.');
        assert!(p.answered());
    }

    /// Multi-byte text is counted in characters, and a cap in bytes would
    /// have panicked on the pop.
    #[test]
    fn accented_text_survives_and_counts_as_characters() {
        let mut p = Prompt::new("why", "", 3);
        for c in "éàü".chars() {
            press(&mut p, c);
        }
        assert_eq!(p.value(), "éàü");
        assert_eq!(p.remaining(), 0);
        code(&mut p, KeyCode::Backspace);
        assert_eq!(p.value(), "éà");
    }
}
