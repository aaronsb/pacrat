//! `pacrat update` end to end: a real git upstream, a real grade cache, a
//! real store, and — once — a real `makepkg`.
//!
//! The unit tests in `update.rs` cover the gate as arithmetic: which verdict
//! means what in which mode, and which outcome earns which exit code. What
//! they cannot cover is whether the loop *wires the gate to anything*. A
//! `decide` that says Hold and a run that adopts anyway would pass every one
//! of them. So everything here goes through the binary, and the assertions
//! are about the store on disk: whether the tree changed, whether the ledger
//! moved, whether a package reached the repo.
//!
//! The upstream is a git repository in a temp directory, reached over
//! `file://`. Nothing here touches the network, the AUR, the user's store,
//! the user's grade cache, or the system's pacman repos: every run gets
//! `env_clear()` and its own `HOME`, `DOTFILES_DIR`, `XDG_CONFIG_HOME`,
//! `XDG_STATE_HOME` and repo directory.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The package every scenario curates. Also the pkgbase makepkg builds, so
/// it has to be a name pacman would accept.
const PKG: &str = "pacrat-test-pkg";

/// The exit code ADR-001 gives "ran fine, deliberately did not act".
const HELD: i32 = 10;

struct Sandbox {
    root: PathBuf,
}

struct Out {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Out {
    /// Both streams, for assertions that do not care which one a line took.
    fn text(&self) -> String {
        format!("{}{}", self.stdout, self.stderr)
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

impl Sandbox {
    /// A store with one vendored package, an upstream git repository it was
    /// vendored from, and a repo directory to build into.
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "pacrat-update-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let sb = Self { root };
        fs::create_dir_all(sb.root.join("repo")).unwrap();
        fs::create_dir_all(sb.root.join("upstream")).unwrap();

        // The upstream, at the commit that will be "reviewed".
        sb.git(&["init", "-q", "-b", "main", "."]);
        sb.write("upstream/PKGBUILD", &pkgbuild("1.0"));
        let reviewed = sb.commit("initial");

        // The store: the same bytes, and a ledger pointing at that commit.
        sb.write(
            &format!("store/aur/packages/{PKG}/PKGBUILD"),
            &pkgbuild("1.0"),
        );
        sb.write(
            "store/aur/sources.toml",
            &format!(
                "[packages.{PKG}]\n\
                 upstream = \"{}\"\n\
                 reviewed = \"{reviewed}\"\n\
                 role = \"vendored\"\n",
                sb.upstream()
            ),
        );
        sb.configure(Some(0), 0);
        sb
    }

    fn write(&self, rel: &str, contents: &str) -> PathBuf {
        let path = self.root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, contents).unwrap();
        path
    }

    fn upstream(&self) -> String {
        format!("file://{}", self.root.join("upstream").display())
    }

