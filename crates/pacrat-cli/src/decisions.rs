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
///
/// **Read-modify-write, and not locked.** Two pacrats overriding on the same
/// host in the same handful of microseconds — between this re-read and the
/// rename below — would leave only the second entry. That window is not
/// worth a lock file here: it opens *after* a human answered a prompt, which
/// is to say after the slowest step in the flow, and the sequence that would
/// hit it is one person running two overrides at once. The fleet-wide claim
/// is carried by git, which is where a real concurrent-write conflict shows
/// up as a merge conflict a human resolves — the same answer `sources.toml`
/// relies on. If the TUI ever gains background overrides, this is the line
/// to revisit.
///
/// `digest` is the candidate tree's content digest — what the human actually
/// read. Both surfaces pass it, because both have it, and both go through
/// *this* function: a commit id names a tree and can be made to name a
/// different one, so an entry keyed on the name alone cannot be checked
/// against bytes later. See [`Decision::digest`].
pub fn record_override(
    ctx: &Ctx,
    package: &str,
    commit: &str,
    digest: &str,
    grade: Option<u8>,
    reason: &str,
) -> Result<(), String> {
    let decision = Decision {
        kind: Kind::OverrideBlock,
        package: package.to_string(),
        commit: commit.to_string(),
        digest: Some(digest.to_string()),
        grade,
        // Stored verbatim: neutering is a rendering decision, and a record
        // that quietly rewrote what a human wrote would be a poor record.
        // Every reader on every host renders it through `visible_line`.
        reason: reason.trim().to_string(),
        host: ctx.host.clone(),
        at: now_rfc3339(),
        // A decision this version writes has nothing this version does not
        // know about. `extra` carries a *newer* pacrat's fields through this
        // one's rewrites; it is never something to invent on the way in.
        extra: Default::default(),
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
    const DIGEST: &str = "be8938a589c9413c771a08b6886ef80cb4d0c8fe7acc376646ab310a5e6ea59a";

    #[test]
    fn a_recorded_override_reads_back_and_the_next_one_joins_it() {
        let (_s, ctx) = store("append");
        record_override(
            &ctx,
            "mdcat",
            COMMIT,
            DIGEST,
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
        record_override(&ctx, "yay", COMMIT, DIGEST, None, "second").unwrap();
        let ledger = ctx.load_decisions().unwrap();
        assert_eq!(ledger.decisions.len(), 2);
        assert_eq!(ledger.decisions[0].package, "mdcat");
        assert_eq!(ledger.decisions[1].grade, None);
    }

    /// One writer, so one shape. The TUI and the CLI both reach the ledger
    /// through `record_override` and nothing else — the value of that is
    /// that an entry cannot say which surface made it, and this is the test
    /// that keeps it true if somebody adds a second path.
    ///
    /// It compares the *keys*, not the values: host and timestamp differ by
    /// construction, and it is the schema the fleet parses.
    #[test]
    fn both_surfaces_write_the_same_entry_shape() {
        let (_s, ctx) = store("one-shape");
        record_override(&ctx, "mdcat", COMMIT, DIGEST, Some(4), "from the CLI").unwrap();
        record_override(&ctx, "widget", COMMIT, DIGEST, Some(4), "from the TUI").unwrap();

        let ledger = ctx.load_decisions().unwrap();
        let keys = |d: &Decision| {
            let mut k = vec!["kind", "package", "commit", "reason", "host", "at"];
            if d.digest.is_some() {
                k.push("digest");
            }
            if d.grade.is_some() {
                k.push("grade");
            }
            k.sort_unstable();
            k
        };
        assert_eq!(keys(&ledger.decisions[0]), keys(&ledger.decisions[1]));
        // And the digest really is in the file rather than only in the type.
        let raw = fs::read_to_string(ctx.decisions_path()).unwrap();
        assert_eq!(raw.matches("digest = ").count(), 2, "{raw}");
        assert!(raw.contains(DIGEST), "{raw}");
    }

    /// The writer refuses a digest the ledger would refuse, at the door
    /// rather than at the next parse — the rule every other field here
    /// already follows.
    #[test]
    fn a_digest_the_ledger_would_reject_is_never_written() {
        let (_s, ctx) = store("bad-digest");
        assert!(record_override(&ctx, "mdcat", COMMIT, "../../PWNED", Some(4), "x").is_err());
        assert!(record_override(&ctx, "mdcat", COMMIT, "", Some(4), "x").is_err());
        assert!(!ctx.decisions_path().exists(), "a refusal wrote a file");
    }

    /// A reason is stored as written and *rendered* safely. Storing a
    /// neutered copy would quietly rewrite what a human said; rendering the
    /// raw bytes would let them paint the terminal of the person auditing.
    #[test]
    fn a_hostile_reason_is_stored_verbatim_and_cannot_forge_a_row() {
        let (_s, ctx) = store("hostile");
        let hostile = "looks fine\x1b[2K\noverride  none — adopted cleanly";
        record_override(&ctx, "mdcat", COMMIT, DIGEST, Some(4), hostile).unwrap();

        let raw = fs::read_to_string(ctx.decisions_path()).unwrap();
        assert!(raw.contains("looks fine"), "{raw}");

        let d = &ctx.load_decisions().unwrap().decisions[0];
        let shown = truncate(&visible_line(&d.reason).0, WHY_CAP);
        assert!(!shown.contains('\x1b'), "{shown:?}");
        assert_eq!(shown.lines().count(), 1, "{shown:?}");
    }

    /// The writer is where forward compatibility is actually tested: this
    /// host reads a file a newer pacrat wrote, appends its own decision, and
    /// writes the whole thing back. A field it does not understand has to
    /// come out the other side, because the alternative is one stale machine
    /// deleting the fleet's record as a side effect of an unrelated override.
    #[test]
    fn appending_a_decision_does_not_strip_a_newer_pacrats_fields() {
        let (_s, ctx) = store("forward-compat");
        fs::write(
            ctx.decisions_path(),
            format!(
                "[[decision]]\n\
                 kind = \"override-block\"\n\
                 package = \"mdcat\"\n\
                 commit = \"{COMMIT}\"\n\
                 reason = \"read it\"\n\
                 host = \"slab\"\n\
                 at = \"2026-08-09T00:00:00Z\"\n\
                 signature = \"phase-2-adds-this\"\n"
            ),
        )
        .unwrap();

        record_override(&ctx, "yay", COMMIT, DIGEST, Some(4), "mine").unwrap();

        let raw = fs::read_to_string(ctx.decisions_path()).unwrap();
        assert!(raw.contains("phase-2-adds-this"), "field dropped:\n{raw}");
        let ledger = ctx.load_decisions().unwrap();
        assert_eq!(ledger.decisions.len(), 2);
        assert_eq!(ledger.decisions[0].extra.len(), 1);
        assert!(ledger.decisions[1].extra.is_empty());
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
        assert!(record_override(&ctx, "mdcat", COMMIT, DIGEST, Some(4), "x").is_err());
    }

    /// The writer refuses what the ledger's own validator refuses — the
    /// check is core's, and this is the door being shut before the file is
    /// touched rather than after.
    #[test]
    fn a_decision_the_ledger_would_reject_is_never_written() {
        let (_s, ctx) = store("reject");
        assert!(record_override(&ctx, "../escape", COMMIT, DIGEST, Some(4), "x").is_err());
        assert!(record_override(&ctx, "mdcat", "HEAD~1", DIGEST, Some(4), "x").is_err());
        assert!(record_override(&ctx, "mdcat", COMMIT, DIGEST, Some(4), "   ").is_err());
        assert!(!ctx.decisions_path().exists(), "a refusal wrote a file");
    }
}
