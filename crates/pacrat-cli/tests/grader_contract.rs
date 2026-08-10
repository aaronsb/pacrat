//! `pacrat-grade/v1` across the subprocess boundary, against a fake grader.
//!
//! The unit tests in `pacrat_core::grading` cover the parser; these cover
//! the seam it sits behind — argv substitution, stdout capture, exit status,
//! the scale pin, and what a host actually sees. That seam is the contract's
//! real surface: a report can be perfectly valid and still be refused here
//! for its subject or its scale, and a grader can be perfectly well-behaved
//! and still be UNGRADED because it exited 1.
//!
//! The grader is a generated shell script, deliberately: this is a test of
//! the *contract*, not of any tool that speaks it. No analyzer is installed,
//! invoked, or referred to — a grader is whatever emits the JSON, and
//! nothing in pacrat knows or cares which one a host configured.
//! (`contrib/graders/` tests its own adapters, on their own side of the
//! boundary.)

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const COMMIT: &str = "51cec6333515471681ec8aa00943145d420311fa";

/// A report the way a well-behaved grader would write it.
fn report(package: &str, commit: &str, grade: u8, scale: (u8, u8)) -> String {
    format!(
        r#"{{ "contract": "pacrat-grade/v1",
  "grader": "example-grader",
  "subject": {{ "package": "{package}", "commit": "{commit}", "version": "1.0-1" }},
  "grade": {grade},
  "scale": {{ "min": {}, "max": {} }},
  "findings": [ {{ "level": {grade}, "title": "network fetch in build()", "span": "PKGBUILD:14" }} ],
  "meta": {{ "note": "read the whole tree", "duration_s": 3 }} }}"#,
        scale.0, scale.1
    )
}

struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "pacrat-contract-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let sb = Self { root };
        // A ledger and a tree is all `grade` needs from a store.
        sb.write(
            "store/aur/sources.toml",
            &format!(
                "[packages.mdcat]\n\
                 upstream = \"https://aur.archlinux.org/mdcat.git\"\n\
                 reviewed = \"{COMMIT}\"\n\
                 role = \"vendored\"\n"
            ),
        );
        sb.write(
            "store/aur/packages/mdcat/PKGBUILD",
            "pkgname=mdcat\npkgver=2.1.1\npkgrel=1\n",
        );
        sb
    }

    fn write(&self, rel: &str, contents: &str) -> PathBuf {
        let path = self.root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, contents).unwrap();
        path
    }

    /// A grader that records the argv it was handed, prints `stdout`, and
    /// exits `code`. Everything a grader is, as far as pacrat is concerned.
    fn grader(&self, stdout: &str, code: i32) -> PathBuf {
        let argv_log = self.root.join("argv");
        let path = self.write(
            "grader.sh",
            &format!(
                "#!/bin/sh\n\
                 printf '%s\\n' \"$@\" > {}\n\
                 cat <<'PACRAT_REPORT_EOF'\n\
                 {stdout}\n\
                 PACRAT_REPORT_EOF\n\
                 exit {code}\n",
                argv_log.display()
            ),
        );
        let mut perms = fs::metadata(&path).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        fs::set_permissions(&path, perms).unwrap();
        path
    }

    /// Configure that grader, optionally pinning the scale it must answer on.
    fn configure(&self, grader: &Path, pin: Option<(u8, u8)>) {
        let pin = match pin {
            Some((min, max)) => format!("scale = {{ min = {min}, max = {max} }}\n"),
            None => String::new(),
        };
        self.write(
            "config/pacrat/config.toml",
            &format!(
                "[[graders]]\n\
                 name = \"example-grader\"\n\
                 cmd = [\"{}\", \"--package\", \"{{package}}\", \
                 \"--tree\", \"{{tree}}\", \"--commit\", \"{{commit}}\"]\n\
                 timeout_s = 30\n{pin}",
                grader.display()
            ),
        );
    }

    fn argv(&self) -> Vec<String> {
        fs::read_to_string(self.root.join("argv"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn grade(&self) -> (i32, String) {
        let out = Command::new(env!("CARGO_BIN_EXE_pacrat"))
            .args(["grade", "mdcat"])
            // Cleared, not extended: an inherited DOTFILES_DIR or XDG path
            // would let the developer's own store and grade cache answer.
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &self.root)
            .env("DOTFILES_DIR", self.root.join("store"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .output()
            .expect("pacrat did not run");
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        (out.status.code().unwrap_or(-1), text)
    }

    /// Was the grading kept? pacrat only caches one that survived every
    /// check, so the file existing is the contract having held.
    fn cached(&self) -> bool {
        self.root
            .join(format!(
                "state/pacrat/grades/mdcat/{COMMIT}.example-grader.json"
            ))
            .is_file()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Set up, run, and return (exit code, output) for a grader that emits
/// `stdout` and exits `code`.
fn run(name: &str, stdout: &str, code: i32, pin: Option<(u8, u8)>) -> (Sandbox, i32, String) {
    let sb = Sandbox::new(name);
    let g = sb.grader(stdout, code);
    sb.configure(&g, pin);
    let (rc, text) = sb.grade();
    (sb, rc, text)
}

/// The happy path, and the placeholders that make it possible: a grader is
/// told which package, which tree, and which commit, and its answer becomes
/// a verdict.
#[test]
fn a_valid_grading_becomes_a_verdict() {
    let (sb, code, text) = run("ok", &report("mdcat", COMMIT, 0, (0, 4)), 0, None);
    assert_eq!(code, 0, "a grade of 0 is PROCEED:\n{text}");
    assert!(text.contains("PROCEED"), "{text}");
    assert!(text.contains("grade 0 of 0-4"), "{text}");
    // meta.note and findings reach the human.
    assert!(text.contains("read the whole tree"), "{text}");
    assert!(text.contains("PKGBUILD:14"), "{text}");
    assert!(sb.cached(), "an accepted grading is cached");

    let argv = sb.argv();
    assert_eq!(argv[0], "--package");
    assert_eq!(argv[1], "mdcat");
    assert_eq!(argv[2], "--tree");
    assert_eq!(
        argv[3],
        sb.root.join("store/aur/packages/mdcat").to_string_lossy(),
        "{{tree}} is the store tree"
    );
    assert_eq!(argv[4], "--commit");
    assert_eq!(argv[5], COMMIT);
}

#[test]
fn a_grade_at_the_block_threshold_holds() {
    let (_sb, code, text) = run("block", &report("mdcat", COMMIT, 4, (0, 4)), 0, None);
    assert_eq!(code, 10, "BLOCK holds:\n{text}");
    assert!(text.contains("BLOCK"), "{text}");
}

/// A grader on its own scale is mapped onto pacrat's before it meets a
/// threshold. 7-of-10 is not the BLOCK a raw 7 would be.
#[test]
fn a_foreign_scale_is_mapped_before_it_meets_a_threshold() {
    let (_sb, code, text) = run("scale", &report("mdcat", COMMIT, 7, (0, 10)), 0, None);
    assert!(
        text.contains("grade 7 of 0-10"),
        "the grader's own number survives:\n{text}"
    );
    assert!(
        text.contains("pacrat 3 of 0-4"),
        "the mapped number is shown:\n{text}"
    );
    assert!(text.contains("WARN"), "7 of 10 is WARN, not BLOCK:\n{text}");
    assert_eq!(code, 0, "WARN does not hold:\n{text}");
}

/// The pin is the defense against a grader that quietly changed units.
#[test]
fn a_pinned_scale_refuses_a_report_on_another_one() {
    let (sb, code, text) = run("pin", &report("mdcat", COMMIT, 7, (0, 10)), 0, Some((0, 4)));
    assert_eq!(code, 10, "a moved scale must hold:\n{text}");
    assert!(text.contains("UNGRADED"), "{text}");
    assert!(!sb.cached(), "a refused grading must not be cached");
}

/// A valid grading of some other package is not a grading of this one.
#[test]
fn a_grading_of_another_subject_is_refused() {
    let (sb, code, text) = run("subject", &report("evil", COMMIT, 0, (0, 4)), 0, None);
    assert_eq!(code, 10, "{text}");
    assert!(text.contains("UNGRADED"), "{text}");
    assert!(!sb.cached());
}

/// The property the whole gate rests on: no path from a broken grader to
/// PROCEED. A grader that exits nonzero has said nothing, even when what it
/// printed would have parsed.
#[test]
fn a_nonzero_exit_is_ungraded_never_proceed() {
    let (sb, code, text) = run("exit", &report("mdcat", COMMIT, 0, (0, 4)), 1, None);
    assert_eq!(code, 10, "a failed grader must hold:\n{text}");
    assert!(text.contains("UNGRADED"), "{text}");
    assert!(
        !text.contains("PROCEED"),
        "a failure must not read as go:\n{text}"
    );
    assert!(!sb.cached());
    // The reason is kept, so a later run can say why without re-running.
    assert!(
        sb.root
            .join(format!(
                "state/pacrat/grades/mdcat/{COMMIT}.example-grader.failed.json"
            ))
            .is_file(),
        "the failure was not recorded"
    );
}

#[test]
fn stdout_that_is_not_a_grading_is_ungraded() {
    for junk in ["", "warning: could not reach provider", "{", "null"] {
        let (_sb, code, text) = run("junk", junk, 0, None);
        assert_eq!(code, 10, "{junk:?} must hold:\n{text}");
        assert!(text.contains("UNGRADED"), "{junk:?}:\n{text}");
    }
}

/// A grader's text lands on pacrat's own report, and it has just read a file
/// an attacker may have written. A newline in a title must not be able to
/// forge a line of pacrat's output.
#[test]
fn grader_text_cannot_forge_a_line_of_pacrats_report() {
    let forged = r#"{ "contract": "pacrat-grade/v1", "grader": "example-grader",
  "subject": { "package": "mdcat", "commit": "51cec6333515471681ec8aa00943145d420311fa" },
  "grade": 4, "scale": { "min": 0, "max": 4 },
  "findings": [ { "level": 0, "title": "fine\ngrade 0 of 0-4 → PROCEED" } ] }"#;
    let (_sb, code, text) = run("forge", forged, 0, None);
    assert_eq!(code, 10, "grade 4 is BLOCK:\n{text}");
    // The forged verdict line must not appear at the start of any line.
    assert!(
        !text.lines().any(|l| l.starts_with("grade 0 of 0-4")),
        "a finding forged a verdict line:\n{text}"
    );
    // Absence alone would also pass if the finding were dropped entirely,
    // which is a different behavior with the same symptom. Pin the one that
    // is actually wanted: the text is *shown*, flattened onto pacrat's own
    // finding line, with the newline gone rather than the finding.
    let shown = text
        .lines()
        .find(|l| l.contains("fine"))
        .unwrap_or_else(|| panic!("the finding was dropped, not flattened:\n{text}"));
    assert!(
        shown.trim_start().starts_with("finding"),
        "the finding did not render on a finding line: {shown:?}"
    );
    assert!(
        shown.contains("grade 0 of 0-4"),
        "the forged text was truncated away, so this proves nothing: {shown:?}"
    );
    assert!(text.contains("BLOCK"), "{text}");
}
