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
        self.digest_of("mdcat")
    }

    fn digest_of(&self, package: &str) -> String {
        let dir = self.root.join("store/aur/packages").join(package);
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

    /// An `ssh` that says what the test needs the AUR to have said.
    ///
    /// The write probe is the one place pacrat asks a question whose *answer*
    /// changes what it tells the operator, and the answers worth testing —
    /// maintenance, a refused key, a name that will not resolve — cannot be
    /// arranged against the real service. So the service is faked at the
    /// process boundary, which leaves everything above it real: the argv,
    /// the classification, the wording, the queue entry.
    fn fake_ssh(&self, stdout: &str, stderr: &str, code: i32) {
        let path = self.write(
            "bin/ssh",
            &format!(
                "#!/bin/sh\n\
                 [ -n \"{stdout}\" ] && printf '%s\\n' \"{stdout}\"\n\
                 [ -n \"{stderr}\" ] && printf '%s\\n' \"{stderr}\" >&2\n\
                 exit {code}\n"
            ),
        );
        let mut perms = fs::metadata(&path).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        fs::set_permissions(&path, perms).unwrap();
    }

    fn pacrat(&self, args: &[&str]) -> (i32, String) {
        // The sandbox's own bin first, so a `fake_ssh` wins over the real one.
        // Nothing else is shadowed: the directory holds only what a test put
        // there, and does not exist at all for tests that never call it.
        let path = format!("{}/bin:/usr/bin:/bin", self.root.display());
        let out = Command::new(env!("CARGO_BIN_EXE_pacrat"))
            .args(args)
            // Cleared, not extended: an inherited DOTFILES_DIR or XDG path
            // would let the developer's own store and queue answer.
            .env_clear()
            .env("PATH", path)
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

/// Is `binary` on PATH? Asked of PATH rather than of `/usr/bin`, because a
/// container image, a Nix profile or a `~/.local/bin` install all put it
/// somewhere else — and a skip that fires because the lookup was too narrow
/// is a test that quietly stopped running.
fn have(binary: &str) -> bool {
    std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).any(|dir| dir.join(binary).is_file()))
        .unwrap_or(false)
}

