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
pacman.conf section, and a PreTransaction hook that aborts foreign installs
that didn't come through pacrat. The curated path is the only path, including
against brain-farts.

**How the hook decides.** The hook is `NeedsTargets` + `AbortOnFail`; its Exec
script reads the target names on stdin and blocks any target that resolves in
**no sync database** (`pacman -Sl`). That set difference is the signature of a
helper building an AUR package and handing the file to `pacman -U`: official
repos and `[dotfiles-aur]` itself are both sync repos, so the curated path
passes untouched and only the bypass path is caught.

**Not an env marker.** An earlier draft of this section said "env-marker
recognized". That cannot work: pacman hooks run as root and do not inherit the
invoking user's environment, so `yay → sudo → pacman` drops `PACRAT_BYPASS`
before the hook ever sees it. An env check alone is a guard that never fires.
The two escapes are therefore:

1. **A marker file** in the repo directory, `.pacrat-transaction`, holding
   `pid=<pid> started=<unix seconds>`. **Contract for `build` and `sync`:**
   write it before invoking pacman, remove it after. It is honoured only while
   that pid is alive and the timestamp is under an hour, and every use is
   announced on stderr — an interrupted build leaves a stale marker, and a
   stale marker that silently disabled the guard forever would be worse than
   no guard. Anyone who can write the repo directory can forge one; this is a
   rail against mistakes, not an access control.
2. **`PACRAT_BYPASS=1`**, which only survives when carried across sudo
   deliberately (`PACRAT_BYPASS=1 sudo -E pacman -U …`) and is logged. The
   verbosity is the point: there is no way to set it once and forget.

**Known limit (accepted for now).** The check is by package *name*. Rebuilding
a name the curated repo already carries and installing that file with
`pacman -U` passes, because the name resolves. Comparing version and identity —
so that a curated name still has to match the curated *build* — is future
hardening, not part of this decision.

**Deferred: repo-first yay config.** An earlier draft listed this among the
guards. It is not implemented and not decided. Ordering within pacman.conf
already determines who wins a name collision (`pacrat setup` appends, so
official repos win, and says so), and making curated builds shadow official
packages is a deliberate manual edit. Whether yay should additionally be
pinned repo-first is left open.

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
- **System:** `/var/cache/pacrat/repo` (repo db + built packages, plus the
  `.pacrat-transaction` marker while a pacrat transaction is running),
  `/etc/pacman.d/hooks/pacrat-guard.hook` and its Exec script
  `/usr/share/pacrat/pacrat-guard.sh`. `pacrat setup` never writes these
  itself — it stages them under XDG state and prints the `sudo install`
  commands, because pacrat does not elevate.

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
