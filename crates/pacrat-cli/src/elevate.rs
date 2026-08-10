//! The confirmed run: one door for every command pacrat executes on this
//! host's say-so — ADR-003's sudo flow, amending ADR-001's "pacrat never
//! elevates" to "pacrat never elevates *unasked*, and never authenticates
//! itself". Four rules, in order:
//!
//! 1. the exact argv is printed first, `sudo` included — the always-visible
//!    -calls rule was never negotiable;
//! 2. a y/n is asked on a real terminal, and there is no `--yes` past it: an
//!    elevated command answered by a pipe is precisely the accident the
//!    question exists to catch;
//! 3. the command runs with the terminal attached, so sudo does its own
//!    prompting and caching — pacrat never reads, stores or forwards a
//!    credential;
//! 4. no terminal, no flow: [`Session::open`] answers `None` up front and
//!    callers fall back to printing the commands, so a timer cannot elevate.
//!
//! Not everything that walks through here is sudo — `sync --run` brings its
//! whole plan, `pacrat vendor` lines included — but everything pacrat has
//! ever run as root came through here, which is the one-code-path answer
//! ADR-003 asks for.
//!
//! Tests cover the pure pieces — the printed line, the answer grammar, the
//! closing report — and never spawn anything: a test that runs sudo is a
//! test that prompts for a password, or worse, does not.

use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::process::Command;

use crate::out::{shell_quote, visible_line};

/// One command for the flow: an argv handed straight to exec — no shell —
/// plus, optionally, a file to feed it on stdin (`sudo tee` appending a
/// staged section is the caller). The printed line is derived from these
/// fields and nothing else, so what is shown is what runs.
pub struct Cmd {
    argv: Vec<String>,
    stdin: Option<PathBuf>,
}

impl Cmd {
    /// `sudo <args…>` — the constructor for elevated commands pacrat
    /// composes itself. Not the only place the word appears: sync's plan
    /// carries `sudo` in its argv as data, built where the plan is built.
    /// The invariant that does hold is one exec path — every elevated
    /// command reaches the terminal through [`Session::run`], printed and
    /// confirmed, argv as data, no shell. To enumerate what pacrat can run
    /// as root, grep for `"sudo"`, not for this constructor.
    pub fn sudo(args: &[&str]) -> Self {
        let mut argv = Vec::with_capacity(args.len() + 1);
        argv.push("sudo".to_string());
        argv.extend(args.iter().map(|a| (*a).to_string()));
        Self { argv, stdin: None }
    }

    /// A command that is not elevated but still walks the confirmed flow —
    /// `sync --run`'s plan has `pacrat` and `flatpak` lines beside its sudo
    /// ones, and a plan half of whose commands skipped the question would
    /// not be a walked plan.
    pub fn plain(argv: Vec<String>) -> Self {
        Self { argv, stdin: None }
    }

    /// Feed the command this file on stdin. Part of the printed line (`<
    /// path`), because a command whose input is hidden is a command the
    /// human cannot judge.
    pub fn reading(mut self, path: PathBuf) -> Self {
        self.stdin = Some(path);
        self
    }

    /// The display line, derived from the argv — never the reverse.
    pub fn line(&self) -> String {
        let mut out = self
            .argv
            .iter()
            .map(|a| visible_line(&shell_quote(a)).0)
            .collect::<Vec<_>>()
            .join(" ");
        if let Some(path) = &self.stdin {
            out.push_str(" < ");
            out.push_str(&visible_line(&shell_quote(&path.to_string_lossy())).0);
        }
        out
    }
}

/// How one walk of the flow ended. `Failed` covers both "did not spawn" and
/// "ran and said no" — either way the step is not done and the report says
/// so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Ran,
    Declined,
    Failed,
}

/// A run of confirmed commands and the record of how each went, reported
/// once at the end so a declined step cannot be quietly forgotten.
pub struct Session {
    done: Vec<(String, Verdict)>,
}

impl Session {
    /// `None` when there is no terminal on both ends — rule 4. The caller's
    /// fallback is what it always was: print the commands and stop.
    pub fn open() -> Option<Self> {
        (io::stdin().is_terminal() && io::stdout().is_terminal())
            .then(|| Session { done: Vec::new() })
    }

    /// Print, ask, run. The terminal was checked at [`Session::open`]; the
    /// question still reads a line rather than assuming, and anything but a
    /// typed `y` is a no.
    pub fn run(&mut self, cmd: Cmd) -> Verdict {
        let line = cmd.line();
        crate::say!("run       {line}");
        let verdict = match self.confirm() {
            Ok(false) => {
                crate::say!("          declined — skipped.");
                Verdict::Declined
            }
            Ok(true) => match execute(&cmd) {
                Ok(()) => Verdict::Ran,
                Err(e) => {
                    crate::say!("          {e}");
                    Verdict::Failed
                }
            },
            Err(e) => {
                crate::say!("          {e} — treating that as a no.");
                Verdict::Declined
            }
        };
        self.done.push((line, verdict));
        verdict
    }

