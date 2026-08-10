//! Every git subprocess pacrat runs, and the guarantees they all share.
//!
//! git is the one tool pacrat asks about things it does not control: an
//! upstream URL out of a synced ledger, a remote that may be hostile, merely
//! broken, or (the AUR, mid-incident) deliberately refusing. Four rules make
//! that survivable, and they are here rather than at each call site because
//! the one call that forgets a rule is the one that hangs a timer or prompts
//! a machine that has no keyboard:
//!
//! 1. **No interactive doors.** `GIT_TERMINAL_PROMPT=0` and
//!    `GIT_ASKPASS=/bin/true`: a private upstream fails fast instead of
//!    asking a systemd unit for a password. (stdin is already `/dev/null` —
//!    [`crate::proc`] closes it.)
//! 2. **A wall-clock bound.** Every call goes through
//!    [`crate::proc::run_with_timeout`], so "never answers" is a reportable
//!    outcome rather than a terminal nobody can leave. Even the local calls:
//!    a `rev-parse` in a scratch clone answers at once *unless* the
//!    filesystem it is on has gone away, and that is exactly when a hang
//!    costs the most.
//! 3. **The argv is shown.** ADR-001's always-visible-calls rule, with no
//!    exception for the quiet ones. Quoted so the line can be pasted, and
//!    neutered so a URL out of the ledger cannot paint the terminal it is
//!    printed on.
//! 4. **`--` before anything that came from outside**, so a URL beginning
//!    with a hyphen is an argument and not an option. [`url_arg`] refuses
//!    one anyway: defence in depth is cheap, and the error naming the URL is
//!    kinder than git's.
//!
//! **`GIT_SSH_COMMAND` is deliberately left alone.** It is the one obvious
//! addition to the list above and it would be wrong: a maintainer's AUR push
//! key is configured in *their* `~/.ssh/config` — an `IdentityFile`, an
//! `IdentitiesOnly`, a `ProxyJump` — and clobbering the ssh invocation with
//! pacrat's own flags would break exactly the machine that has been set up
//! properly. ssh's own prompts (a passphrase, an unknown host key) are not
//! reachable through the env above; rule 2 is what covers them, which is why
//! it has no exceptions. Where pacrat runs ssh *itself* rather than through
//! git — the AUR write probe in [`crate::push`] — it does pass those flags,
//! because there it is the one choosing the command line.

use std::ffi::{OsStr, OsString};
use std::process::Command;
use std::time::Duration;

use crate::out::{shell_quote, truncate, visible, visible_line};
use crate::proc::{self, Ran};

/// A call that touches only the local filesystem. Generous, because the
/// bound is here to catch a wedged filesystem, not to hurry git along.
pub const LOCAL: Duration = Duration::from_secs(30);

/// One question to a remote (`ls-remote`). Sized against the AUR RPC's
/// `--max-time 10` — the same order of patience for the same kind of wait —
/// with room for a slow forge's TLS handshake on top.
pub const REMOTE: Duration = Duration::from_secs(15);

/// Transferring a repository, in either direction. Longer than [`REMOTE`]
/// because this one moves bytes rather than asking a question, and short
/// enough that a remote which accepts the connection and then goes quiet
/// does not become a terminal a human is stuck in.
pub const TRANSFER: Duration = Duration::from_secs(120);

/// A diff. Local work, over a tree somebody else chose the size of.
pub const DIFF: Duration = Duration::from_secs(60);

/// Config that keeps a diff a rendering of the *bytes*.
///
/// `diff.external` replaces the diff body outright — a reviewer demonstrated
/// a gitconfig that answered "PKGBUILD: no changes worth reading" — and a
/// `diff` attribute with a `textconv` does the same per path, from a
/// `.gitattributes` the other side of the diff controls. Both doors are shut
/// here rather than trusted to be unused, in the one view a human uses to
/// decide whether to trust a tree.
pub const NO_ATTRS: [&str; 2] = ["-c", "core.attributesFile=/dev/null"];

/// The `diff` flags that go with [`NO_ATTRS`]; separate because they sit on
/// the other side of the subcommand.
pub const NO_FILTERS: [&str; 2] = ["--no-ext-diff", "--no-textconv"];

