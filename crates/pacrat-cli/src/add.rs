//! `pacrat add` — adopt installed packages into the manifest: the
//! unmanaged → tracked rung of the custody ladder. Writes the host's
//! tracked list in the store; committing the store is the user's act.

use std::fs;

use pacrat_core::pkg::Source;

use crate::ctx::Ctx;
use crate::live;

pub fn run(ctx: &Ctx, packages: &[String], host: Option<&str>) -> Result<(), String> {
    let host = host.unwrap_or(&ctx.host);
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
        for pkg in &adds {
            if !list.iter().any(|t| t == pkg) {
                list.push(pkg.to_string());
            }
        }
        if list.len() == before {
            println!("{}: already tracked on {host}", source.name());
            continue;
        }
        list.sort();
        let dir = ctx.packages_dir().join(host);
        fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let path = dir.join(format!("{}.txt", source.name()));
        fs::write(&path, list.join("\n") + "\n").map_err(|e| format!("{}: {e}", path.display()))?;
        println!(
            "{}: tracked {} on {host} ({} → {} packages) — {}",
            source.name(),
            adds.join(", "),
            before,
            list.len(),
            path.strip_prefix(&ctx.store).unwrap_or(&path).display()
        );
    }
    println!("note: commit the store to sync this to other hosts");
    Ok(())
}
