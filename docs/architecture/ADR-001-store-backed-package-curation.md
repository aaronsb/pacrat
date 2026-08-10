# ADR-001: pacrat — store-backed package curation with graded updates

**Status:** Draft
**Date:** 2026-08-09
**Design mockup:** `docs/design/mockup-rev3.html` (screen-by-screen TUI mockup; treat it as this ADR's appendix)

## Context

The June 2026 AUR incident (200+ packages compromised via orphan adoption and
malicious takeovers; uploads and ssh still read-only months later) made the
default AUR posture — helper fetches HEAD, human squints, makepkg runs —
untenable for a fleet of Arch machines (cube, north, padnoir, slab) carrying
60+ foreign packages.

The pieces of a better posture already exist, separately:

- **dotfiles** (the store) syncs shared state across machines; its CLI's `pkg`
  subcommand tracks per-host package lists with n-way drift math — but package
  management is out of scope for a *files* tool (dotfiles-cli ADR-007 spirit).
- **yay-friend** (Go, ours) analyzes PKGBUILDs with an AI provider and caches
  verdicts keyed by AUR git commit hash.
- **aurutils** is a composable toolbox for fetch/review/build into a local
  pacman repo — its philosophy (AUR packages become repo packages) is exactly
  the serving model we want.
- **clicue's publish flow** (`packaging/publish-aur.zsh`) is the pattern for
  pushing our own packages back to the AUR, including the tarball-tamper alarm.
- A custom pacman repo served from GitHub releases (`[clicue]`) is already in
  production on this fleet.

What does not exist: the integration — one manifest, one custody model, one
review gate, one place the machines trust.

## Decision

Build **pacrat**: a Rust workspace (pacrat-core pure model, pacrat-cli
clap + ratatui binary) that owns package curation end to end and orchestrates
the existing tools underneath. It nests in the dotfiles store like
dotfiles-tui does: its own repo, gitignored by the store.

### Custody ladder

Every package is in exactly one state, and the state is the UI's organizing
concept: **unmanaged → tracked → vendored → maintained**. Adopt puts a package
in the manifest; vendor commits its PKGBUILD tree to the store at a reviewed
commit; maintained adds AUR push rights. Official-repo packages top out at
tracked. Browsing shows identical metadata (description, upstream, install
dates from each host's pacman db) at every rung.

### Serving model

`pacrat build` runs makepkg (via aurutils) and repo-adds into a local pacman
repo `[dotfiles-aur]`. Once a vendored package exists there, pacman/yay treat
it as a repo package and stop consulting the AUR for it. **A held update is
simply never built** — hosts keep the last approved version with zero pinning.
Binaries never enter git.

### Guard rails, not script integration

pacrat is invisible to update scripts (update-arch, `yay -Syu` run unchanged).
What it owns are the guards `pacrat setup` deploys: the `[dotfiles-aur]`
pacman.conf section, a PreTransaction hook that aborts foreign installs that
didn't come through pacrat (env-marker recognized; `PACRAT_BYPASS=1` is the
logged escape), and a repo-first yay config. The curated path is the only
path, including against brain-farts.

### Grading contract (pacrat-grade/v1)

PROCEED/WARN/BLOCK is **pacrat's data**. Graders — yay-friend, future tools,
or a human via `pacrat grade --grade N --note …` — return only:

```json
{ "contract": "pacrat-grade/v1",
  "grader": "yay-friend",
  "subject": { "package": "...", "commit": "...", "version": "..." },
  "grade": 0, "scale": { "min": 0, "max": 4 },
  "findings": [ { "level": 0, "title": "...", "span": "PKGBUILD:3" } ],
  "meta": { "provider": "claude", "duration_s": 41, "cached": true } }
```

Rules: pacrat maps grade→verdict with its own thresholds (default warn≥2,
block≥4); cache key is (grader, package, commit) and a moved AUR HEAD
invalidates the grading; grader failure/timeout/bad JSON = **ungraded, which
holds in auto mode** — failure is never Proceed; multiple graders aggregate
worst-wins (open question 5); every invocation is logged argv-verbatim in the
jobs view. yay-friend needs only an output-format adapter — its commit-hash
cache already matches the contract's idempotency model.

### Update loop and gates

current → drifted (drift check on launch or timer) → graded (async dispatch
against the candidate commit) → decide → build → served. The decide/grade/build
transitions are **gates**, each auto | verbose | manual; presets bundle them:
manual, semi-auto (grade auto, decide manual), auto. Two invariants no preset
changes: BLOCK always holds, and external calls are always visible.
`pacrat update --mode auto` is the whole loop headless (exit 0 clean, 10 holds
present, 1 failure; `--format json` for machines) — the timer entry point that
keeps gradings warm before a human ever looks.

### Surfaces

One binary, two faces: bare `pacrat` opens the `default_ui` preference; any
subcommand is always CLI, and every TUI action has a CLI twin. Six screens on
number keys: overview (triage), browse (custody-aware search), updates
(diff-since-reviewed beside gradings; upstream region flags tracked-not-
vendored AUR drift), hosts (the lifted n-way matrix, bulk adopt), jobs
(argv-verbatim log, publish queue with AUR-ssh probe), config (drawn gate
pipeline, graders, sources, guards). Scrolling is first-class: every region is
a viewport (j/k, ctrl-d/u, ctrl-f/b, g/G, scrollbar + position indicator,
tab moves focus). Help's about tab renders `art/petit-chef.html`'s 38×32 grid
as half-blocks (16 rows, truecolor→256→absent when !isatty or NO_COLOR);
`--help` stays plain for scripts and screen readers.

### Data placement

- **Store (shared, synced):** `aur/packages/<name>/` pristine PKGBUILD trees;
  `aur/sources.toml` (upstream url, reviewed commit, role, adopted grades);
  `packages/<host>/` manifest lists (lifted from dotfiles `pkg`).
- **XDG config (host preference):** `~/.config/pacrat/config.toml` —
  default_ui, gate presets, thresholds, graders, repo path. Itself deployable
  as a dotfiles entry.
- **XDG state (host):** job/publish queues, probe history.
- **System:** `/var/cache/pacrat/repo` (repo db + built packages),
  `/etc/pacman.d/hooks/pacrat-guard.hook`.

No version pin couples pacrat to the store: its store data is plain text,
TOML, and pristine PKGBUILD trees. (This also removes one of the couplings
that motivated dotfiles-cli ADR-200.)

### Dependencies

clap + ratatui in the CLI crate; core stays dependency-free (serde/toml when
the sources model lands). git, makepkg, aurutils, and graders are subprocesses
with argv logging — the dotfiles-tui split, kept deliberately.

## Consequences

- dotfiles-cli retires `pkg` (short ADR + pin bump there); dotfiles returns to
  files only. The set math moves here.
- aurutils becomes a runtime dependency; it lives in the AUR, so it is the
  first vendored package (bootstrapped once by hand per new machine, or from
  the fleet's repo thereafter).
- yay-friend gains `--format pacrat`; until then a wrapper script adapts its
  cache JSON.
- clicue-style publishing generalizes to `pacrat push` for maintained
  packages; blocked until AUR ssh returns (probe is built in).
- Migration is incremental: adopt/vendor packages as their updates come due,
  not big-bang.

## Open questions

1. **Bulk adopt ergonomics** — first-run "adopt everything, then prune" mode?
2. **Block override** — CLI-only `--override-block --reason` recorded in
   sources.toml, or no override at all?
3. **Jobs runtime** — TUI-process-only, or a user service owning the queue?
4. **Sync transport** — remote sync over ssh, or each host syncs itself?
5. **Multi-grader aggregation** — worst-wins, or weights/vetoes (does manual
   outrank tools)?