/// Where the argv line goes.
///
/// stdout for a human. stderr when stdout belongs to a machine-readable
/// answer — `pacrat updates --format json` still shows every call it makes,
/// because "external calls are always visible" has no quiet mode; the format
/// changes where the trace lands, never whether it exists.
#[derive(Copy, Clone, PartialEq, Eq)]
pub enum Trace {
    Stdout,
    Stderr,
}

impl Trace {
    pub fn line(self, text: &str) {
        match self {
            Trace::Stdout => println!("{text}"),
            Trace::Stderr => eprintln!("{text}"),
        }
    }
}

/// One git invocation: its argv, its bound, and where its trace goes.
pub struct Git {
    argv: Vec<OsString>,
    timeout: Duration,
    trace: Trace,
}

impl Git {
    /// A local call traced to stdout — override either with [`Git::timeout`]
    /// and [`Git::trace`].
    pub fn new<I, S>(argv: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        Self {
            argv: argv
                .into_iter()
                .map(|a| a.as_ref().to_os_string())
                .collect(),
            timeout: LOCAL,
            trace: Trace::Stdout,
        }
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn trace(mut self, trace: Trace) -> Self {
        self.trace = trace;
        self
    }

    /// The command as a line a human can read and paste.
    ///
    /// Quoted *and* neutered. The quoting makes the line true; the neutering
    /// is needed because one of these arguments is often an upstream URL out
    /// of a ledger every host in the fleet can write to, and single quotes
    /// do not stop an escape code from painting a terminal.
    pub fn line(&self) -> String {
        let mut out = String::from("git");
        for arg in &self.argv {
            out.push(' ');
            out.push_str(&visible_line(&shell_quote(&arg.to_string_lossy())).0);
        }
        out
    }

    /// Run it. `Err` means it did not run or did not answer; a git that ran
    /// and said no comes back as `Ok` with a failed status, because some
    /// callers ([`crate::review`]'s diff) read the code themselves.
    pub fn run(self) -> Result<Ran, String> {
        let shown = self.line();
        self.trace.line(&format!("run       {shown}"));

        let mut cmd = Command::new("git");
        cmd.args(&self.argv)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ASKPASS", "/bin/true");
        let ran = proc::run_with_timeout(cmd, self.timeout).map_err(|e| format!("git: {e}"))?;
        if ran.timed_out {
            return Err(format!(
                "{shown} timed out after {}s",
                self.timeout.as_secs()
            ));
        }
        Ok(ran)
    }

    /// Run it and require success; the answer is trimmed stdout.
    pub fn text(self) -> Result<String, String> {
        let shown = self.line();
        let ran = self.run()?;
        if !ran.status.success() {
            let reason = error(&String::from_utf8_lossy(&ran.stderr));
            return Err(if reason.is_empty() {
                format!("{shown} exited {}", ran.status)
            } else {
                format!("{shown} failed: {reason}")
            });
        }
        Ok(String::from_utf8_lossy(&ran.stdout).trim().to_string())
    }
}

/// A URL on its way to becoming a git argument.
///
/// Every call site pairs this with a literal `--`, which is what actually
/// makes a hyphenated URL safe. This exists for the error message: git's own
/// complaint about `--upload-pack=…` names a flag the user never typed,
/// while this names the string and where it came from.
pub fn url_arg(url: &str) -> Result<&str, String> {
    if url.starts_with('-') {
        return Err(format!(
            "{:?} is not a URL — a git remote cannot begin with '-'",
            visible_line(url).0
        ));
    }
    if url.is_empty() {
        return Err("the remote URL is empty".into());
    }
    Ok(url)
}

/// Boil git's stderr down to one line fit for a table cell or an error.
///
/// Three things happen here, each for its own reason. The text is a
/// *remote's* text, so it goes through the same neutering a PKGBUILD does — a
/// server that can name itself can otherwise paint the terminal. It collapses
/// to one line, so it cannot forge extra rows. And git's advice prose
/// ("Please make sure you have the correct access rights…") is dropped in
/// favour of its `fatal:` / `error:` / `remote:` lines, which are the part
/// that says what actually went wrong.
pub fn error(stderr: &str) -> String {
    /// Long enough for a URL and a reason; short enough not to reflow a table.
    const CAP: usize = 120;

    let (safe, _) = visible(stderr.trim());
    let lines: Vec<&str> = safe
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let diagnostic: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|l| l.starts_with("fatal:") || l.starts_with("error:") || l.starts_with("remote:"))
        .collect();
    let kept = if diagnostic.is_empty() {
        lines
    } else {
        diagnostic
    };
    truncate(&kept.join("; "), CAP)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The argv line is pacrat's own output about someone else's string. It
    /// has to stay one pasteable line whatever the ledger says the upstream
    /// is — quotes do not stop an escape code.
    #[test]
    fn a_printed_argv_can_neither_paint_nor_sprawl() {
        let hostile = "file:///srv/x\x1b[2K\nrun       git push --force";
        let line = Git::new(["clone", "--", hostile, "/tmp/scratch"]).line();
        assert!(!line.contains('\x1b'), "{line}");
        assert_eq!(line.lines().count(), 1, "{line}");
        assert!(line.starts_with("git clone -- "), "{line}");
        // An ordinary URL is left alone, quotes and all.
        assert_eq!(
            Git::new([
                "clone",
                "--",
                "https://aur.archlinux.org/mdcat.git",
                "/tmp/x"
            ])
            .line(),
            "git clone -- https://aur.archlinux.org/mdcat.git /tmp/x"
        );
    }

