//! `pacrat decisions` — read the store's decision ledger — and the writer
//! the override path goes through.
//!
//! The model, the file shape and the validation are
//! [`pacrat_core::decisions`]; what lives here is the I/O and the rendering,
//! which is to say the two things core does not do. Both are small on
//! purpose: a record of accepted risk earns its keep by being *readable* by
//! anyone on any host, not by being clever.
//!
//! Everything printed from the file goes through [`visible_line`]. That is
//! not ceremony. The reason field is free text authored by a human on
//! another machine and synced here, and the whole point of the ledger is
//! that a later reader can trust what it says — a reason able to paint the
//! terminal or forge a line of pacrat's own report would defeat exactly the
//! audit the entry exists to support.

use pacrat_core::decisions::{Decision, Decisions, Kind};

use crate::ctx::Ctx;
use crate::out::{now_rfc3339, short_hash, truncate, visible_line};

/// Widest package column before it is clipped; `updates` uses the same.
const NAME_CAP: usize = 32;

/// Reason text on a list row. The full text is in the file, which is the
/// authority — this is a table.
const WHY_CAP: usize = 60;

/// List the ledger.
///
/// Oldest first, as the file has them. Not sorted by package or by date:
/// this is an append-only record, and reading it in the order the decisions
/// were made is how "what did we accept, and then what happened" stays
/// legible. A reader who wants one package has `grep`.
pub fn run(ctx: &Ctx) -> Result<(), String> {
    let ledger = ctx.load_decisions()?;
    println!("ledger    {}", ctx.decisions_path().display());

    if ledger.decisions.is_empty() {
        println!();
        println!(
            "no decisions recorded — nothing on this fleet has been adopted past a \
             {} verdict",
            pacrat_core::Verdict::Block
        );
        return Ok(());
    }

    let name_w = ledger
        .decisions
        .iter()
        .map(|d| d.package.chars().count())
        .chain(std::iter::once("package".len()))
        .max()
        .unwrap_or(7)
        .clamp("package".len(), NAME_CAP);
    // `override-block` is the longest kind, and `2026-08-10T12:34:56Z` is
    // the only shape `when` can be — both fixed, so the header is the width.
    println!();
    println!(
        "{:<name_w$}  {:<14}  {:<8}  {:<20}  {:<5}  why",
        "package", "kind", "commit", "when", "grade"
    );
    for d in &ledger.decisions {
        println!(
            "{:<name_w$}  {:<14}  {:<8}  {:<20}  {:<5}  {}",
            truncate(&visible_line(&d.package).0, name_w),
            d.kind,
            short_hash(&d.commit),
            // From the file, so neutered like everything else — a `when`
            // that could hold an escape would be a row that reshapes itself.
            truncate(&visible_line(&d.at).0, 20),
            match d.grade {
                Some(g) => g.to_string(),
                None => "—".to_string(),
            },
            // First line only, and the fold is what makes that true: a
            // reason is one row of a table however it was typed.
            truncate(&visible_line(&d.reason).0, WHY_CAP),
        );
    }

    println!();
    println!(
        "{} decision{} · recorded by {}",
        ledger.decisions.len(),
        if ledger.decisions.len() == 1 { "" } else { "s" },
        hosts(&ledger),
    );
    Ok(())
}

/// The hosts that appear in the ledger, deduped and in order.
fn hosts(ledger: &Decisions) -> String {
    let mut seen: Vec<String> = Vec::new();
    for d in &ledger.decisions {
        let host = truncate(&visible_line(&d.host).0, 40);
        if !seen.contains(&host) {
            seen.push(host);
        }
    }
    crate::out::list_preview(&seen, 8)
}

