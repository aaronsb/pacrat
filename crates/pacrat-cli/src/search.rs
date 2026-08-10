//! `pacrat search` — the CLI twin of the browse screen (mockup §4): one
//! table over both worlds, official repos and the AUR, with pacrat's custody
//! column answering "and what is our relationship to it?".
//!
//! The two worlds are queried independently and merged. The AUR half is
//! allowed to fail: a host offline or an RPC outage costs you the AUR rows
//! and a warning, not the command.

use pacrat_core::Custody;

use crate::aur::{self, AurPkg};
use crate::ctx::Ctx;
use crate::custody::{self, Index};
use crate::out::truncate;
use crate::pacman::{self, SyncHit};

/// How many AUR hits a table shows. The AUR answers broad terms with
/// hundreds; the rest are counted, never silently dropped.
const AUR_CAP: usize = 25;

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
    let repo_hits = pacman::search(term)?;
    let aur_hits = aur::search(term);

    let mut rows: Vec<Row> = repo_hits
        .iter()
        .map(|h| repo_row(h, term, &index))
        .collect();

    let (aur_rows, aur_overflow, aur_error) = match &aur_hits {
        Ok(hits) => {
            let mut ranked: Vec<&AurPkg> = hits.iter().collect();
            // Popularity descending, exact match first, name as the
            // tie-break so equal-popularity rows do not shuffle run to run.
            ranked.sort_by(|a, b| {
                (a.name != term)
                    .cmp(&(b.name != term))
                    .then_with(|| {
                        b.popularity
                            .unwrap_or(0.0)
                            .total_cmp(&a.popularity.unwrap_or(0.0))
                    })
                    .then_with(|| a.name.cmp(&b.name))
            });
            let overflow = ranked.len().saturating_sub(AUR_CAP);
            let rows: Vec<Row> = ranked
                .into_iter()
                .take(AUR_CAP)
                .map(|p| aur_row(p, term, &index))
                .collect();
            (rows, overflow, None)
        }
        Err(e) => (Vec::new(), 0, Some(e)),
    };
    rows.extend(aur_rows);

    // Repo-before-AUR is already the vector's order; a stable sort on
    // exactness alone lifts the searched-for name without disturbing it.
    rows.sort_by_key(|r| !r.exact);

    if rows.is_empty() {
        // With half the search dead, "no matches" would be a claim pacrat
        // cannot make.
        let scope = if aur_error.is_some() {
            " in the official repos"
        } else {
            ""
        };
        println!("no matches for {term}{scope}");
    } else {
        print_table(&rows);
    }
    if aur_overflow > 0 {
        println!("… and {aur_overflow} more on the AUR");
    }
    if let Some(e) = aur_error {
        println!("warning: the AUR search failed ({e}) — repo results only");
    }
    Ok(())
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
