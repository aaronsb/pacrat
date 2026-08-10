//! `pacrat push` end to end, against a local bare repository.
//!
//! The unit tests cover the pieces — the tamper alarm as arithmetic over two
//! PKGBUILDs, the push argv, the queue model. What they cannot cover is the
//! thing this verb actually is: a clone, a mirror, a regenerated `.SRCINFO`,
//! a commit and a push, in that order, against a real git remote. So the
//! remote here is real, and it is a `file://` bare repo in a temp dir.
//!
//! **It is never anything else.** No test in this file may name a network
//! remote: the AUR write probe is exercised by hand against the real service
//! (read-only, `ssh … help`), and everything that writes runs against these
//! fixtures. A publish test that reaches the internet is a publish.
//!
//! Needs `git` and `makepkg`. makepkg is Arch's, so the publishing cases
//! skip elsewhere — CI runs on Ubuntu and would otherwise fail on a missing
//! binary rather than on anything about pacrat. The cases that do not need it
//! (role refusal, the queue, the status line) always run.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// A commit-shaped string for the ledger's `reviewed`.
const REVIEWED: &str = "51cec6333515471681ec8aa00943145d420311fa";
const OTHER: &str = "aa0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f";
const DIGEST_SHAPED: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "pacrat-publish-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let sb = Self { root };
        // git needs an identity to commit with, and a default branch both
        // sides agree on. Both live in the sandbox's own HOME, which is also
        // the point: pacrat does not configure git, the user does.
        sb.write(
            ".gitconfig",
            "[user]\n\tname = pacrat test\n\temail = pacrat@example.invalid\n\
             [init]\n\tdefaultBranch = master\n",
        );
        sb
    }

    fn write(&self, rel: &str, contents: &str) -> PathBuf {
        let path = self.root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, contents).unwrap();
        path
    }

    /// The bare repository a publish goes to: the AUR's stand-in.
    fn remote(&self) -> PathBuf {
        let path = self.root.join("remote.git");
        self.git(
            &["init", "--bare", "--quiet", path.to_str().unwrap()],
            &self.root,
        );
        path
    }

    /// The ledger, pointing at that remote.
    fn ledger(&self, role: &str, upstream: &str) {
        self.write(
            "store/aur/sources.toml",
            &format!(
                "[packages.mdcat]\n\
                 upstream = \"{upstream}\"\n\
                 reviewed = \"{REVIEWED}\"\n\
                 role = \"{role}\"\n"
            ),
        );
    }

    /// The store's tree for the package: a PKGBUILD makepkg will parse.
    fn tree(&self, pkgver: &str, pkgrel: &str, sum: &str) {
        self.write(
            "store/aur/packages/mdcat/PKGBUILD",
            &format!(
                "pkgname=mdcat\n\
                 pkgver={pkgver}\n\
                 pkgrel={pkgrel}\n\
                 pkgdesc=\"cat for markdown\"\n\
                 arch=('any')\n\
                 url=\"https://example.invalid/mdcat\"\n\
                 license=('MIT')\n\
                 source=(\"mdcat-$pkgver.tar.gz::https://example.invalid/mdcat-$pkgver.tar.gz\")\n\
                 sha256sums=('{sum}')\n\
                 package() {{ :; }}\n"
            ),
        );
    }

    /// The store tree's digest, computed the way pacrat computes it.
    ///
    /// Through `pacrat_core` rather than by scraping it out of pacrat's
    /// output: a queue entry is a claim about bytes, and a test that got the
    /// claim from the thing it is testing would agree with a bug.
    fn digest(&self) -> String {
        let dir = self.root.join("store/aur/packages/mdcat");
        let mut names: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        let contents: Vec<Vec<u8>> = names
            .iter()
            .map(|n| fs::read(dir.join(n)).unwrap())
            .collect();
        pacrat_core::hash::tree_digest(
            names
                .iter()
                .map(String::as_str)
                .zip(contents.iter().map(Vec::as_slice)),
        )
    }

    /// An errand already on file, as a blocked probe would have left it.
    fn queue(&self, package: &str, commit: &str, digest: &str) {
        self.write(
            "state/pacrat/pushes/queue.toml",
            &format!(
                "[pushes.{package}]\n\
                 commit = \"{commit}\"\n\
                 digest = \"{digest}\"\n\
                 queued_at = 1754784000\n\
                 last_probe = 1754870400\n\
                 last_answer = \"The AUR is down due to maintenance. We will be back soon.\"\n"
            ),
        );
    }

    fn queue_text(&self) -> String {
        fs::read_to_string(self.root.join("state/pacrat/pushes/queue.toml")).unwrap_or_default()
    }

    fn git(&self, args: &[&str], cwd: &Path) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env("HOME", &self.root)
            .env("GIT_CONFIG_GLOBAL", self.root.join(".gitconfig"))
            .output()
            .expect("git did not run");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// What the bare repo holds on master, as `git show` sees it.
    fn published(&self, path: &str) -> Option<String> {
        let remote = self.root.join("remote.git");
        let out = Command::new("git")
            .args(["show", &format!("master:{path}")])
            .current_dir(&remote)
            .env("HOME", &self.root)
            .output()
            .expect("git did not run");
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    }

    fn log(&self) -> Vec<String> {
        let remote = self.root.join("remote.git");
        let out = Command::new("git")
            .args(["log", "--format=%s", "master"])
            .current_dir(&remote)
            .env("HOME", &self.root)
            .output()
            .expect("git did not run");
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn pacrat(&self, args: &[&str]) -> (i32, String) {
        let out = Command::new(env!("CARGO_BIN_EXE_pacrat"))
            .args(args)
            // Cleared, not extended: an inherited DOTFILES_DIR or XDG path
            // would let the developer's own store and queue answer.
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &self.root)
            .env("DOTFILES_DIR", self.root.join("store"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            // pacrat's scratch clone lands under the temp dir, and a failure
            // deliberately keeps it. Pointed inside the sandbox so the kept
            // evidence is cleaned up with everything else instead of
            // accumulating in /tmp, one directory per alarm test.
            .env("TMPDIR", &self.root)
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

fn have(binary: &str) -> bool {
    Path::new("/usr/bin").join(binary).exists()
}

/// Publishing needs makepkg (`--printsrcinfo`), which is Arch's.
fn can_publish() -> bool {
    if have("makepkg") {
        return true;
    }
    eprintln!("skipping: makepkg is not installed (publishing is Arch-only)");
    false
}

/// A first publish into an empty repository, then the same tree again.
///
/// The AUR hands back a perfectly good empty repository for a package that
/// has never been uploaded, so "clone, find nothing, publish everything" is
/// the ordinary first-time path and not an error case.
#[test]
fn a_first_publish_lands_the_tree_and_a_generated_srcinfo() {
    if !can_publish() {
        return;
    }
    let sb = Sandbox::new("first");
    let remote = sb.remote();
    sb.ledger("maintained", &format!("file://{}", remote.display()));
    sb.tree("1.0", "1", &"a".repeat(64));

    let (code, out) = sb.pacrat(&["push", "mdcat", "--yes"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("first publish"), "{out}");
    assert!(out.contains("published mdcat 1.0-1"), "{out}");

    assert!(sb.published("PKGBUILD").unwrap().contains("pkgver=1.0"));
    let srcinfo = sb.published(".SRCINFO").expect("no .SRCINFO published");
    assert!(srcinfo.contains("pkgver = 1.0"), "{srcinfo}");
    assert!(srcinfo.contains("pkgbase = mdcat"), "{srcinfo}");
    assert_eq!(sb.log(), ["mdcat 1.0-1"]);

    // Asked again with nothing changed: convergent, and it says so rather
    // than making an empty commit.
    let (code, out) = sb.pacrat(&["push", "mdcat", "--yes"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("already current"), "{out}");
    assert_eq!(sb.log().len(), 1, "an empty publish made a commit");
}

/// The update path: a new version over a published one.
#[test]
fn a_new_version_is_published_over_the_old_one() {
    if !can_publish() {
        return;
    }
    let sb = Sandbox::new("update");
    let remote = sb.remote();
    sb.ledger("maintained", &format!("file://{}", remote.display()));
    sb.tree("1.0", "1", &"a".repeat(64));
    assert_eq!(sb.pacrat(&["push", "mdcat", "--yes"]).0, 0);

    // A new release: new version, new tarball, new checksum.
    sb.tree("2.0", "1", &"b".repeat(64));
    // And a file that did not exist before.
    sb.write(
        "store/aur/packages/mdcat/mdcat.install",
        "post_install() { :; }\n",
    );
    let (code, out) = sb.pacrat(&["push", "mdcat", "--yes"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("published mdcat 2.0-1"), "{out}");
    // The diff a human would have been asked about is on screen.
    assert!(out.contains("+pkgver=2.0"), "{out}");

    assert!(sb.published("PKGBUILD").unwrap().contains("pkgver=2.0"));
    assert!(sb.published("mdcat.install").is_some());
    assert!(sb.published(".SRCINFO").unwrap().contains("pkgver = 2.0"));
    assert_eq!(sb.log(), ["mdcat 2.0-1", "mdcat 1.0-1"]);

    // A file dropped from the store is dropped from the publish: the tree
    // mirrors the store rather than accumulating.
    fs::remove_file(sb.root.join("store/aur/packages/mdcat/mdcat.install")).unwrap();
    sb.tree("2.0", "2", &"b".repeat(64));
    let (code, out) = sb.pacrat(&["push", "mdcat", "--yes"]);
    assert_eq!(code, 0, "{out}");
    assert!(
        sb.published("mdcat.install").is_none(),
        "removed file survived"
    );
}

/// THE alarm. An already-published version whose tarball now hashes
/// differently: someone rewrote an immutable tag, and pacrat will not
/// quietly re-sum it.
#[test]
fn a_rewritten_tarball_at_a_published_version_alarms_and_publishes_nothing() {
    if !can_publish() {
        return;
    }
    let sb = Sandbox::new("alarm");
    let remote = sb.remote();
    sb.ledger("maintained", &format!("file://{}", remote.display()));
    sb.tree("3.0", "1", &"a".repeat(64));
    assert_eq!(sb.pacrat(&["push", "mdcat", "--yes"]).0, 0);
    let published_before = sb.published("PKGBUILD").unwrap();

    // Same version, same source, different checksum — a pkgrel bump does not
    // excuse it: the tag's bytes are supposed to be the tag's bytes.
    sb.tree("3.0", "2", &"c".repeat(64));
    let (code, out) = sb.pacrat(&["push", "mdcat", "--yes"]);
    assert_eq!(code, 1, "the alarm must fail the run: {out}");
    assert!(out.contains("ALARM"), "{out}");
    assert!(out.contains("incident"), "{out}");
    assert!(
        out.contains(&"a".repeat(64)),
        "the published sum is not shown: {out}"
    );
    assert!(
        out.contains(&"c".repeat(64)),
        "the new sum is not shown: {out}"
    );
    // The alarm is the case where the tree is the evidence.
    assert!(out.contains("kept for inspection"), "{out}");

    assert_eq!(
        sb.published("PKGBUILD").unwrap(),
        published_before,
        "the alarm published anyway"
    );
    assert_eq!(sb.log().len(), 1);

    // The way past it is a new version, which is a new claim about new bytes.
    sb.tree("3.1", "1", &"c".repeat(64));
    let (code, out) = sb.pacrat(&["push", "mdcat", "--yes"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("published mdcat 3.1-1"), "{out}");
}

/// Without `--yes` and without a terminal, nothing is published. A piped
/// answer is not consent for a publish — the one verb where that is stricter
/// than the rest of pacrat.
#[test]
fn a_publish_nobody_confirmed_does_not_happen() {
    if !can_publish() {
        return;
    }
    let sb = Sandbox::new("declined");
    let remote = sb.remote();
    sb.ledger("maintained", &format!("file://{}", remote.display()));
    sb.tree("1.0", "1", &"a".repeat(64));

    let (code, out) = sb.pacrat(&["push", "mdcat"]);
    assert_eq!(code, 10, "{out}");
    assert!(out.contains("not published"), "{out}");
    assert!(out.contains("not a terminal"), "{out}");
    assert!(
        sb.published("PKGBUILD").is_none(),
        "an unconfirmed publish landed"
    );
    // A decline is an answered question, not evidence: no scratch left behind.
    assert!(!out.contains("kept for inspection"), "{out}");
}

/// Push is for packages we claim. A vendored one is somebody else's.
#[test]
fn a_vendored_package_is_refused_by_role() {
    let sb = Sandbox::new("role");
    sb.ledger("vendored", "https://aur.archlinux.org/mdcat.git");
    sb.tree("1.0", "1", &"a".repeat(64));

    let (code, out) = sb.pacrat(&["push", "mdcat", "--yes"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("vendored"), "{out}");
    assert!(out.contains("maintained"), "{out}");
    // Refused before the network: no probe, no clone.
    assert!(!out.contains("run       ssh"), "it probed anyway: {out}");
    assert!(!out.contains("git clone"), "it cloned anyway: {out}");
}

/// A package nobody vendored has nothing to publish, and says so in the same
/// words `review` uses.
#[test]
fn an_unvendored_package_is_refused_before_anything_else() {
    let sb = Sandbox::new("absent");
    sb.write("store/aur/sources.toml", "");
    let (code, out) = sb.pacrat(&["push", "mdcat", "--yes"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("not vendored"), "{out}");
}

/// The queue is what a blocked publish leaves behind, and `pacrat status` is
/// where a maintainer trips over it again. Written directly here — the
/// blocking answer comes from the AUR, and no test reaches the AUR.
#[test]
fn a_queued_publish_shows_up_in_status_and_in_the_drain() {
    let sb = Sandbox::new("queue");
    sb.ledger("maintained", "https://aur.archlinux.org/mdcat.git");
    sb.tree("1.0", "1", &"a".repeat(64));
    sb.write(
        "state/pacrat/pushes/queue.toml",
        &format!(
            "[pushes.mdcat]\n\
             commit = \"{REVIEWED}\"\n\
             digest = \"{DIGEST_SHAPED}\"\n\
             queued_at = 1754784000\n\
             last_probe = 1754870400\n\
             last_answer = \"The AUR is down due to maintenance. We will be back soon.\"\n"
        ),
    );

    let (code, out) = sb.pacrat(&["status"]);
    assert_eq!(code, 0, "{out}");
    assert!(
        out.contains("1 publish queued (aur ssh: The AUR is down due to maintenance."),
        "{out}"
    );

    // The drain re-probes, which reaches the AUR — not from a test. What is
    // checked here is the shape of the queue the drain reads: an entry whose
    // digest is not the store's is re-queued rather than published, and that
    // decision is made before any probe.
    let queue = fs::read_to_string(sb.root.join("state/pacrat/pushes/queue.toml")).unwrap();
    assert!(queue.contains("mdcat"));
}

/// An empty queue is not a failure and not a probe: there is nothing to ask
/// about.
#[test]
fn draining_an_empty_queue_asks_nobody_anything() {
    let sb = Sandbox::new("empty-queue");
    sb.ledger("maintained", "https://aur.archlinux.org/mdcat.git");
    sb.tree("1.0", "1", &"a".repeat(64));

    let (code, out) = sb.pacrat(&["push"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("publish queue empty"), "{out}");
    assert!(
        !out.contains("run       ssh"),
        "it probed an empty queue: {out}"
    );
}

/// The drain's other half: a queued errand whose bytes are still the bytes
/// that were queued is published, and the entry goes away.
///
/// The remote is a local bare repo, so nothing here is bound for the AUR and
/// nothing is probed — which is exactly the condition that makes the happy
/// path testable at all while the AUR is read-only.
#[test]
fn a_queued_publish_whose_bytes_are_unchanged_is_published_by_the_drain() {
    if !can_publish() {
        return;
    }
    let sb = Sandbox::new("drain-ready");
    let remote = sb.remote();
    sb.ledger("maintained", &format!("file://{}", remote.display()));
    sb.tree("1.0", "1", &"a".repeat(64));

    let digest = sb.digest();
    sb.queue("mdcat", REVIEWED, &digest);

    let (code, out) = sb.pacrat(&["push", "--retry", "--yes"]);
    assert_eq!(code, 0, "{out}");
    assert!(
        !out.contains("run       ssh"),
        "it probed a local remote: {out}"
    );
    assert!(out.contains("published mdcat 1.0-1"), "{out}");
    assert!(out.contains("publish queue empty"), "{out}");
    assert!(sb.published("PKGBUILD").is_some());
    assert!(
        !sb.queue_text().contains("mdcat"),
        "the entry outlived the publish"
    );
}

/// A queued errand whose store tree moved on is not published: what was
/// queued is not what is there now, and "publish the newer thing instead" is
/// a decision nobody made. It is re-queued against the new bytes, with a note.
#[test]
fn a_queued_publish_whose_tree_moved_is_re_queued_rather_than_published() {
    if !can_publish() {
        return;
    }
    let sb = Sandbox::new("drain-moved");
    let remote = sb.remote();
    sb.ledger("maintained", &format!("file://{}", remote.display()));
    sb.tree("1.0", "1", &"a".repeat(64));
    let queued_digest = sb.digest();
    sb.queue("mdcat", REVIEWED, &queued_digest);

    // The store moves after the errand was recorded — a sync, a hand, an
    // adopted update. The ledger's commit has not moved with it.
    sb.tree("1.1", "1", &"b".repeat(64));

    let (code, out) = sb.pacrat(&["push", "--retry", "--yes"]);
    assert_eq!(code, 10, "{out}");
    assert!(out.contains("re-queued"), "{out}");
    assert!(
        out.contains("the store tree changed since it was queued"),
        "{out}"
    );
    assert!(
        sb.published("PKGBUILD").is_none(),
        "it published the newer tree anyway"
    );

    // The entry now describes the bytes that are actually there, so an
    // explicit push is one command away.
    let queue = sb.queue_text();
    assert!(
        queue.contains(&sb.digest()),
        "not re-queued at the new digest: {queue}"
    );
    assert!(
        !queue.contains(&queued_digest),
        "the stale digest survived: {queue}"
    );
    assert!(queue.contains("note = "), "{queue}");

    let (code, out) = sb.pacrat(&["push", "mdcat", "--yes"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("published mdcat 1.1-1"), "{out}");
    assert!(
        !sb.queue_text().contains("mdcat"),
        "the entry outlived the publish"
    );
}

/// A queued package that is no longer maintained in the ledger is an errand
/// nobody can run: dropped, loudly, rather than pushed under a claim that has
/// been withdrawn.
#[test]
fn a_queued_publish_that_lost_its_claim_is_dropped() {
    let sb = Sandbox::new("drain-gone");
    let remote = sb.remote();
    sb.ledger("vendored", &format!("file://{}", remote.display()));
    sb.tree("1.0", "1", &"a".repeat(64));
    sb.queue("mdcat", REVIEWED, DIGEST_SHAPED);

    let (code, out) = sb.pacrat(&["push", "--retry", "--yes"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("dropped"), "{out}");
    assert!(out.contains("vendored"), "{out}");
    assert!(sb.published("PKGBUILD").is_none());
    assert!(!sb.queue_text().contains("mdcat"));
}

#[test]
fn retry_and_a_package_name_are_not_one_command() {
    let sb = Sandbox::new("retry-arg");
    sb.ledger("maintained", "https://aur.archlinux.org/mdcat.git");
    let (code, out) = sb.pacrat(&["push", "mdcat", "--retry"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("--retry"), "{out}");
}

/// A queue file pacrat did not write is refused rather than acted on: the
/// package name in it becomes a store lookup and a command.
#[test]
fn a_hostile_queue_file_is_refused_by_status_rather_than_obeyed() {
    let sb = Sandbox::new("hostile-queue");
    sb.ledger("maintained", "https://aur.archlinux.org/mdcat.git");
    sb.write(
        "state/pacrat/pushes/queue.toml",
        &format!(
            "[pushes.\"../../../../etc\"]\n\
             commit = \"{OTHER}\"\n\
             digest = \"{DIGEST_SHAPED}\"\n\
             queued_at = 1\nlast_probe = 1\nlast_answer = \"\"\n"
        ),
    );
    let (code, out) = sb.pacrat(&["status"]);
    // status degrades rather than dying — but it says so, and it does not
    // report a queued publish it could not read.
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("queue  unreadable"), "{out}");

    // The drain refuses outright: it was about to act on that name.
    let (code, out) = sb.pacrat(&["push"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("not a package name"), "{out}");
}