    #[test]
    fn a_url_that_could_be_an_option_is_refused_by_name() {
        let err = url_arg("--upload-pack=touch /tmp/pwn").unwrap_err();
        assert!(err.contains("upload-pack"), "{err}");
        assert!(url_arg("").is_err());
        assert_eq!(
            url_arg("ssh://aur@aur.archlinux.org/x.git").unwrap(),
            "ssh://aur@aur.archlinux.org/x.git"
        );
        // The refusal must not itself carry what it is refusing.
        let err = url_arg("-\x1b[2Kx").unwrap_err();
        assert!(!err.contains('\x1b'), "{err}");
    }

    /// The env is the whole point of the module: a git that can prompt is a
    /// git that can hang a timer. Asked of the child itself — through an
    /// alias that shells out — rather than of the builder, so what is
    /// asserted is what actually ran.
    #[test]
    fn the_interactive_doors_are_shut_for_the_child() {
        let ran = Git::new([
            "-c",
            r#"alias.pacrat-env=!printf '%s\n' "$GIT_ASKPASS" "$GIT_TERMINAL_PROMPT""#,
            "pacrat-env",
        ])
        .run()
        .unwrap();
        assert!(
            ran.status.success(),
            "{}",
            String::from_utf8_lossy(&ran.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&ran.stdout), "/bin/true\n0\n");
    }

    /// A hang is a reported failure, not a terminal nobody can leave. A
    /// local sleep tests the same wiring a black-holed remote would, without
    /// the network.
    #[test]
    fn a_bound_is_a_bound() {
        let started = std::time::Instant::now();
        let err = Git::new(["-c", "alias.pacrat-wait=!sleep 60", "pacrat-wait"])
            .timeout(Duration::from_millis(300))
            .run()
            .unwrap_err();
        assert!(err.contains("timed out"), "{err}");
        assert!(err.contains("pacrat-wait"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(10), "{err}");
    }

    #[test]
    fn a_failure_carries_gits_own_diagnosis() {
        let err = Git::new(["-C", "/nonexistent/pacrat", "rev-parse", "HEAD"])
            .text()
            .unwrap_err();
        assert!(err.contains("fatal:"), "{err}");
        assert!(err.starts_with("git -C /nonexistent/pacrat"), "{err}");
    }

    #[test]
    fn stderr_is_boiled_down_to_one_neutered_line() {
        let stderr = "Cloning into 'x'...\n\
                      fatal: could not read Username for 'https://x': No such device\n\
                      Please make sure you have the correct access rights\n";
        assert_eq!(
            error(stderr),
            "fatal: could not read Username for 'https://x': No such device"
        );
        assert_eq!(error("fatal: \x1b[2Kx\n"), "fatal: ␛[2Kx");
        assert!(!error("fatal: a\nfatal: b\n").contains('\n'));
        // A remote's own refusal is a diagnosis, not advice prose: the AUR
        // says what it says through `remote:`.
        assert_eq!(
            error("remote: The AUR is down\nTo ssh://x\nhint: try later\n"),
            "remote: The AUR is down"
        );
        // Nothing that looks like a diagnosis: keep what there is.
        assert_eq!(error("something odd\n"), "something odd");
        assert_eq!(error("   \n\n"), "");
        let long = format!("fatal: {}", "x".repeat(300));
        assert_eq!(error(&long).chars().count(), 120);
        assert!(error(&long).ends_with('…'));
    }
}
