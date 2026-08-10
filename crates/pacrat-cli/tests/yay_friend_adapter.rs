//! The yay-friend adapter, end to end through pacrat's own grade engine.
//!
//! `contrib/graders/test-yay-friend-grade.sh` tests the adapter's own
//! behavior in detail; this tests the seam it exists for — that what the
//! adapter writes is a grading *pacrat* accepts. Nothing else proves that,
//! because the contract is enforced on the reader's side: a report that
//! parses in a shell test can still be rejected here for its contract
//! string, its scale pin, or its subject.
//!
//! Hermetic and free: yay-friend is never run. The whole environment is
//! redirected — store, config, state, and yay-friend's own XDG cache, which
//! is fabricated with a captured cache entry. The one thing not faked is
//! pacrat, which is the point.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const COMMIT: &str = "51cec6333515471681ec8aa00943145d420311fa";

/// A yay-friend cache entry, in the shape it really writes them.
fn cache_entry(package: &str, commit: &str, entropy: u8) -> String {
    format!(
        r#"{{
  "cache_metadata": {{
    "commit_hash": "{commit}",
    "package_name": "{package}",
    "cache_version": "1.0",
    "yay_friend_version": "1.0.0"
  }},
  "analysis": {{
    "package_name": "{package}",
    "overall_entropy": {entropy},
    "overall_level": {entropy},
    "findings": [
      {{ "type": "source_analysis", "entropy": 2, "severity": 2,
         "description": "Source downloaded from official GNU FTP server",
         "line_number": 12, "context": "source=(...)",
         "suggestion": "Legitimate source location" }}
    ],
    "summary": "Clean package.",
    "recommendation": "PROCEED",
    "provider": "claude"
  }}
}}"#
    )
}

struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "pacrat-adapter-{name}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let sb = Self { root };

        // The store: a ledger and a tree, which is all `grade` needs.
        sb.write(
            "store/aur/sources.toml",
            &format!(
                "[packages.hello]\n\
                 upstream = \"https://aur.archlinux.org/hello.git\"\n\
                 reviewed = \"{COMMIT}\"\n\
                 role = \"vendored\"\n"
            ),
        );
        sb.write(
            "store/aur/packages/hello/PKGBUILD",
            "pkgname=hello\npkgver=2.12.1\npkgrel=1\n",
        );
        sb.write(
            "store/aur/packages/hello/.SRCINFO",
            "pkgbase = hello\n\tpkgver = 2.12.1\n\tpkgrel = 1\n",
        );

        // The adapter's whole PATH: bash for its shebang, jq for the
        // translation, and deliberately no yay-friend. On a developer's own
        // machine yay-friend *is* installed, and the cache-miss test would
        // otherwise call a real LLM provider — slow, billed, and dependent
        // on the network for a result the test does not want.
        let bin = sb.root.join("bin");
        fs::create_dir_all(&bin).unwrap();
        for prog in ["bash", "jq"] {
            let found = locate(prog).unwrap_or_else(|| panic!("{prog} is not on PATH"));
            std::os::unix::fs::symlink(found, bin.join(prog)).unwrap();
        }
        sb
    }

    fn write(&self, rel: &str, contents: &str) -> PathBuf {
        let path = self.root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, contents).unwrap();
        path
    }

    /// Point the grader at the adapter in this repo, with the scale pinned
    /// exactly as the documented stanza does — so a report on any other
    /// scale is a failure here rather than a quietly rescaled grade.
    fn configure_adapter(&self) {
        let adapter = repo_root().join("contrib/graders/yay-friend-grade");
        assert!(
            adapter.is_file(),
            "adapter missing at {}",
            adapter.display()
        );
        self.write(
            "config/pacrat/config.toml",
            &format!(
                "[[graders]]\n\
                 name = \"yay-friend\"\n\
                 cmd = [\"{}\", \"--package\", \"{{package}}\", \
                 \"--tree\", \"{{tree}}\", \"--commit\", \"{{commit}}\"]\n\
                 timeout_s = 60\n\
                 scale = {{ min = 0, max = 4 }}\n",
                adapter.display()
            ),
        );
    }

    fn grade(&self, package: &str) -> (i32, String) {
        let out = Command::new(env!("CARGO_BIN_EXE_pacrat"))
            .args(["grade", package])
            // Cleared rather than added to: an inherited XDG_DATA_HOME or
            // DOTFILES_DIR would let the developer's own store and real
            // yay-friend cache answer for the fixture.
            .env_clear()
            .env("PATH", self.root.join("bin"))
            .env("HOME", &self.root)
            .env("DOTFILES_DIR", self.root.join("store"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .output()
            .expect("pacrat did not run");
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        (out.status.code().unwrap_or(-1), text)
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn repo_root() -> PathBuf {
    // crates/pacrat-cli → repo root.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("the manifest is two levels below the repo root")
        .to_path_buf()
}

/// The first `prog` on PATH. Used instead of shelling out to `which`, which
/// is not everywhere, and instead of hardcoding /usr/bin, which is not true
/// everywhere either.
fn locate(prog: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(prog))
        .find(|p| p.is_file())
}

/// jq is the adapter's one dependency. Without it the adapter exits nonzero
/// and every assertion below would fail for a reason that has nothing to do
/// with pacrat, so say so and stop rather than reporting a false failure.
fn have_jq() -> bool {
    if locate("jq").is_some() {
        return true;
    }
    eprintln!("skipping: jq is not installed (the adapter's one dependency)");
    false
}

/// The path the whole adapter exists for: a yay-friend cache entry for the
/// reviewed commit becomes a grading pacrat accepts, records, and turns into
/// a verdict — with no yay-friend, no provider, and no network.
#[test]
fn a_cached_analysis_becomes_a_pacrat_verdict() {
    if !have_jq() {
        return;
    }
    let sb = Sandbox::new("hit");
    sb.configure_adapter();
    sb.write(
        &format!("data/yay-friend/cache/hello/{COMMIT}.json"),
        &cache_entry("hello", COMMIT, 0),
    );

    let (code, text) = sb.grade("hello");
    assert_eq!(code, 0, "expected a clean exit:\n{text}");
    assert!(
        text.contains("grade 0 of 0-4"),
        "the engine did not read the grade:\n{text}"
    );
    assert!(text.contains("PROCEED"), "expected PROCEED:\n{text}");
    // meta.note is the summary the adapter lifts out of the analysis.
    assert!(
        text.contains("Clean package."),
        "the summary did not reach the report:\n{text}"
    );
    assert!(
        text.contains("Source downloaded from official GNU FTP server"),
        "the finding did not reach the report:\n{text}"
    );

    // Accepted means cached: pacrat only writes a grading that survived
    // every check, so the file existing is the contract having held.
    let cached = sb.root.join(format!(
        "state/pacrat/grades/hello/{COMMIT}.yay-friend.json"
    ));
    assert!(
        cached.is_file(),
        "no grading recorded at {}",
        cached.display()
    );
    let recorded = fs::read_to_string(&cached).unwrap();
    assert!(
        recorded.contains("pacrat-grade/v1") && recorded.contains("yay-friend-grade/v1"),
        "the recorded grading is not the adapter's:\n{recorded}"
    );
}

/// A high entropy has to travel the same path and come out as a hold — the
/// translation is only worth anything if it can also say "no".
#[test]
fn a_critical_analysis_blocks() {
    if !have_jq() {
        return;
    }
    let sb = Sandbox::new("block");
    sb.configure_adapter();
    sb.write(
        &format!("data/yay-friend/cache/hello/{COMMIT}.json"),
        &cache_entry("hello", COMMIT, 4),
    );

    let (code, text) = sb.grade("hello");
    assert_eq!(code, 10, "a BLOCK must hold:\n{text}");
    assert!(text.contains("BLOCK"), "expected BLOCK:\n{text}");
}

/// No cache entry and no yay-friend on PATH: the adapter fails, and a
/// grader that fails is UNGRADED, which holds. This is the outcome on a host
/// where yay-friend is not installed, and it must not read as PROCEED.
#[test]
fn a_grader_that_cannot_answer_holds() {
    if !have_jq() {
        return;
    }
    let sb = Sandbox::new("miss");
    sb.configure_adapter();
    // No data/yay-friend/cache at all.

    let (code, text) = sb.grade("hello");
    assert_eq!(code, 10, "an ungraded package must hold:\n{text}");
    assert!(text.contains("UNGRADED"), "expected UNGRADED:\n{text}");
    // The failure is recorded with its reason, not swallowed.
    let failed = sb.root.join(format!(
        "state/pacrat/grades/hello/{COMMIT}.yay-friend.failed.json"
    ));
    assert!(
        failed.is_file(),
        "no failure recorded at {}",
        failed.display()
    );
}
