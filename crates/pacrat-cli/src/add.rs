//! `pacrat add` — adopt installed packages into the manifest: the
//! unmanaged → tracked rung of the custody ladder. Writes the host's
//! tracked list in the store; committing the store is the user's act.
//!
//! [`adopt`] is the whole edit and [`run`] is only its mouth: the TUI's
//! bulk-adopt apply calls `adopt` unchanged (the `record_override`
//! precedent — one writer, two surfaces), and both directions of the
//! ladder go through [`crate::ctx::Ctx::save_tracked`], so `add` and
//! `untrack` cannot disagree about the file.

use std::path::PathBuf;

use pacrat_core::pkg::Source;

use crate::ctx::Ctx;
use crate::live;

/// One tracked list edited: which source, which names, and the counts a
/// reader checks the edit by. Shared with `untrack`, whose changes are the
/// same shape in the other direction.
#[derive(Debug)]
pub struct Change {
    pub source: Source,
    /// The names this edit added or removed, in list order.
    pub packages: Vec<String>,
    pub before: usize,
    pub after: usize,
    /// The list file that was written.
    pub path: PathBuf,
}

pub fn run(ctx: &Ctx, packages: &[String], host: Option<&str>) -> Result<(), String> {
    let host = host.unwrap_or(&ctx.host);
    let changes = adopt(ctx, host, packages)?;
    if changes.is_empty() {
        println!("already tracked on {host} — the manifest is unchanged");
        return Ok(());
    }
    for c in &changes {
        println!(
            "{}: tracked {} on {host} ({} → {} packages) — {}",
            c.source.name(),
            c.packages.join(", "),
            c.before,
            c.after,
            c.path.strip_prefix(&ctx.store).unwrap_or(&c.path).display()
        );
    }
    println!("note: commit the store to sync this to other hosts");
    Ok(())
}

/// Adopt `packages` into `host`'s tracked lists, and say what changed.
///
/// Names already tracked are not an error and not a change — adopting is
/// idempotent, and an empty answer means the manifest already said all of
/// this. A name that is not installed here at all *is* an error, because
/// adopt records reality rather than intent.
pub fn adopt(ctx: &Ctx, host: &str, packages: &[String]) -> Result<Vec<Change>, String> {
    if packages.is_empty() {
        return Err("nothing to add".into());
    }

    // Where is each package actually installed? Source is detected, not
    // guessed: a package must be present in exactly one live set.
    let live: Vec<(Source, Vec<String>)> = Source::ALL
        .iter()
        .map(|s| live::installed(*s).map(|list| (*s, list)))
        .collect::<Result<_, _>>()?;

    let mut planned: Vec<(Source, &str)> = Vec::new();
    for pkg in packages {
        let found: Vec<Source> = live
            .iter()
            .filter(|(_, list)| list.binary_search(pkg).is_ok())
            .map(|(s, _)| *s)
            .collect();
        match found.as_slice() {
            [one] => planned.push((*one, pkg)),
            [] => {
                return Err(format!(
                    "{pkg} is not installed on this host — adopt records reality, \
                     it does not install (that's `pacrat sync`)"
                ))
            }
            many => {
                return Err(format!(
                    "{pkg} appears in multiple sources ({}) — file an issue, \
                     this should be impossible",
                    many.iter().map(|s| s.name()).collect::<Vec<_>>().join(", ")
                ))
            }
        }
    }

    let mut changes = Vec::new();
    for source in Source::ALL {
        let adds: Vec<&str> = planned
            .iter()
            .filter(|(s, _)| *s == source)
            .map(|(_, p)| *p)
            .collect();
        if adds.is_empty() {
            continue;
        }
        let mut list = ctx.tracked(host, source)?;
        let before = list.len();
        let mut recorded: Vec<String> = Vec::new();
        for pkg in &adds {
            if !list.iter().any(|t| t == pkg) {
                list.push(pkg.to_string());
                recorded.push(pkg.to_string());
            }
        }
        if list.len() == before {
            continue;
        }
        let path = ctx.save_tracked(host, source, &list)?;
        changes.push(Change {
            source,
            packages: recorded,
            before,
            after: list.len(),
            path,
        });
    }
    Ok(changes)
}
