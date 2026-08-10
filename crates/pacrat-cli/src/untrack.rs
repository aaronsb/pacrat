//! `pacrat untrack` — accept reality into the manifest: the tracked →
//! unmanaged step, `add`'s exact inverse (ADR-002 amendment).
//!
//! A manifest-only edit, and deliberately nothing more. pacman owns the
//! machine's package state, so pacrat prints no uninstall here and takes
//! no removal framing: a package untracked while still installed is simply
//! demoted to unmanaged, and `pacrat sync --prune`'s printed plan surfaces
//! the uninstall where it always lived — typed by the operator, never run
//! by pacrat.
//!
//! Like `add`, the verb asks nothing: the operator typed the names, and
//! repeating them back as a question adds nothing. The confirm lives in
//! the TUI apply instead, where a selection was *accumulated* rather than
//! typed. Both surfaces edit the file through [`Ctx::save_tracked`] — one
//! writer, so the two directions of the ladder cannot disagree about it.

use pacrat_core::pkg::{valid_name, Source};

use crate::add::Change;
use crate::ctx::Ctx;
use crate::out::visible_line;

pub fn run(ctx: &Ctx, packages: &[String]) -> Result<(), String> {
    let changes = untrack(ctx, &ctx.host, packages)?;
    for c in &changes {
        println!(
            "{}: untracked {} on {} ({} → {} packages) — {}",
            c.source.name(),
            c.packages.join(", "),
            ctx.host,
            c.before,
            c.after,
            c.path.strip_prefix(&ctx.store).unwrap_or(&c.path).display()
        );
    }
    println!("note: commit the store to sync this to other hosts — anything still");
    println!("      installed here appears in `pacrat sync --prune`'s plan.");
    Ok(())
}

