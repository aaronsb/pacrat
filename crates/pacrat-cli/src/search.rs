//! `pacrat search` — the CLI twin of the browse screen (mockup §4): one
//! table over both worlds, official repos and the AUR, with pacrat's custody
//! column answering "and what is our relationship to it?".
//!
//! The two worlds are queried independently and merged, and *either* half is
//! allowed to fail: an RPC outage costs you the AUR rows, a rejected search
//! term costs you the repo rows, and in both cases you keep the other half
//! plus a warning saying what you lost. Half an answer beats none — and a
//! half that already cost a network round trip should not be thrown away
//! because the other half failed afterwards.

use pacrat_core::Custody;

use crate::aur::{self, AurPkg};
use crate::ctx::Ctx;
use crate::custody::{self, Index};
use crate::out::truncate;
use crate::pacman::{self, SyncHit};

/// How many hits each half of the table shows. Both worlds answer a broad
/// term with far more than fits a screen — pacman reads the term as an ERE,
/// so `c++` alone matches thousands — and the same cap on both keeps neither
/// side able to bury the other. The remainder is counted, never silently
/// dropped.
const AUR_CAP: usize = 25;
const REPO_CAP: usize = 25;

/// Total column budget. The CLI prints, the TUI scrolls (ADR-001).
const WIDTH: usize = 100;

struct Row {
    name: String,
    version: String,
    source: String,
    custody: Option<Custody>,
    description: String,
    /// Searched for this package by name — it goes first whatever else sorts.
    exact: bool,
}

pub fn run(ctx: &Ctx, term: &str) -> Result<(), String> {
    if term.trim().is_empty() {
        return Err("nothing to search for".into());
    }

    // Both calls are announced before they run: the RPC can sit for its
    // whole timeout, and a user staring at a blank terminal deserves to know
    // what is being waited on.
    let rpc_url = aur::search_url(term);
    println!("  {}", pacman::search_argv(term));
    println!("  {}", aur::argv(&rpc_url));
    println!();

    let index = Index::build(ctx)?;
    let repo_hits = pacman::search(term);
    let aur_hits = aur::search(term);

    // pacman already orders by repo priority then name; rank_and_cap only
    // lifts an exact match out of it.
    let (mut rows, repo_overflow, repo_error) = match &repo_hits {
        Ok(hits) => {
            let rows = hits.iter().map(|h| repo_row(h, term, &index)).collect();
            let (rows, overflow) = rank_and_cap(rows, REPO_CAP);
            (rows, overflow, None)
        }
        Err(e) => (Vec::new(), 0, Some(e)),
    };

    let (aur_rows, aur_overflow, aur_error) = match &aur_hits {
        Ok(hits) => {
            let mut ranked: Vec<&AurPkg> = hits.iter().collect();
            // Popularity descending, with the name as a tie-break so
            // equal-popularity rows do not shuffle from run to run.
            ranked.sort_by(|a, b| {
                b.popularity
                    .unwrap_or(0.0)
                    .total_cmp(&a.popularity.unwrap_or(0.0))
                    .then_with(|| a.name.cmp(&b.name))
            });
            let rows = ranked
                .into_iter()
                .map(|p| aur_row(p, term, &index))
                .collect();
            let (rows, overflow) = rank_and_cap(rows, AUR_CAP);
            (rows, overflow, None)
        }
        Err(e) => (Vec::new(), 0, Some(e)),
    };
    rows.extend(aur_rows);

    // Repo-before-AUR is already the vector's order; a stable sort on
    // exactness alone lifts the searched-for name without disturbing it.
    rows.sort_by_key(|r| !r.exact);

    if rows.is_empty() {
        // With half the search dead, a bare "no matches" would claim more
        // than pacrat knows.
        let scope = match (repo_error.is_some(), aur_error.is_some()) {
            (false, false) => "",
            (false, true) => " in the official repos",
            (true, false) => " on the AUR",
            (true, true) => " — neither half of the search ran",
        };
        println!("no matches for {term}{scope}");
    } else {
        print_table(&rows);
    }
    if repo_overflow > 0 {
        println!("… and {repo_overflow} more in the official repos");
    }
    if aur_overflow > 0 {
        println!("… and {aur_overflow} more on the AUR");
    }
    // Neither warning promises what survived — with both halves down, any
    // such promise would be wrong, and the table above already shows it.
    if let Some(e) = &repo_error {
        println!("warning: the official-repo search failed ({e})");
    }
    if let Some(e) = &aur_error {
        println!("warning: the AUR search failed ({e})");
    }
    // One half down is degradation; both halves down means the command could
    // not answer the question at all, and a script deserves to know.
    if repo_error.is_some() && aur_error.is_some() {
        return Err(format!("could not search either world for {term}"));
    }
    Ok(())
}

