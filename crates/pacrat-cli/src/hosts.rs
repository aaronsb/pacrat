//! `pacrat hosts` — the CLI twin of the hosts screen: n-way comparison of
//! tracked lists across every host in the store.

use pacrat_core::pkg::{nway_diff, Source};

use crate::ctx::Ctx;
use crate::out::list_preview;

pub fn run(ctx: &Ctx) -> Result<(), String> {
    let hosts = ctx.tracked_hosts();
    if hosts.is_empty() {
        return Err(format!(
            "no host lists under {}",
            ctx.packages_dir().display()
        ));
    }

    for source in Source::ALL {
        let lists: Vec<(String, Vec<String>)> = hosts
            .iter()
            .map(|h| Ok((h.clone(), ctx.tracked(h, source)?)))
            .collect::<Result<_, String>>()?;
        let participating: Vec<&str> = lists
            .iter()
            .filter(|(_, l)| !l.is_empty())
            .map(|(h, _)| h.as_str())
            .collect();
        if participating.is_empty() {
            continue;
        }
        let n = nway_diff(&lists);
        println!(
            "{} — {} hosts ({}) · {} common",
            source.name(),
            participating.len(),
            participating.join(", "),
            n.common
        );
        for (host, unique) in &n.unique {
            if unique.is_empty() {
                continue;
            }
            let marker = if *host == ctx.host { "*" } else { " " };
            println!(
                " {marker}{:<10} only here ({}): {}",
                host,
                unique.len(),
                list_preview(unique, 10)
            );
        }
        println!();
    }
    Ok(())
}