    fn confirm(&self) -> Result<bool, String> {
        print!("          run it? [y/N] ");
        io::stdout().flush().map_err(|e| format!("stdout: {e}"))?;
        let mut answer = String::new();
        let read = io::stdin()
            .read_line(&mut answer)
            .map_err(|e| format!("stdin: {e}"))?;
        if read == 0 {
            println!();
            return Ok(false);
        }
        Ok(is_yes(&answer))
    }

    pub fn any_declined(&self) -> bool {
        self.done.iter().any(|(_, v)| *v == Verdict::Declined)
    }

    pub fn any_failed(&self) -> bool {
        self.done.iter().any(|(_, v)| *v == Verdict::Failed)
    }

    /// Say how the walk went — counts first, then every command that did
    /// not run, verbatim, so what is still yours to do is on the screen at
    /// the end and not twelve confirmations up.
    pub fn report(&self) {
        for line in summary(&self.done) {
            crate::say!("{line}");
        }
    }
}

/// The answer grammar, shared with every other prompt in pacrat: a typed
/// `y`, nothing else.
fn is_yes(answer: &str) -> bool {
    matches!(answer.trim(), "y" | "Y")
}

/// The closing report, as lines. Pure so the shape is testable.
fn summary(done: &[(String, Verdict)]) -> Vec<String> {
    if done.is_empty() {
        return Vec::new();
    }
    let count = |want: Verdict| done.iter().filter(|(_, v)| *v == want).count();
    let (ran, declined, failed) = (
        count(Verdict::Ran),
        count(Verdict::Declined),
        count(Verdict::Failed),
    );
    let mut lines = vec![format!(
        "asked     {} command{} — {ran} ran, {declined} declined, {failed} failed",
        done.len(),
        if done.len() == 1 { "" } else { "s" },
    )];
    for (line, verdict) in done {
        match verdict {
            Verdict::Ran => {}
            Verdict::Declined => lines.push(format!("  declined  {line}")),
            Verdict::Failed => lines.push(format!("  failed    {line}")),
        }
    }
    lines
}

/// The thin exec wrapper: stdio inherited (or the stated file on stdin), so
/// the subprocess — sudo's password prompt included — talks to the same
/// terminal the human just answered on.
fn execute(cmd: &Cmd) -> Result<(), String> {
    let mut proc = Command::new(&cmd.argv[0]);
    proc.args(&cmd.argv[1..]);
    if let Some(path) = &cmd.stdin {
        let file = fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
        proc.stdin(file);
    }
    let status = proc.status().map_err(|e| format!("{}: {e}", cmd.argv[0]))?;
    if !status.success() {
        return Err(format!("failed ({status})"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The printed line is the argv, quoted the way the shell would need it
    /// written, with the stdin file stated — nothing about it is composed
    /// separately from what runs.
    #[test]
    fn the_line_is_derived_from_the_argv_and_names_the_stdin_file() {
        let cmd = Cmd::sudo(&["tee", "-a", "/etc/pacman.conf"]).reading(PathBuf::from(
            "/home/u/.local/state/pacrat/setup/pacman.conf.section",
        ));
        assert_eq!(
            cmd.line(),
            "sudo tee -a /etc/pacman.conf < /home/u/.local/state/pacrat/setup/pacman.conf.section"
        );

        // A path that needs quoting is quoted on the line — the same rule
        // every printed command follows.
        let cmd = Cmd::sudo(&["install", "-d", "-o", "u", "/srv/my repo"]);
        assert_eq!(cmd.line(), "sudo install -d -o u '/srv/my repo'");
    }

    /// A hostile argument cannot forge the line it is shown on: escapes are
    /// made visible and whitespace cannot fake a second command.
    #[test]
    fn a_hostile_argument_cannot_lie_on_the_printed_line() {
        let cmd = Cmd::sudo(&["install", "-d", "/srv/\u{1b}[8mHidden"]);
        let line = cmd.line();
        assert!(!line.contains('\u{1b}'), "escape survived: {line:?}");
        assert!(line.contains("␛[8m"), "the bytes are not shown: {line}");
    }

    #[test]
    fn only_a_typed_y_is_a_yes() {
        for yes in ["y", "Y", " y\n"] {
            assert!(is_yes(yes), "{yes:?}");
        }
        for no in ["", "\n", "n", "N", "yes", "Y es", "sudo"] {
            assert!(!is_yes(no), "{no:?} must not be a yes");
        }
    }

    #[test]
    fn the_report_counts_and_names_what_did_not_run() {
        assert!(summary(&[]).is_empty(), "nothing asked, nothing said");
        let done = vec![
            ("sudo install -d /srv/repo".to_string(), Verdict::Ran),
            (
                "sudo tee -a /etc/pacman.conf".to_string(),
                Verdict::Declined,
            ),
            ("sudo install -Dm644 hook".to_string(), Verdict::Failed),
        ];
        let lines = summary(&done);
        assert!(lines[0].contains("3 commands"), "{lines:?}");
        assert!(
            lines[0].contains("1 ran, 1 declined, 1 failed"),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("declined  sudo tee")),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("failed    sudo install -Dm644")),
            "{lines:?}"
        );
        // What ran needs no second mention — it already printed itself.
        assert!(
            !lines.iter().any(|l| l.contains("install -d /srv/repo")),
            "{lines:?}"
        );
    }
}