/// Lift any exact name match to the front, then cap, reporting how many rows
/// were left out. The sort is stable, so the caller's own ranking — pacman's
/// repo priority, or the AUR's popularity order — survives underneath it.
///
/// Promotion happens *before* the cap on purpose: the package you typed the
/// name of must never be the one the cap drops.
fn rank_and_cap(mut rows: Vec<Row>, max: usize) -> (Vec<Row>, usize) {
    rows.sort_by_key(|r| !r.exact);
    let overflow = rows.len().saturating_sub(max);
    rows.truncate(max);
    (rows, overflow)
}

fn repo_row(hit: &SyncHit, term: &str, index: &Index) -> Row {
    Row {
        custody: index.custody(&hit.name),
        exact: hit.name == term,
        name: hit.name.clone(),
        version: hit.version.clone(),
        source: hit.repo.clone(),
        description: hit.description.clone(),
    }
}

fn aur_row(pkg: &AurPkg, term: &str, index: &Index) -> Row {
    Row {
        custody: index.custody(&pkg.name),
        exact: pkg.name == term,
        name: pkg.name.clone(),
        version: pkg.version.clone(),
        source: "aur".into(),
        description: pkg.description.clone().unwrap_or_default(),
    }
}

fn print_table(rows: &[Row]) {
    let width = |header: &str, f: fn(&Row) -> &str, lo: usize, hi: usize| {
        rows.iter()
            .map(|r| f(r).chars().count())
            .chain(std::iter::once(header.len()))
            .max()
            .unwrap_or(lo)
            .clamp(lo, hi)
    };
    let name_w = width("name", |r| &r.name, 4, 32);
    let ver_w = width("ver", |r| &r.version, 3, 18);
    let src_w = width("source", |r| &r.source, 6, 10);
    let state_w = "maintained".len();
    let desc_w = WIDTH
        .saturating_sub(name_w + ver_w + src_w + state_w + 4)
        .max(20);

    // A blank custody cell or description would otherwise leave the padding
    // dangling off the end of the line.
    let line = format!(
        "{:<name_w$} {:<ver_w$} {:<src_w$} {:<state_w$} {}",
        "name", "ver", "source", "state", "description"
    );
    println!("{}", line.trim_end());
    for r in rows {
        let line = format!(
            "{:<name_w$} {:<ver_w$} {:<src_w$} {:<state_w$} {}",
            truncate(&r.name, name_w),
            truncate(&r.version, ver_w),
            truncate(&r.source, src_w),
            custody::label(r.custody),
            truncate(&r.description, desc_w),
        );
        println!("{}", line.trim_end());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(names: &[&str], exact: &str) -> Vec<Row> {
        names
            .iter()
            .map(|n| Row {
                name: (*n).to_string(),
                version: "1-1".into(),
                source: "extra".into(),
                custody: None,
                description: String::new(),
                exact: *n == exact,
            })
            .collect()
    }

    fn names(rows: &[Row]) -> Vec<&str> {
        rows.iter().map(|r| r.name.as_str()).collect()
    }

    #[test]
    fn under_the_cap_nothing_is_dropped_or_reordered() {
        let (kept, overflow) = rank_and_cap(rows(&["a", "b", "c"], ""), 25);
        assert_eq!(names(&kept), ["a", "b", "c"]);
        assert_eq!(overflow, 0);
        // Exactly at the cap is still a clean answer, not a tail of zero.
        let (kept, overflow) = rank_and_cap(rows(&["a", "b"], ""), 2);
        assert_eq!(kept.len(), 2);
        assert_eq!(overflow, 0);
    }

    #[test]
    fn over_the_cap_counts_the_remainder() {
        // `c++` against the sync dbs really does return thousands.
        let many: Vec<String> = (0..12_746).map(|i| format!("pkg{i}")).collect();
        let refs: Vec<&str> = many.iter().map(String::as_str).collect();
        let (kept, overflow) = rank_and_cap(rows(&refs, ""), REPO_CAP);
        assert_eq!(kept.len(), REPO_CAP);
        assert_eq!(overflow, 12_746 - REPO_CAP);
    }

    #[test]
    fn the_exact_match_is_never_what_the_cap_drops() {
        // Buried past the cap by the caller's own ranking.
        let mut names_in: Vec<String> = (0..40).map(|i| format!("pkg{i}")).collect();
        names_in.push("wanted".into());
        let refs: Vec<&str> = names_in.iter().map(String::as_str).collect();
        let (kept, overflow) = rank_and_cap(rows(&refs, "wanted"), REPO_CAP);
        assert_eq!(kept[0].name, "wanted");
        assert_eq!(kept.len(), REPO_CAP);
        assert_eq!(overflow, 41 - REPO_CAP);
        // Everything else keeps the order it arrived in.
        assert_eq!(names(&kept)[1..4], ["pkg0", "pkg1", "pkg2"]);
    }

    #[test]
    fn an_empty_half_has_no_tail() {
        let (kept, overflow) = rank_and_cap(Vec::new(), REPO_CAP);
        assert!(kept.is_empty());
        assert_eq!(overflow, 0);
    }
}