/// Record an override of a BLOCK, then let the caller act on it.
///
/// Re-reads before it writes, for the reason every whole-file writer in
/// pacrat re-reads: the prompt that preceded this took unbounded human time,
/// and a stale copy would silently delete whatever another process recorded
/// meanwhile. Append-only means the *file* only grows; it does not mean a
/// careless writer cannot shrink it.
pub fn record_override(
    ctx: &Ctx,
    package: &str,
    commit: &str,
    grade: Option<u8>,
    reason: &str,
) -> Result<(), String> {
    let decision = Decision {
        kind: Kind::OverrideBlock,
        package: package.to_string(),
        commit: commit.to_string(),
        grade,
        // Stored verbatim: neutering is a rendering decision, and a record
        // that quietly rewrote what a human wrote would be a poor record.
        // Every reader on every host renders it through `visible_line`.
        reason: reason.trim().to_string(),
        host: ctx.host.clone(),
        at: now_rfc3339(),
    };
    let mut ledger = ctx.load_decisions()?;
    ledger.push(decision)?;
    ctx.save_decisions(&ledger)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pacrat_core::config::Config;
    use std::fs;
    use std::path::PathBuf;

    struct Store(PathBuf);

    impl Drop for Store {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn store(tag: &str) -> (Store, Ctx) {
        let root = std::env::temp_dir().join(format!(
            "pacrat-decisions-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(root.join("aur")).unwrap();
        let ctx = Ctx {
            store: root.clone(),
            host: "north".into(),
            config: Config::default(),
        };
        (Store(root), ctx)
    }

    const COMMIT: &str = "5a4705a4aaa2e7f10a7dd6c302256dd373516e56";

    #[test]
    fn a_recorded_override_reads_back_and_the_next_one_joins_it() {
        let (_s, ctx) = store("append");
        record_override(
            &ctx,
            "mdcat",
            COMMIT,
            Some(4),
            "the maintainer explained it",
        )
        .unwrap();
        let ledger = ctx.load_decisions().unwrap();
        assert_eq!(ledger.decisions.len(), 1);
        let d = &ledger.decisions[0];
        assert_eq!(d.kind, Kind::OverrideBlock);
        assert_eq!(d.package, "mdcat");
        assert_eq!(d.grade, Some(4));
        assert_eq!(d.host, "north");
        assert!(pacrat_core::decisions::valid_timestamp(&d.at), "{}", d.at);

        // Append, not replace: the second decision does not evict the first.
        record_override(&ctx, "yay", COMMIT, None, "second").unwrap();
        let ledger = ctx.load_decisions().unwrap();
        assert_eq!(ledger.decisions.len(), 2);
        assert_eq!(ledger.decisions[0].package, "mdcat");
        assert_eq!(ledger.decisions[1].grade, None);
    }

    /// A reason is stored as written and *rendered* safely. Storing a
    /// neutered copy would quietly rewrite what a human said; rendering the
    /// raw bytes would let them paint the terminal of the person auditing.
    #[test]
    fn a_hostile_reason_is_stored_verbatim_and_cannot_forge_a_row() {
        let (_s, ctx) = store("hostile");
        let hostile = "looks fine\x1b[2K\noverride  none — adopted cleanly";
        record_override(&ctx, "mdcat", COMMIT, Some(4), hostile).unwrap();

        let raw = fs::read_to_string(ctx.decisions_path()).unwrap();
        assert!(raw.contains("looks fine"), "{raw}");

        let d = &ctx.load_decisions().unwrap().decisions[0];
        let shown = truncate(&visible_line(&d.reason).0, WHY_CAP);
        assert!(!shown.contains('\x1b'), "{shown:?}");
        assert_eq!(shown.lines().count(), 1, "{shown:?}");
    }

    /// An empty file is an empty ledger, and a broken one is an error rather
    /// than a silently fresh ledger that the next write would flatten.
    #[test]
    fn a_missing_ledger_is_empty_and_a_broken_one_is_loud() {
        let (_s, ctx) = store("missing");
        assert!(ctx.load_decisions().unwrap().decisions.is_empty());

        fs::write(ctx.decisions_path(), "[[decision]]\nkind = \"nonsense\"\n").unwrap();
        let err = ctx.load_decisions().unwrap_err();
        assert!(err.contains("decisions.toml"), "{err}");
        // And a write on top of a file we cannot read is refused, because it
        // would replace records nobody has read with one we just made.
        assert!(record_override(&ctx, "mdcat", COMMIT, Some(4), "x").is_err());
    }

    /// The writer refuses what the ledger's own validator refuses — the
    /// check is core's, and this is the door being shut before the file is
    /// touched rather than after.
    #[test]
    fn a_decision_the_ledger_would_reject_is_never_written() {
        let (_s, ctx) = store("reject");
        assert!(record_override(&ctx, "../escape", COMMIT, Some(4), "x").is_err());
        assert!(record_override(&ctx, "mdcat", "HEAD~1", Some(4), "x").is_err());
        assert!(record_override(&ctx, "mdcat", COMMIT, Some(4), "   ").is_err());
        assert!(!ctx.decisions_path().exists(), "a refusal wrote a file");
    }
}