    /// git in the upstream repository, with an identity of its own so the
    /// developer's `~/.gitconfig` is neither needed nor consulted.
    fn git(&self, args: &[&str]) -> String {
        let out = Command::new("git")
            .current_dir(self.root.join("upstream"))
            .args(args)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &self.root)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_AUTHOR_NAME", "pacrat tests")
            .env("GIT_AUTHOR_EMAIL", "tests@localhost")
            .env("GIT_COMMITTER_NAME", "pacrat tests")
            .env("GIT_COMMITTER_EMAIL", "tests@localhost")
            .output()
            .expect("git did not run");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn commit(&self, message: &str) -> String {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "-m", message]);
        self.git(&["rev-parse", "HEAD"])
    }

    /// Move the upstream on: a new version, a new commit, and drift.
    fn publish(&self, version: &str) -> String {
        self.write("upstream/PKGBUILD", &pkgbuild(version));
        self.commit(&format!("bump to {version}"))
    }

    /// A grader that answers `grade` about whatever it is asked, and exits
    /// `code`. A non-zero exit is how the ungraded cases are staged: pacrat
    /// takes nothing from a grader that failed, which is the whole of
    /// "failure is never proceed".
    ///
    /// `None` configures no grader at all — the other road to ungraded.
    fn configure(&self, grade: Option<u8>, code: i32) {
        let Some(grade) = grade else {
            self.write("config/pacrat/config.toml", &self.repo_config());
            return;
        };
        let log = self.root.join("grader-invocations");
        let script = self.write(
            "grader.sh",
            &format!(
                // An *unquoted* heredoc: the grader has to answer about the
                // subject it was handed, and `is_about` refuses a report
                // that names another package or another commit — so `$2`
                // and `$6` have to expand for this fixture to be a grading
                // of anything at all.
                "#!/bin/sh\n\
                 echo \"$@\" >> {}\n\
                 cat <<EOF\n\
                 {{ \"contract\": \"pacrat-grade/v1\", \"grader\": \"stub\",\n\
                 \"subject\": {{ \"package\": \"$2\", \"commit\": \"$6\" }},\n\
                 \"grade\": {grade}, \"scale\": {{ \"min\": 0, \"max\": 4 }},\n\
                 \"findings\": [], \"meta\": {{}} }}\n\
                 EOF\n\
                 exit {code}\n",
                log.display()
            ),
        );
        let mut perms = fs::metadata(&script).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        fs::set_permissions(&script, perms).unwrap();
        self.write(
            "config/pacrat/config.toml",
            &format!(
                "{}\n\
                 [[graders]]\n\
                 name = \"stub\"\n\
                 cmd = [\"{}\", \"--package\", \"{{package}}\", \"--tree\", \"{{tree}}\", \
                 \"--commit\", \"{{commit}}\"]\n\
                 timeout_s = 60\n",
                self.repo_config(),
                script.display()
            ),
        );
    }

    /// A grader that answers PROCEED *and edits the tree it was given*.
    ///
    /// Not a hypothetical: a grader is handed a path, runs as the user, and
    /// nothing about being asked to read a file stops it writing one. The
    /// line it appends is the payload — if pacrat decided on the strength of
    /// the verdict alone, these bytes would be adopted, built and served.
    fn configure_tampering_grader(&self) {
        let script = self.write(
            "grader.sh",
            &format!(
                "#!/bin/sh\n\
                 echo \"$@\" >> {}\n\
                 echo '# appended by the grader after it read the file' >> \"$4/PKGBUILD\"\n\
                 cat <<EOF\n\
                 {{ \"contract\": \"pacrat-grade/v1\", \"grader\": \"stub\",\n\
                 \"subject\": {{ \"package\": \"$2\", \"commit\": \"$6\" }},\n\
                 \"grade\": 0, \"scale\": {{ \"min\": 0, \"max\": 4 }},\n\
                 \"findings\": [], \"meta\": {{}} }}\n\
                 EOF\n",
                self.root.join("grader-invocations").display()
            ),
        );
        let mut perms = fs::metadata(&script).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        fs::set_permissions(&script, perms).unwrap();
        self.write(
            "config/pacrat/config.toml",
            &format!(
                "{}\n\
                 [[graders]]\n\
                 name = \"stub\"\n\
                 cmd = [\"{}\", \"--package\", \"{{package}}\", \"--tree\", \"{{tree}}\", \
                 \"--commit\", \"{{commit}}\"]\n\
                 timeout_s = 60\n",
                self.repo_config(),
                script.display()
            ),
        );
    }

    fn repo_config(&self) -> String {
        format!(
            "[repo]\nname = \"pacrat-test\"\npath = \"{}\"\n",
            self.root.join("repo").display()
        )
    }

    /// How many times a grader has actually been executed in this sandbox.
    fn grader_runs(&self) -> usize {
        fs::read_to_string(self.root.join("grader-invocations"))
            .map(|log| log.lines().count())
            .unwrap_or(0)
    }

    fn run(&self, args: &[&str]) -> Out {
        self.run_with_stdin(args, None)
    }

    /// `stdin: None` is `/dev/null` — not a terminal, and at EOF. That is
    /// the timer's world, and it is also what makes "a pipe is not a human"
    /// testable: `Some("y")` below is a pipe carrying a yes.
    fn run_with_stdin(&self, args: &[&str], stdin: Option<&str>) -> Out {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_pacrat"));
        cmd.args(args)
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
            .stdin(match stdin {
                None => Stdio::null(),
                Some(_) => Stdio::piped(),
            });
        let mut child = cmd.spawn().expect("pacrat did not run");
        if let Some(text) = stdin {
            child
                .stdin
                .take()
                .unwrap()
                .write_all(text.as_bytes())
                .unwrap();
        }
        let out = child.wait_with_output().expect("pacrat did not finish");
        Out {
            code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    /// The same run, with stderr as an `AF_UNIX` socket instead of a pipe.
    ///
    /// This is what a systemd unit hands a process: the journal is a socket,
    /// not a pipe or a file. It matters because a socket cannot be *reopened*
    /// through `/proc/self/fd` — `open("/dev/stderr")` returns ENXIO — so an
    /// implementation that reopens rather than duplicates the descriptor
    /// fails here and nowhere else, on the timer, in production, silently.
    /// A pipe cannot tell the two apart, which is the whole reason this
    /// helper exists.
    fn run_stderr_socket(&self, args: &[&str]) -> Out {
        use std::io::Read;
        use std::os::fd::OwnedFd;
        use std::os::unix::net::UnixStream;

        let (theirs, mut ours) = UnixStream::pair().expect("socketpair");
        let child = Command::new(env!("CARGO_BIN_EXE_pacrat"))
            .args(args)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &self.root)
            .env("DOTFILES_DIR", self.root.join("store"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .stdout(Stdio::piped())
            .stderr(Stdio::from(OwnedFd::from(theirs)))
            .stdin(Stdio::null())
            .spawn()
            .expect("pacrat did not run");

        // Drained on its own thread: a build transcript is larger than a
        // reader that only starts after `wait` would be safe with.
        let drain = std::thread::spawn(move || {
            let mut text = Vec::new();
            let _ = ours.read_to_end(&mut text);
            text
        });
        let out = child.wait_with_output().expect("pacrat did not finish");
        let stderr = drain.join().expect("the stderr reader panicked");
        Out {
            code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
        }
    }

    /// The ledger's reviewed commit — the thing an adoption moves.
    fn reviewed(&self) -> String {
        let text = fs::read_to_string(self.root.join("store/aur/sources.toml")).unwrap();
        text.lines()
            .find_map(|l| l.trim().strip_prefix("reviewed = "))
            .map(|v| v.trim_matches('"').to_string())
            .expect("no reviewed commit in the ledger")
    }

    /// The store tree's PKGBUILD — the bytes a build would use.
    fn store_pkgbuild(&self) -> String {
        fs::read_to_string(self.root.join(format!("store/aur/packages/{PKG}/PKGBUILD"))).unwrap()
    }

    fn decisions(&self) -> String {
        fs::read_to_string(self.root.join("store/aur/decisions.toml")).unwrap_or_default()
    }

    /// Package files that reached the repo directory.
    fn served(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(self.root.join("repo"))
            .unwrap()
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| n.contains(".pkg.tar"))
            .collect();
        names.sort();
        names
    }
}

/// A package that builds in about a second and needs nothing from anywhere.
fn pkgbuild(version: &str) -> String {
    format!(
        "pkgname={PKG}\n\
         pkgver={version}\n\
         pkgrel=1\n\
         pkgdesc='a fixture'\n\
         arch=('any')\n\
         license=('MIT')\n\
         options=(!debug)\n\
         package() {{\n\
         \x20 install -Dm644 /dev/null \"$pkgdir/usr/share/{PKG}/marker\"\n\
         }}\n"
    )
}

// ---------------------------------------------------------------- the gate

/// The whole loop, with nothing in the way: upstream moves, the grader says
/// 0, auto mode adopts it and builds it, and the exit code is a plain zero.
///
/// The only test that runs a real `makepkg`. Everything else stops at the
/// decision, because that is where the interesting failures are and a build
/// per scenario would make this suite too slow to run often.
#[test]
fn auto_adopts_a_clean_verdict_and_serves_it() {
    let sb = Sandbox::new("auto-proceed");
    let candidate = sb.publish("2.0");

    let out = sb.run(&["update", "--mode", "auto"]);
    assert_eq!(
        out.code,
        0,
        "a PROCEED in auto mode is a clean run:\n{}",
        out.text()
    );

    // The gate ran, visibly, and the store moved.
    assert!(out.stdout.contains("PROCEED"), "{}", out.stdout);
    assert!(out.stdout.contains("adopted"), "{}", out.stdout);
    assert_eq!(sb.reviewed(), candidate, "the ledger did not advance");
    assert!(
        sb.store_pkgbuild().contains("pkgver=2.0"),
        "the store tree is not the candidate"
    );

    // And it is served: a package file in the repo, by the name the
    // reviewed PKGBUILD declared.
    let served = sb.served();
    assert!(
        served
            .iter()
            .any(|n| n.starts_with(&format!("{PKG}-2.0-1"))),
        "nothing was served: {served:?}\n{}",
        out.text()
    );
}

/// WARN is the verdict the modes disagree about, so it is the one that
/// proves `--mode` is wired to anything at all.
#[test]
fn warn_holds_in_auto_and_a_pipe_cannot_answer_for_a_human() {
    let sb = Sandbox::new("warn");
    sb.configure(Some(2), 0);
    let candidate = sb.publish("2.0");
    let before = sb.reviewed();

    let auto = sb.run(&["update", "--mode", "auto"]);
    assert_eq!(
        auto.code,
        HELD,
        "a WARN must hold in auto:\n{}",
        auto.text()
    );
    assert!(auto.stdout.contains("WARN"), "{}", auto.stdout);
    assert_eq!(sb.reviewed(), before, "auto mode adopted a WARN");

    // semi *asks* — and a pipe is not a person. One `y` on stdin would
    // otherwise answer every question in a run, about packages the reader
    // has not seen, which is friction a cron line could satisfy.
    let semi = sb.run_with_stdin(&["update", "--mode", "semi"], Some("y\n"));
    assert_eq!(
        semi.code,
        HELD,
        "a pipe answered a prompt:\n{}",
        semi.text()
    );
    assert!(
        semi.stdout.contains("not a terminal"),
        "the run did not say why it refused to ask:\n{}",
        semi.stdout
    );
    assert_eq!(sb.reviewed(), before, "a piped y adopted a WARN");
    assert_ne!(sb.reviewed(), candidate);
}

/// The invariant, in every mode there is. No flag on this verb moves it —
/// the override lives on `adopt-update`, and the output says so rather than
/// leaving a reader hunting for one.
#[test]
fn block_holds_in_every_mode_and_names_the_door() {
    let sb = Sandbox::new("block");
    sb.configure(Some(4), 0);
    sb.publish("2.0");
    let before = sb.reviewed();

    for mode in ["auto", "semi", "manual"] {
        let out = sb.run(&["update", "--mode", mode]);
        assert_eq!(
            out.code,
            HELD,
            "a BLOCK must hold in {mode} mode:\n{}",
            out.text()
        );
        assert!(out.stdout.contains("BLOCK"), "{}", out.stdout);
        assert!(
            out.stdout.contains("--override-block"),
            "{mode} mode did not name the one door past a BLOCK:\n{}",
            out.stdout
        );
        assert_eq!(sb.reviewed(), before, "{mode} mode adopted a BLOCK");
    }
}

/// A grader that failed said nothing, and nothing is not permission.
#[test]
fn ungraded_holds_however_it_came_about() {
    // A grader that exits non-zero, and a host with no grader at all: two
    // roads to the same verdict, and neither may be read as proceed.
    for (tag, grade, code) in [("crashed", Some(0), 1), ("none", None, 0)] {
        let sb = Sandbox::new(&format!("ungraded-{tag}"));
        sb.configure(grade, code);
        sb.publish("2.0");
        let before = sb.reviewed();

        for mode in ["auto", "semi"] {
            let out = sb.run(&["update", "--mode", mode]);
            assert_eq!(
                out.code,
                HELD,
                "{tag}: ungraded must hold in {mode}:\n{}",
                out.text()
            );
            assert!(out.stdout.contains("UNGRADED"), "{}", out.stdout);
            assert_eq!(sb.reviewed(), before, "{tag}: {mode} adopted an ungraded");
        }
    }
}

/// The bytes that were read must be the bytes that would be adopted.
///
/// A grader that edits the tree it was asked to judge gets a PROCEED about
/// the file as it was, and the loop would otherwise adopt, build and serve
/// the file as the grader left it. The verdict is real; it is simply not
/// about the candidate any more, and nothing else in the pipeline would
/// notice — the cache recorded the pre-edit digest, and the adoption copies
/// whatever is on disk.
#[test]
fn a_grader_that_edits_the_tree_it_graded_gets_nothing_adopted() {
    let sb = Sandbox::new("tamper");
    sb.configure_tampering_grader();
    sb.publish("2.0");
    let before = sb.reviewed();

    let out = sb.run(&["update", "--mode", "auto"]);
    assert_eq!(
        out.code,
        HELD,
        "a tampered candidate must hold:\n{}",
        out.text()
    );
    assert!(
        out.stdout.contains("changed while it was being graded"),
        "the run did not say what it caught:\n{}",
        out.stdout
    );
    // Not adopted, not served, and the store still holds the reviewed bytes.
    assert_eq!(sb.reviewed(), before, "the tampered tree was adopted");
    assert!(!sb.store_pkgbuild().contains("appended by the grader"));
    assert!(sb.served().is_empty(), "a tampered build was served");
    // And the PROCEED is not reported as this candidate's verdict, because
    // it is not about these bytes.
    assert!(!out.stdout.contains("adopted   "), "{}", out.stdout);
}

/// A candidate a human already refused is not work. It is reported, it does
/// not hold the run, and it never reaches a grader.
#[test]
fn a_rejected_candidate_is_skipped_rather_than_re_raised() {
    let sb = Sandbox::new("rejected");
    sb.publish("2.0");
    let reject = sb.run(&["reject", PKG, "--note", "not this one"]);
    assert_eq!(reject.code, 0, "{}", reject.text());
    let runs = sb.grader_runs();

    let out = sb.run(&["update", "--mode", "auto"]);
    assert_eq!(
        out.code,
        0,
        "a decided question is not outstanding work:\n{}",
        out.text()
    );
    assert!(out.stdout.contains("skipped"), "{}", out.stdout);
    assert_eq!(sb.grader_runs(), runs, "a refused candidate was graded");
}

#[test]
fn a_store_at_its_reviewed_commit_is_a_clean_zero() {
    let sb = Sandbox::new("quiet");
    let out = sb.run(&["update", "--mode", "auto"]);
    assert_eq!(out.code, 0, "{}", out.text());
    assert!(out.stdout.contains("nothing pending"), "{}", out.stdout);
    assert_eq!(sb.grader_runs(), 0, "a current package was graded anyway");
}

/// An upstream that cannot be reached is reported and holds — and when it is
/// the *only* upstream, the run learned nothing and says so with a 1.
#[test]
fn an_unreachable_upstream_holds_and_a_blind_run_fails() {
    let sb = Sandbox::new("unreachable");
    fs::remove_dir_all(sb.root.join("upstream")).unwrap();

    let alone = sb.run(&["update", "--mode", "auto"]);
    assert_eq!(
        alone.code,
        1,
        "a run that could check nothing must fail:\n{}",
        alone.text()
    );
    assert!(alone.text().contains("learned nothing"), "{}", alone.text());

    // With something else to go on, the same row is merely a hold: the
    // report is still worth having, and 10 is what fetches a human.
    sb.write(
        "store/aur/sources.toml",
        &format!(
            "{}\n[packages.other]\nupstream = \"{}\"\nreviewed = \"{}\"\nrole = \"vendored\"\n",
            fs::read_to_string(sb.root.join("store/aur/sources.toml")).unwrap(),
            sb.upstream(),
            "0".repeat(40),
        ),
    );
    fs::create_dir_all(sb.root.join("upstream")).unwrap();
    sb.git(&["init", "-q", "-b", "main", "."]);
    sb.write("upstream/PKGBUILD", &pkgbuild("1.0"));
    sb.commit("initial");
    // `other` has no store tree, so it fails after the probe rather than
    // before it — still a hold, and still not the whole run.
    let out = sb.run(&["update", "--mode", "auto"]);
    assert_eq!(out.code, HELD, "{}", out.text());
}

// ------------------------------------------------------------- the grading

/// A grading is about bytes. Grading a *candidate* — bytes that are not in
/// the store yet — is what lets the loop decide before it adopts, and the
/// digest is what keeps that grading valid across the adoption that follows.
///
/// Both halves are checked here, because they are the same claim: the second
/// run must not pay for the grader again, and the grading the adoption
/// inherited must be the one `review` reads back afterwards.
#[test]
fn a_candidate_is_graded_once_and_the_grading_survives_its_adoption() {
    let sb = Sandbox::new("cache");
    sb.configure(Some(2), 0); // WARN: holds, so the candidate stays pending
    sb.publish("2.0");

    let first = sb.run(&["update", "--mode", "auto"]);
    assert_eq!(first.code, HELD, "{}", first.text());
    assert_eq!(sb.grader_runs(), 1, "the grader did not run");

    // Idempotent: the same candidate, the same bytes, no second invocation.
    let second = sb.run(&["update", "--mode", "auto"]);
    assert_eq!(second.code, HELD, "{}", second.text());
    assert_eq!(sb.grader_runs(), 1, "the cached grading was not reused");
    assert!(second.stdout.contains("cached"), "{}", second.stdout);

    // Adopt it by hand, which installs exactly the bytes that were graded.
    let candidate = sb.git(&["rev-parse", "HEAD"]);
    let adopt = sb.run(&["adopt-update", PKG, "--commit", &candidate, "--yes"]);
    assert_eq!(adopt.code, 0, "{}", adopt.text());
    assert_eq!(sb.reviewed(), candidate);

    // The grading was made of a candidate tree in /tmp and is read back
    // against the store tree, because the digest — not the path, and not the
    // commit id — is what it was filed under.
    let review = sb.run(&["review", PKG]);
    assert!(
        review.stdout.contains("1 on file for these bytes"),
        "the candidate's grading was lost in the adoption:\n{}",
        review.stdout
    );
    assert_eq!(sb.grader_runs(), 1);
}

// ------------------------------------------------------ the decision ledger

/// The one path past a BLOCK: a commit, a justification, and a permanent
/// record. Everything about it is friction, and the friction is checked.
#[test]
fn an_override_needs_a_commit_and_a_reason_and_leaves_a_record() {
    let sb = Sandbox::new("override");
    sb.configure(Some(4), 0);
    let candidate = sb.publish("2.0");
    let before = sb.reviewed();

    // No reason: refused on the command line, before anything is fetched.
    let no_reason = sb.run(&[
        "adopt-update",
        PKG,
        "--commit",
        &candidate,
        "--override-block",
    ]);
    assert_eq!(no_reason.code, 1);
    assert!(
        no_reason.stderr.contains("--reason"),
        "{}",
        no_reason.stderr
    );

    // No commit: refused too. An override says "I read *this*", and without
    // a commit it would vouch for whatever HEAD became.
    let no_commit = sb.run(&[
        "adopt-update",
        PKG,
        "--override-block",
        "--reason",
        "I read it",
    ]);
    assert_eq!(no_commit.code, 1);
    assert!(
        no_commit.stderr.contains("--commit"),
        "{}",
        no_commit.stderr
    );

    // A reason with no override records nothing and is refused rather than
    // quietly dropped — the quiet reading ends with somebody believing an
    // override is on file.
    let stray = sb.run(&["adopt-update", PKG, "--reason", "I read it", "--yes"]);
    assert_eq!(stray.code, 1);
    assert!(
        stray.stderr.contains("--override-block"),
        "{}",
        stray.stderr
    );
    assert_eq!(sb.reviewed(), before, "a refused invocation still adopted");
    assert!(sb.decisions().is_empty(), "a refusal wrote a decision");

    // Nothing has graded this candidate yet, so the verdict is UNGRADED —
    // and an override of a BLOCK that is not there is refused rather than
    // quietly performed. Someone reaching for this flag believes they are
    // looking at a BLOCK; they are not, and acting on a model that has just
    // been shown to be wrong is the thing the tool exists to prevent.
    let wrong_door = sb.run(&[
        "adopt-update",
        PKG,
        "--commit",
        &candidate,
        "--override-block",
        "--reason",
        "I think this is blocked",
        "--yes",
    ]);
    assert_eq!(wrong_door.code, 1, "{}", wrong_door.text());
    assert!(
        wrong_door.stderr.contains("nothing to override")
            && wrong_door.stderr.contains("UNGRADED"),
        "{}",
        wrong_door.stderr
    );
    assert_eq!(sb.reviewed(), before, "the wrong door still adopted");
    assert!(sb.decisions().is_empty(), "a refusal wrote a decision");

    // The loop is what grades a candidate, so this is the real sequence: the
    // timer holds at BLOCK, and the human comes along afterwards and decides
    // to accept it.
    let held = sb.run(&["update", "--mode", "auto"]);
    assert_eq!(held.code, HELD, "{}", held.text());

    // And the real thing.
    let reason = "upstream's new post_install is the packaging fix I asked for";
    let out = sb.run(&[
        "adopt-update",
        PKG,
        "--commit",
        &candidate,
        "--override-block",
        "--reason",
        reason,
        "--yes",
    ]);
    assert_eq!(out.code, 0, "{}", out.text());
    assert!(out.stdout.contains("OVERRIDE"), "quietly:\n{}", out.stdout);
    assert_eq!(sb.reviewed(), candidate, "the override did not adopt");

    let file = sb.decisions();
    assert!(file.contains("override-block"), "{file}");
    assert!(file.contains(reason), "{file}");
    assert!(file.contains(&candidate), "{file}");
    assert!(file.contains("grade = 4"), "the overridden grade: {file}");

    // And it is readable as a decision, not just as a file.
    let listed = sb.run(&["decisions"]);
    assert_eq!(listed.code, 0, "{}", listed.text());
    assert!(listed.stdout.contains(PKG), "{}", listed.stdout);
    assert!(
        listed.stdout.contains("override-block"),
        "{}",
        listed.stdout
    );
    assert!(listed.stdout.contains("packaging fix"), "{}", listed.stdout);
}

/// The loop has no override, and that is a property of the *binary*, not
/// only of its documentation.
#[test]
fn the_update_loop_offers_no_override_flag() {
    let sb = Sandbox::new("no-override");
    sb.configure(Some(4), 0);
    sb.publish("2.0");

    let out = sb.run(&[
        "update",
        "--mode",
        "auto",
        "--override-block",
        "--reason",
        "no",
    ]);
    assert_ne!(out.code, 0);
    assert!(
        out.stderr.contains("unexpected argument") || out.stderr.contains("--override-block"),
        "the loop accepted an override flag:\n{}",
        out.stderr
    );

    let empty = sb.run(&["decisions"]);
    assert!(
        empty.stdout.contains("no decisions recorded"),
        "{}",
        empty.stdout
    );
}

#[test]
fn an_empty_ledger_lists_as_empty() {
    let sb = Sandbox::new("no-decisions");
    let out = sb.run(&["decisions"]);
    assert_eq!(out.code, 0, "{}", out.text());
    assert!(
        out.stdout.contains("no decisions recorded"),
        "{}",
        out.stdout
    );
}

// -------------------------------------------------------------- the machine

/// `--format json`: stdout is one object and nothing else, whatever the loop
/// printed on the way. A timer that has to sift a transcript out of its
/// answer does not have a machine format.
#[test]
fn the_json_report_is_the_whole_of_stdout_and_says_what_happened() {
    let sb = Sandbox::new("json");
    sb.configure(Some(4), 0);
    let candidate = sb.publish("2.0");

    let out = sb.run(&["update", "--mode", "auto", "--format", "json"]);
    assert_eq!(out.code, HELD, "{}", out.text());

    let report: serde_json::Value = serde_json::from_str(&out.stdout)
        .unwrap_or_else(|e| panic!("stdout is not JSON ({e}):\n{}", out.stdout));
    assert_eq!(report["mode"], "auto");
    assert_eq!(report["exit"], HELD);
    assert_eq!(report["holds"], 1);
    assert_eq!(report["reached"], 1);
    assert_eq!(report["served"].as_array().unwrap().len(), 0);

    let pkg = &report["packages"][0];
    assert_eq!(pkg["package"], PKG);
    assert_eq!(pkg["candidate"], candidate);
    assert_eq!(pkg["verdict"], "BLOCK");
    assert_eq!(pkg["grade"], 4);
    assert_eq!(pkg["outcome"], "held-block");

    // The transcript still exists — the rule is about *where* it goes, never
    // whether the calls are visible.
    assert!(out.stderr.contains("run       git"), "{}", out.stderr);
    assert!(out.stderr.contains("BLOCK"), "{}", out.stderr);
}

/// The same run under the descriptor a systemd unit actually provides: a
/// socket. An implementation that reopens `/dev/stderr` instead of
/// duplicating it gets ENXIO here, falls back to inheriting, and puts
/// makepkg's whole transcript in front of the object — in production, on the
/// timer, and nowhere a pipe-based test would ever see it.
#[test]
fn journald_style_socket_stderr_does_not_leak_the_build_into_the_json() {
    let sb = Sandbox::new("json-socket");
    sb.publish("2.0");

    let out = sb.run_stderr_socket(&["update", "--mode", "auto", "--format", "json"]);
    assert_eq!(out.code, 0, "{}", out.text());
    let report: serde_json::Value = serde_json::from_str(&out.stdout)
        .unwrap_or_else(|e| panic!("stdout is not one JSON object ({e}):\n{}", out.stdout));
    assert_eq!(report["served"][0], PKG);
    // The transcript went somewhere, and that somewhere is the socket.
    assert!(
        out.stderr.contains("makepkg") || out.stderr.contains("Finished making"),
        "the build transcript did not reach the socket:\n{}",
        out.stderr
    );
}

/// A clean run in JSON, including the build stage: makepkg's own output is
/// noisy and inherits a descriptor of its own, so this is the case where a
/// stray write would corrupt the object.
#[test]
fn a_building_run_still_answers_with_one_json_object() {
    let sb = Sandbox::new("json-build");
    let candidate = sb.publish("2.0");

    let out = sb.run(&["update", "--mode", "auto", "--format", "json"]);
    assert_eq!(out.code, 0, "{}", out.text());
    let report: serde_json::Value = serde_json::from_str(&out.stdout)
        .unwrap_or_else(|e| panic!("makepkg leaked into stdout ({e}):\n{}", out.stdout));
    assert_eq!(report["packages"][0]["outcome"], "adopted");
    assert_eq!(report["served"][0], PKG);
    assert_eq!(report["adopted"][0], PKG);
    assert_eq!(sb.reviewed(), candidate);
}

// ------------------------------------------------------------------ hygiene

/// Nothing in this suite may touch a real store. Cheap to assert, and the
/// assertion is the reason the sandbox clears the environment.
#[test]
fn the_fixture_store_is_the_only_store() {
    let sb = Sandbox::new("hygiene");
    let out = sb.run(&["update", "--mode", "auto"]);
    assert!(
        out.stdout.contains(&sb.root.display().to_string()),
        "the run did not name the sandbox store:\n{}",
        out.stdout
    );
    assert!(
        !out.text().contains("/home/"),
        "a run referred to a home directory outside the sandbox:\n{}",
        out.text()
    );
    let _: &Path = &sb.root;
}
