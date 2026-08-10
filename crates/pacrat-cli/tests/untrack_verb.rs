//! `pacrat untrack` end to end: the binary against a temp store.
//!
//! The unit tests in `untrack.rs` cover the edit as list arithmetic. What
//! they cannot cover is the verb's face: argument parsing, the exit code,
//! the report a reader acts on, and that a refusal really leaves the store
//! byte-identical. Every run gets `env_clear()` and its own `HOME`,
//! `DOTFILES_DIR` and `XDG_CONFIG_HOME`, so nothing here can see — or
//! touch — the developer's real store.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};

struct Sandbox {
    root: PathBuf,
}

struct Out {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// The host name pacrat will decide it is running on. Its rule, replayed:
/// `/etc/hostname` first; the sandbox clears the environment, so the
/// `HOSTNAME` fallback is always empty and the last resort is "unknown".
fn host() -> String {
    match fs::read_to_string("/etc/hostname") {
        Ok(name) if !name.trim().is_empty() => name.trim().to_string(),
        _ => "unknown".into(),
    }
}

impl Sandbox {
    /// A store whose only content is this host's native list.
    fn new(name: &str, native: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("pacrat-untrack-verb-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let dir = root.join("store").join("packages").join(host());
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("native.txt"), native).unwrap();
        Self { root }
    }

    fn native(&self) -> String {
        fs::read_to_string(
            self.root
                .join("store")
                .join("packages")
                .join(host())
                .join("native.txt"),
        )
        .unwrap()
    }

    fn run(&self, args: &[&str]) -> Out {
        let out = Command::new(env!("CARGO_BIN_EXE_pacrat"))
            .args(args)
            // Cleared, not extended: an inherited DOTFILES_DIR or XDG path
            // would let the developer's own store answer for the fixture.
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &self.root)
            .env("DOTFILES_DIR", self.root.join("store"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null())
            .output()
            .expect("pacrat did not run");
        Out {
            code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }
}

/// The whole verb, working: the list is edited, the report names the edit
/// and the file, and the next step points at the store commit and at
/// `sync --prune` — never at an uninstall pacrat would print itself.
#[test]
fn untrack_edits_the_list_and_points_at_the_next_step() {
    let sb = Sandbox::new("edits", "cowsay\nfd\nripgrep\n");
    let out = sb.run(&["untrack", "cowsay"]);
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    assert_eq!(sb.native(), "fd\nripgrep\n");
    assert!(out.stdout.contains("untracked cowsay"), "{}", out.stdout);
    assert!(
        out.stdout.contains("packages/") && out.stdout.contains("native.txt"),
        "the report does not name the list file: {}",
        out.stdout
    );
    assert!(out.stdout.contains("commit the store"), "{}", out.stdout);
    assert!(out.stdout.contains("sync --prune"), "{}", out.stdout);
    // No removal framing: untrack never prints a pacman command.
    assert!(
        !out.stdout.contains("pacman -R"),
        "an uninstall leaked into the report: {}",
        out.stdout
    );
}

/// Like `add`, the verb asks nothing — a pipe on stdin changes nothing,
/// because there is no question to leave unanswered.
#[test]
fn untrack_takes_no_prompt() {
    let sb = Sandbox::new("noprompt", "fd\n");
    let out = sb.run(&["untrack", "fd"]);
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    assert!(
        !out.stdout.contains("[y/N]"),
        "untrack asked a question: {}",
        out.stdout
    );
}

/// A name tracked nowhere refuses the whole run in plain words, exits 1,
/// and leaves the store byte-identical.
#[test]
fn a_stranger_is_refused_and_the_store_is_untouched() {
    let sb = Sandbox::new("stranger", "fd\nripgrep\n");
    let out = sb.run(&["untrack", "fd", "cowsay"]);
    assert_eq!(out.code, 1);
    assert!(
        out.stderr.contains("cowsay is not tracked on"),
        "{}",
        out.stderr
    );
    assert_eq!(sb.native(), "fd\nripgrep\n", "a refusal wrote anyway");
}
