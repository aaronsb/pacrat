//! `pacrat status` — the CLI twin of the overview screen: store, ledger
//! counts, per-host tracked sizes, and this host's drift per source.

use pacrat_core::pkg::Source;
use pacrat_core::sources::Role;

use crate::ctx::Ctx;
use crate::live;
use crate::out::list_preview;
use crate::setup;

pub fn run(ctx: &Ctx) -> Result<(), String> {
    let sources = ctx.load_sources()?;

    println!("store  {}", ctx.store.display());
    println!("host   {}", ctx.host);
    let repo = &ctx.config.repo;
    // One shared probe with `pacrat setup`, so the two verbs cannot disagree
    // about whether this host is serving: a repo directory alone is not
    // "set up" if pacman has never been told about it or the guard is absent.
    let setup_state = setup::state(repo);
    let note = if setup_state.complete() {
        String::new()
    } else if setup_state.untouched() {
        "  (not set up — `pacrat setup`)".into()
    } else {
        format!(
            "  (partly set up, missing {} — `pacrat setup`)",
            setup_state.missing().join(" + ")
        )
    };
    println!("repo   [{}] {}{note}", repo.name, repo.path);
    println!(
        "ledger vendored {} · maintained {}{}",
        sources.count(Role::Vendored),
        sources.count(Role::Maintained),
        if sources.packages.is_empty() {
            "  (no aur/sources.toml yet)"
        } else {
            ""
        }
    );

    let hosts = ctx.tracked_hosts();
    if hosts.is_empty() {
        println!("hosts  none tracked under {}", ctx.packages_dir().display());
        return Ok(());
    }

    println!();
    println!("tracked lists");
    for host in &hosts {
        let marker = if *host == ctx.host { "*" } else { " " };
        let sizes: Vec<String> = Source::ALL
            .iter()
            .map(|s| Ok(format!("{} {}", s.name(), ctx.tracked(host, *s)?.len())))
            .collect::<Result<_, String>>()?;
        println!(" {marker}{host:<10} {}", sizes.join(" · "));
    }

    if !hosts.iter().any(|h| h == &ctx.host) {
        println!();
        println!(
            "note   this host ({}) has no tracked lists yet — `pacrat add` will create them",
            ctx.host
        );
        return Ok(());
    }

    println!();
    println!("drift on {} (tracked vs installed)", ctx.host);
    for sd in live::host_drift(ctx)? {
        let d = &sd.drift;
        if d.in_sync() {
            println!(
                "  {:<8} in sync ({} packages)",
                sd.source.name(),
                sd.tracked.len()
            );
        } else {
            println!(
                "  {:<8} {} missing · {} extra   [{}]",
                sd.source.name(),
                d.missing.len(),
                d.extra.len(),
                live::query_argv(sd.source)
            );
            if !d.missing.is_empty() {
                println!("           missing: {}", list_preview(&d.missing, 12));
            }
            if !d.extra.is_empty() {
                println!("           extra:   {}", list_preview(&d.extra, 12));
            }
        }
    }
    println!();
    println!("       `pacrat sync` turns that into commands that move this host toward");
    println!("       the store — it prints them and runs none of them");
    Ok(())
}