/// Drop `packages` from `host`'s tracked lists, and say what changed.
///
/// Refusal, not filtering — the rule `add` applies from the other side. A
/// name in none of this host's lists is nothing untrack can edit, and
/// skipping it silently would report a reconciliation that did not happen;
/// one stranger refuses the whole run, and nothing is written.
pub fn untrack(ctx: &Ctx, host: &str, packages: &[String]) -> Result<Vec<Change>, String> {
    if packages.is_empty() {
        return Err("nothing to untrack".into());
    }
    // The grammar, checked at the door as everywhere else: these names are
    // compared against lists and printed back in the report, and the report
    // of a hostile name must not itself be hostile.
    if let Some(bad) = packages.iter().find(|p| !valid_name(p)) {
        let (shown, _) = visible_line(bad);
        return Err(format!(
            "{shown:?} is not a package name — expected letters, digits and \
             @._+- (no leading hyphen or dot)"
        ));
    }

    let lists: Vec<(Source, Vec<String>)> = Source::ALL
        .iter()
        .map(|s| ctx.tracked(host, *s).map(|list| (*s, list)))
        .collect::<Result<_, _>>()?;
    for pkg in packages {
        if !lists.iter().any(|(_, list)| list.iter().any(|t| t == pkg)) {
            return Err(format!(
                "{pkg} is not tracked on {host} — untrack edits this host's \
                 manifest, and no list names it (`pacrat hosts` shows what is \
                 tracked where)"
            ));
        }
    }

    let mut changes = Vec::new();
    for (source, list) in lists {
        let (removed, kept): (Vec<String>, Vec<String>) = list
            .into_iter()
            .partition(|t| packages.iter().any(|p| p == t));
        if removed.is_empty() {
            continue;
        }
        let path = ctx.save_tracked(host, source, &kept)?;
        changes.push(Change {
            source,
            before: kept.len() + removed.len(),
            after: kept.len(),
            packages: removed,
            path,
        });
    }
    Ok(changes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pacrat_core::config::Config;
    use std::fs;
    use std::path::PathBuf;

    /// A store with one host's lists in it, and nothing else — the same
    /// fixture shape `ctx`'s own tests use. Never the real store: the path
    /// is a temp directory this test owns and removes.
    fn store(tag: &str) -> Ctx {
        let store = env_root(tag);
        let _ = fs::remove_dir_all(&store);
        fs::create_dir_all(store.join("packages").join("north")).unwrap();
        Ctx {
            store,
            host: "north".into(),
            config: Config::default(),
        }
    }

    fn env_root(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("pacrat-untrack-{tag}-{}", std::process::id()))
    }

    fn write(ctx: &Ctx, source: Source, contents: &str) -> PathBuf {
        let path = ctx
            .store
            .join("packages")
            .join("north")
            .join(format!("{}.txt", source.name()));
        fs::write(&path, contents).unwrap();
        path
    }

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    /// The amendment's core claim, as bytes: `add` and `untrack` are exact
    /// inverses because they share one writer. Growing a list the way
    /// `adopt` does and then untracking the same names gives back the very
    /// file that was there before.
    #[test]
    fn add_then_untrack_is_identity_on_the_list_file() {
        let ctx = store("identity");
        let path = write(&ctx, Source::Native, "fd\nripgrep\n");
        let original = fs::read_to_string(&path).unwrap();

        // The add direction: the list plus the newcomers, through the
        // shared writer — exactly the call `adopt` makes.
        let mut grown = ctx.tracked("north", Source::Native).unwrap();
        grown.extend(v(&["zoxide", "bat"]));
        ctx.save_tracked("north", Source::Native, &grown).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "bat\nfd\nripgrep\nzoxide\n"
        );

        // And back.
        let changes = untrack(&ctx, "north", &v(&["zoxide", "bat"])).unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].packages, v(&["bat", "zoxide"]));
        assert_eq!((changes[0].before, changes[0].after), (4, 2));
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        let _ = fs::remove_dir_all(&ctx.store);
    }

    /// One run may edit several lists: the names name the packages, and the
    /// packages say which files change.
    #[test]
    fn untrack_edits_every_list_that_names_a_package() {
        let ctx = store("sources");
        write(&ctx, Source::Native, "fd\nripgrep\n");
        write(&ctx, Source::Aur, "mdcat\nyay\n");

        let changes = untrack(&ctx, "north", &v(&["fd", "mdcat"])).unwrap();
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0].source, Source::Native);
        assert_eq!(changes[0].packages, v(&["fd"]));
        assert_eq!(changes[1].source, Source::Aur);
        assert_eq!(changes[1].packages, v(&["mdcat"]));
        assert_eq!(
            ctx.tracked("north", Source::Native).unwrap(),
            v(&["ripgrep"])
        );
        assert_eq!(ctx.tracked("north", Source::Aur).unwrap(), v(&["yay"]));
        let _ = fs::remove_dir_all(&ctx.store);
    }

    /// Untracking everything a list holds leaves an empty list that reads
    /// back as one — the host is still in the manifest, tracking nothing.
    #[test]
    fn untracking_the_whole_list_is_an_empty_list_not_a_missing_one() {
        let ctx = store("empty");
        let path = write(&ctx, Source::Native, "fd\n");
        untrack(&ctx, "north", &v(&["fd"])).unwrap();
        assert!(path.exists());
        assert!(ctx.tracked("north", Source::Native).unwrap().is_empty());
        let _ = fs::remove_dir_all(&ctx.store);
    }

    /// Refusal, not filtering: one name tracked nowhere refuses the whole
    /// run in plain words, and the lists that do name the others are left
    /// exactly as they were.
    #[test]
    fn a_stranger_refuses_the_whole_run_and_nothing_is_written() {
        let ctx = store("stranger");
        let path = write(&ctx, Source::Native, "fd\nripgrep\n");

        let err = untrack(&ctx, "north", &v(&["fd", "cowsay"])).unwrap_err();
        assert!(err.contains("cowsay is not tracked on north"), "{err}");
        assert_eq!(fs::read_to_string(&path).unwrap(), "fd\nripgrep\n");
        let _ = fs::remove_dir_all(&ctx.store);
    }

    /// The door check: a name outside the grammar is refused before any
    /// list is read, and the refusal cannot carry the escape it reports.
    #[test]
    fn a_hostile_name_is_refused_at_the_door() {
        let ctx = store("hostile");
        write(&ctx, Source::Native, "fd\n");
        let err = untrack(&ctx, "north", &v(&["fd\u{1b}[8m"])).unwrap_err();
        assert!(
            !err.contains('\u{1b}'),
            "the report re-emitted ESC: {err:?}"
        );
        assert!(err.contains("not a package name"), "{err}");

        assert!(
            untrack(&ctx, "north", &[]).is_err(),
            "empty args must refuse"
        );
        let _ = fs::remove_dir_all(&ctx.store);
    }
}