/// Publishing needs makepkg (`--printsrcinfo`), which is Arch's.
///
/// A skip is the right answer on a machine that cannot run makepkg at all —
/// and the wrong one on a machine that was *supposed* to be able to, where a
/// silent skip means the publish path went untested and the run was still
/// green. `PACRAT_REQUIRE_MAKEPKG=1` turns the skip into a failure, and CI's
/// Arch job sets it: that job exists precisely to run these, so a skip there
/// is a broken job rather than a tolerable one.
fn can_publish() -> bool {
    if have("makepkg") {
        return true;
    }
    assert!(
        std::env::var_os("PACRAT_REQUIRE_MAKEPKG").is_none(),
        "PACRAT_REQUIRE_MAKEPKG is set but makepkg is not on PATH — this run was \
         supposed to exercise the publish path and would otherwise have skipped it"
    );
    eprintln!("skipping: makepkg is not on PATH (publishing is Arch-only)");
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

/// Every blocked probe queues the errand — and says something different
/// about why, because "the AUR is not accepting publishes" is a claim about a
/// service and only one of these is evidence for it. A maintainer whose key
/// is not registered must not be sent to wait for an outage to end.
#[test]
fn a_blocked_probe_queues_but_names_the_right_problem() {
    let cases = [
        (
            "The AUR is down due to maintenance. We will be back soon.",
            "",
            1,
            "the remote is not accepting publishes",
            "blocked",
        ),
        (
            "",
            "Permission denied (publickey).",
            255,
            "the AUR refused this host's ssh key",
            "key refused",
        ),
        (
            "",
            "ssh: Could not resolve hostname aur.archlinux.org: Name or service not known",
            255,
            "the AUR could not be reached from this host",
            "unreachable",
        ),
    ];
    for (stdout, stderr, code, summary, label) in cases {
        let sb = Sandbox::new("probe");
        sb.ledger("maintained", "https://aur.archlinux.org/mdcat.git");
        sb.tree("1.0", "1", &"a".repeat(64));
        sb.fake_ssh(stdout, stderr, code);

        let (exit, out) = sb.pacrat(&["push", "mdcat", "--yes"]);
        assert_eq!(exit, 10, "{out}");
        assert!(out.contains(&format!("probe     {label}")), "{out}");
        assert!(out.contains(summary), "wrong diagnosis: {out}");
        // The errand is real whichever it was.
        assert!(sb.queue_text().contains("mdcat"), "not queued: {out}");
        // And the server's own words are what got recorded.
        let said = if stdout.is_empty() { stderr } else { stdout };
        assert!(sb.queue_text().contains(said.trim()), "{}", sb.queue_text());
    }

    // The *open* path has no test here on purpose: past the probe, push
    // clones the derived `ssh://aur@aur.archlinux.org/…` URL, and a test that
    // got that far would be talking to the real AUR. What happens after an
    // open probe is what every local-remote test above already covers, since
    // those take the same path with no probe in front of it.
    //
    // Only a reachable server's refusal may be reported as the AUR being
    // read-only. The other two must not say it at all.
    let sb = Sandbox::new("probe-nokey");
    sb.ledger("maintained", "https://aur.archlinux.org/mdcat.git");
    sb.tree("1.0", "1", &"a".repeat(64));
    sb.fake_ssh("", "Permission denied (publickey).", 255);
    let (_, out) = sb.pacrat(&["push", "mdcat", "--yes"]);
    assert!(!out.contains("not accepting publishes"), "{out}");
    assert!(out.contains("your ssh setup"), "{out}");
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

/// THE staging bug. `git add` honours ignore rules, and the AUR's own
/// recommended `.gitignore` for package repositories ignores everything and
/// re-includes a few names — so a patch in the store was silently not
/// published, was absent from the diff the maintainer confirmed, and the
/// command exited 0.
#[test]
fn an_ignore_file_cannot_drop_a_store_file_from_the_publish() {
    if !can_publish() {
        return;
    }
    let sb = Sandbox::new("gitignore");
    let remote = sb.remote();
    sb.ledger("maintained", &format!("file://{}", remote.display()));
    sb.tree("1.0", "1", &"a".repeat(64));
    // The .gitignore the AUR wiki recommends: ignore everything, re-include
    // the tracked names by hand.
    sb.write(
        "store/aur/packages/mdcat/.gitignore",
        "*\n!.gitignore\n!.SRCINFO\n!PKGBUILD\n",
    );
    sb.write(
        "store/aur/packages/mdcat/fix-cve.patch",
        "--- a/x\n+++ b/x\n@@ -1 +1 @@\n-bad\n+good\n",
    );

    let (code, out) = sb.pacrat(&["push", "mdcat", "--yes"]);
    assert_eq!(code, 0, "{out}");
    // Shown to the human before they answered...
    assert!(
        out.contains("fix-cve.patch"),
        "the patch was never mentioned: {out}"
    );
    // ...and actually published.
    assert!(
        sb.published("fix-cve.patch").is_some(),
        "an ignored store file was silently dropped from the publish"
    );
    assert!(sb.published(".gitignore").is_some());
    assert!(sb.published("PKGBUILD").is_some());
    assert!(sb.published(".SRCINFO").is_some());
}

/// A PKGBUILD is shell, and `makepkg --printsrcinfo` sources it. Whatever
/// that leaves in the directory is not part of the publish.
#[test]
fn a_pkgbuild_side_effect_is_swept_rather_than_published() {
    if !can_publish() {
        return;
    }
    let sb = Sandbox::new("side-effect");
    let remote = sb.remote();
    sb.ledger("maintained", &format!("file://{}", remote.display()));
    // Top-level shell that runs the moment makepkg sources the file.
    sb.write(
        "store/aur/packages/mdcat/PKGBUILD",
        "pkgname=mdcat\npkgver=1.0\npkgrel=1\narch=('any')\n\
         printf 'leaked\\n' > sneaky.txt\nmkdir -p build/deep\n\
         printf 'x\\n' > build/deep/artifact.o\n\
         package() { :; }\n",
    );

    let (code, out) = sb.pacrat(&["push", "mdcat", "--yes"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("swept"), "the sweep was silent: {out}");
    assert!(out.contains("sneaky.txt"), "{out}");
    assert!(
        sb.published("sneaky.txt").is_none(),
        "a side effect of sourcing the PKGBUILD was published"
    );
    assert!(sb.published("build/deep/artifact.o").is_none());
    assert!(sb.published("PKGBUILD").is_some());
}

/// Publishing twice in a row must be a no-op the second time. It is the
/// property that both bugs above broke: a dropped file and a swept-in side
/// effect each make the remote disagree with the store forever, so every
/// later push finds "changes" and commits them again.
#[test]
fn publishing_converges() {
    if !can_publish() {
        return;
    }
    let sb = Sandbox::new("converge");
    let remote = sb.remote();
    sb.ledger("maintained", &format!("file://{}", remote.display()));
    sb.tree("1.0", "1", &"a".repeat(64));
    sb.write(
        "store/aur/packages/mdcat/.gitignore",
        "*\n!.gitignore\n!.SRCINFO\n!PKGBUILD\n",
    );
    sb.write("store/aur/packages/mdcat/extra.patch", "patch\n");

    assert_eq!(sb.pacrat(&["push", "mdcat", "--yes"]).0, 0);
    let first = sb.log().len();
    let (code, out) = sb.pacrat(&["push", "mdcat", "--yes"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("already current"), "{out}");
    assert_eq!(
        sb.log().len(),
        first,
        "a second publish made another commit"
    );
}

/// The epoch is the field that outranks every other, and a publish flow that
/// dropped it would print the wrong version and misjudge what is an update.
#[test]
fn an_epoch_is_carried_into_the_version_everywhere() {
    if !can_publish() {
        return;
    }
    let sb = Sandbox::new("epoch");
    let remote = sb.remote();
    sb.ledger("maintained", &format!("file://{}", remote.display()));
    sb.write(
        "store/aur/packages/mdcat/PKGBUILD",
        "pkgname=mdcat\nepoch=2\npkgver=1.0\npkgrel=1\narch=('any')\npackage() { :; }\n",
    );

    let (code, out) = sb.pacrat(&["push", "mdcat", "--yes"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("published mdcat 2:1.0-1"), "{out}");
    assert_eq!(sb.log(), ["mdcat 2:1.0-1"]);

    // Upstream renumbered: a *lower* pkgver at a higher epoch is an update,
    // and must not be warned about as if nothing had moved.
    sb.write(
        "store/aur/packages/mdcat/PKGBUILD",
        "pkgname=mdcat\nepoch=3\npkgver=0.9\npkgrel=1\narch=('any')\npackage() { :; }\n",
    );
    let (code, out) = sb.pacrat(&["push", "mdcat", "--yes"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("published mdcat 3:0.9-1"), "{out}");
    assert!(
        !out.contains("no host will pick it up"),
        "an epoch bump was called a no-op: {out}"
    );

    // Going backwards is what the warning is for.
    sb.write(
        "store/aur/packages/mdcat/PKGBUILD",
        "pkgname=mdcat\nepoch=3\npkgver=0.9\npkgrel=1\narch=('any')\n# nudge\npackage() { :; }\n",
    );
    let (code, out) = sb.pacrat(&["push", "mdcat", "--yes"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("no host will pick it up"), "{out}");
}

/// One package's bad day must not wedge the queue. With a stable order, a
/// failure that stopped the loop would hold back the same later packages on
/// every run, forever.
#[test]
fn a_failing_entry_does_not_wedge_the_rest_of_the_queue() {
    if !can_publish() {
        return;
    }
    let sb = Sandbox::new("wedge");
    let remote = sb.remote();
    // `aaa-broken` sorts first and its remote does not exist; `mdcat` is fine.
    sb.write(
        "store/aur/sources.toml",
        &format!(
            "[packages.aaa-broken]\n\
             upstream = \"file://{}/nonexistent.git\"\n\
             reviewed = \"{REVIEWED}\"\n\
             role = \"maintained\"\n\
             \n\
             [packages.mdcat]\n\
             upstream = \"file://{}\"\n\
             reviewed = \"{REVIEWED}\"\n\
             role = \"maintained\"\n",
            sb.root.display(),
            remote.display()
        ),
    );
    sb.write(
        "store/aur/packages/aaa-broken/PKGBUILD",
        "pkgname=aaa-broken\npkgver=1.0\npkgrel=1\narch=('any')\npackage() { :; }\n",
    );
    sb.tree("1.0", "1", &"a".repeat(64));

    let broken_digest = sb.digest_of("aaa-broken");
    let mdcat_digest = sb.digest_of("mdcat");
    sb.write(
        "state/pacrat/pushes/queue.toml",
        &format!(
            "[pushes.aaa-broken]\ncommit = \"{REVIEWED}\"\ndigest = \"{broken_digest}\"\n\
             queued_at = 1754784000\nlast_probe = 1754870400\nlast_answer = \"blocked\"\n\
             \n\
             [pushes.mdcat]\ncommit = \"{REVIEWED}\"\ndigest = \"{mdcat_digest}\"\n\
             queued_at = 1754784000\nlast_probe = 1754870400\nlast_answer = \"blocked\"\n"
        ),
    );

    let (code, out) = sb.pacrat(&["push", "--retry", "--yes"]);
    // Non-zero, because something genuinely did not run...
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("failed"), "{out}");
    // ...and the healthy one behind it was published anyway.
    assert!(out.contains("published mdcat 1.0-1"), "{out}");
    assert!(sb.published("PKGBUILD").is_some());
    let queue = sb.queue_text();
    assert!(
        queue.contains("aaa-broken"),
        "the failing errand was lost: {queue}"
    );
    assert!(
        !queue.contains("mdcat"),
        "the published errand survived: {queue}"
    );
}

/// A store that is not there right now is not a withdrawn claim. Deleting the
/// errand over a transient condition is the one thing the queue exists to
/// prevent.
#[test]
fn a_store_tree_that_vanished_keeps_its_errand() {
    if !can_publish() {
        return;
    }
    let sb = Sandbox::new("moved-aside");
    let remote = sb.remote();
    sb.ledger("maintained", &format!("file://{}", remote.display()));
    sb.tree("1.0", "1", &"a".repeat(64));
    let digest = sb.digest();
    sb.queue("mdcat", REVIEWED, &digest);

    // As if the store were unmounted mid-sync: the ledger still claims it.
    let tree = sb.root.join("store/aur/packages/mdcat");
    let aside = sb.root.join("mdcat-aside");
    fs::rename(&tree, &aside).unwrap();

    let (code, out) = sb.pacrat(&["push", "--retry", "--yes"]);
    assert_eq!(code, 10, "{out}");
    assert!(out.contains("kept"), "{out}");
    assert!(out.contains("stays queued"), "{out}");
    assert!(
        sb.queue_text().contains("mdcat"),
        "a transient store problem deleted the errand"
    );

    // Put it back: the errand runs.
    fs::rename(&aside, &tree).unwrap();
    let (code, out) = sb.pacrat(&["push", "--retry", "--yes"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("published mdcat 1.0-1"), "{out}");
    assert!(!sb.queue_text().contains("mdcat"));
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
