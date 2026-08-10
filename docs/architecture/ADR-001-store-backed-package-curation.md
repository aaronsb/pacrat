# ADR-001: pacrat — store-backed package curation with graded updates

**Status:** Accepted (2026-08-10 — the custody ladder, serving model, guard,
grading contract, and review flow are implemented and adversarially reviewed;
the once-open questions are decided below)
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

> The normative specification is `docs/grading-contract.md`, which is
> tool-neutral and implementable without reference to any particular
> analyzer. This section records the decision and the reasoning; where the
> two disagree about a detail, the spec is what pacrat implements.

PROCEED/WARN/BLOCK is **pacrat's data**. Graders — any tool that speaks the
contract, or a human via `pacrat grade --grade N --note …` — return only:

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
holds in auto mode** — failure is never Proceed; every invocation is logged
argv-verbatim in the jobs view. yay-friend needs only an output-format
adapter — its commit-hash cache already matches the contract's idempotency
model.

**A grading is about bytes, not about a commit.** The cache key above is
necessary and not sufficient: a commit says what upstream published, not what
is in the store now, and the store tree can be edited by a hand, a sync, or a
half-finished `git` operation without any of the three key parts changing. So
every cached grading also records a **tree digest** — SHA-256 over the sorted,
length-prefixed (relative path, contents) pairs of the tree — and a read whose
digest does not match the tree on disk is a **miss**, not a hit and not an
error. Without this, editing a PKGBUILD after grading serves the old PROCEED
forever, which is the whole gate defeated by one text editor. This applies to
a human's `--grade` exactly as it does to a tool's: a reading is about the
bytes that were read.

**Aggregation: worst-wins, but only among gradings entitled to answer.**
Splitting the two halves of the question is what makes the rule safe:

1. *Did anybody answer?* Only a **configured** grader or a **human** can. A
   cached grading from a grader this host no longer has configured is
   evidence, not an answer — otherwise deleting a grader from the config
   makes every package it ever touched look permanently graded. With no
   answer the verdict is Ungraded, which holds.
2. *How bad is it?* **Worst-wins over every grading**, including the retired
   one (settles open question 5).

The asymmetry is deliberate: evidence that is not allowed to reassure is
still allowed to alarm. A retired grader's leftover file can make a verdict
worse and can never make one exist. Manual is a human's own reading and
always answers, which is also what lets a recorded `--grade` be read back on
a host with no external graders configured at all.

Grader output is untrusted twice over — malformed by accident, or steered by
the PKGBUILD it was asked to judge. Three bounds follow, none of them
optional: its **size** is capped (4 MB of stdout; the grader picks that
number otherwise, and `cat /dev/zero` is a request for all the memory on the
machine), its **text** is neutered *and* flattened to one line before display
(a newline in a finding title otherwise forges a line of pacrat's own report,
verdict included), and its **subject** is checked — a valid grading of some
other package is not a grading of this one. A grader may also **pin its
scale** in config; a report declaring a different one is ungraded rather than
rescaled, because a tool that silently moved from 0-4 to 0-100 would
otherwise have every grade quietly become four times less alarming.

### Update loop and gates

current → drifted (drift check on launch or timer) → graded (async dispatch
against the candidate commit) → decide → build → served. The decide/grade/build
transitions are **gates**, and the preset is what a user actually sets. Two
invariants no preset changes: BLOCK always holds, and external calls are
always visible. `pacrat update --mode auto` is the whole loop headless (exit
0 clean, 10 holds present, 1 failure; `--format json` for machines) — the
timer entry point that keeps gradings warm before a human ever looks.

**The presets, as implemented** (`config::Mode`, `update_mode`):

|          | auto  | semi (default) | manual |
|----------|-------|----------------|--------|
| PROCEED  | adopt | adopt          | ask    |
| WARN     | hold  | ask            | ask    |
| BLOCK    | hold  | hold           | hold   |
| UNGRADED | hold  | hold           | ask    |

An earlier draft of this section described semi as "grade auto, decide
manual", which read literally would ask about a clean PROCEED too. It does
not, and the reason is the only currency a gate spends: **attention**. A
prompt on every clean verdict is how a person learns to answer without
reading, and the answer they stop reading is the WARN in the middle of the
run. Semi therefore prompts exactly where a prompt carries information;
`manual` remains for someone who wants to see each one, and adds a second
question before the build.

The three per-transition knobs collapse into this one word deliberately. In
practice they are not independent — a host that wants to be asked about a
decision wants to be asked before the build that follows it — and three
orthogonal settings would let a user configure combinations that mean
nothing (decide manual, build auto: approve a candidate and have it served
before you finish reading). One word, three coherent postures.

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

clap + ratatui in the CLI crate; core stays thin — serde/toml for the models,
serde_json for the grading contract, sha2 for the tree digest. "Thin" is not
"none", and the digest is where that distinction got tested: a hand-written
SHA-256 would have kept core's dependency list shorter while putting bespoke
crypto in the part of pacrat that decides what to trust, which is the exact
trade this whole ADR argues against making. Curated upstream over bespoke,
including when it costs us ten crates on the lock. git, makepkg, aurutils and
graders remain subprocesses with argv logging — the dotfiles-tui split, kept
deliberately.

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

## Decisions on the once-open questions

Settled with Aaron, 2026-08-10, after the first three implementation waves —
each answer below was made against working code, not speculation.

1. **Bulk adopt: `pacrat add --all-installed`.** An explicit first-run flag
   that adopts everything currently installed into this host's lists, to be
   pruned at leisure. Deliberate, one command, reversible by editing the
   lists. (The TUI's multi-select adopt remains for batches thereafter.)

2. **Block override exists, in both surfaces, behind friction that cannot be
   piped.** The CLI form is `--override-block --reason "…"` — its friction
   is authoring the justification. The TUI form requires *holding* a key
   until a progress bar fills (a single keypress does nothing, so no script
   or paste can trip it) and then a typed justification. Both record the
   override in the decision ledger (below). The TUI affordance appears only
   after the friction, never as a plain keybinding.

   > **Amendment (2026-08-10), on implementing the TUI form.** The
   > parenthesis above claims more than any hold-to-confirm mechanism can
   > deliver, and the implementation should not be read as delivering it.
   > "No script or paste can trip it" is false: a script that opens a pty and
   > paces its writes — one `sleep` between keystrokes — satisfies every
   > timing condition a hold can test, and one was written to confirm it
   > writes a ledger entry with no human present. No further hardening is
   > proposed, for two reasons.
   >
   > First, it would buy nothing. The childlock guards a door that stands
   > beside an open one: anyone able to drive a pty can invoke `pacrat
   > adopt-update --override-block --reason "…"` directly and
   > non-interactively, which this decision deliberately provides. A TUI gate
   > that resisted automation would not make an override harder to automate;
   > it would only make the CLI the way everyone automates it.
   >
   > Second, every sharper timing test is also a sharper way to lock a real
   > keyboard out on hardware nobody has tested, and a lock that a person
   > cannot open is a lock that gets removed.
   >
   > **What the TUI form does deliver, and what the decision should be read
   > as asking for:** the override cannot be reached by a single keystroke, a
   > misclick, a stray repeat, a naive paste, or anything arriving on a pipe
   > rather than a terminal. It is *accident resistance* for an irreversible,
   > fleet-visible act — not access control. That is the right bar because it
   > matches the threat model this decision actually has: the adversary is
   > the machine's own owner, later, having forgotten, and against them the
   > **record** is the control and the friction is only what makes the record
   > deliberate. Both surfaces still write that record, which is the part of
   > this decision that carries the weight.

3. **Jobs run in-process only.** Background gradings and builds live only as
   long as a pacrat process does; headless warmth is `pacrat update` on a
   systemd timer. No daemon, no IPC surface. Revisit only if the TUI feels
   starved.

4. **Sync is self-only.** Each host runs pacrat for itself; the hosts matrix
   is read-only awareness of the others; the store syncs via git. pacrat
   gains no remote-execution surface.

5. **Aggregation stays worst-wins with the quorum rule; manual does not
   outrank tools.** A human grading is one voice — it can give quorum and
   raise, never suppress a tool's BLOCK. Suppressing a BLOCK is exactly what
   the override path is for, and two doors would blur the audit trail.
   Weights and vetoes remain unbuilt.

6. **Served mirrors require signatures — with a recorded, per-package trust
   escape.** `SigLevel = Required DatabaseOptional` stands for any repo with
   `repo.server` set; package signing (makepkg --sign + key distribution) is
   phase-2 work that gates fleet serving. A user may still choose to trust a
   specific unsigned package, but that choice goes through the same friction
   mechanism as a block override and is recorded in the decision ledger.
   Local-only repos keep `Optional TrustAll` — nothing crosses a network.

7. **Guard marker semantics confirmed as implemented:** pid+timestamp,
   honored only while the pid is alive and under an hour, every use and
   every stale-ignore announced, forgeable by design — a rail against
   mistakes, not access control. Surfacing marker state in `pacrat status`
   arrives with the TUI work.

### The decision ledger

Decisions 2 and 6 share a shape and therefore a mechanism: a human accepting
a named risk — overriding a BLOCK, trusting an unsigned package — passes
through deliberate friction and leaves a record: what was accepted, for
which package and commit/artifact, when, and the stated reason. The record
lives with the store (synced, reviewable, greppable), because a risk one
host accepted is a fact the fleet should be able to read. The exact file
shape lands with the first implementer (the one-shot update loop's override
path); what is decided here is that both flows write to the same ledger
rather than growing parallel bookkeeping.

**The shape, as implemented.** `aur/decisions.toml`, an append-only list
beside `sources.toml`:

```toml
[[decision]]
kind = "override-block"     # or "trust-unsigned" — modelled, writer is phase 2
package = "mdcat"
commit = "5a4705a4…"        # validated exactly as `reviewed` is
grade = 4                   # the overridden verdict's worst grade; absent if none
reason = "…"                # 1-500 chars, required, neutered at every render
host = "north"
at = "2026-08-10T12:34:56Z" # UTC, second precision, the only accepted spelling
```

Three properties are load-bearing and were chosen rather than fallen into. A
decision names a **commit**, not a package, because "we decided mdcat was
fine" is not a statement anybody can act on later. There is **no expiry, no
revocation and no inheritance** by a later commit: the gate re-asks its
question every time, which is what keeps one accepted risk from becoming a
standing exemption. And the list is **append-only with a re-read before every
write**, so a second host's record cannot be erased by the first host's next
override. `pacrat decisions` lists it, `pacrat info <package>` shows the ones
about that package, and nothing removes an entry but a human editing the file.

**A record can outlive the act it was made for.** The entry is written after
the human answers and before the store is touched, so an adoption that then
fails — a full disk, a tree that will not install — leaves a decision on file
for something that did not happen. That ordering is deliberate: of the two
one-sided outcomes, "we recorded accepting a risk we did not end up taking"
is a false positive a reader can dismiss, while "we took a risk and recorded
nothing" is the audit trail failing at the one moment it exists for. The
failure message says so in as many words, because a reader who is told only
that the install failed will not think to go and look at the ledger.

**Unknown fields are carried, unknown kinds are refused, and the asymmetry
implies an upgrade order.** An entry's unrecognised *fields* survive a rewrite
by an older pacrat (`extra`), so a machine a version behind cannot silently
delete what a newer one recorded. An unrecognised *kind* fails the whole
parse, because a decision this binary cannot interpret is not one it may act
around. Together those mean: once one host records a kind that older hosts do
not know, those hosts cannot record an override — or read the ledger at all —
until they upgrade. That is the correct direction to fail, and it is a real
operational consequence: **roll a new decision kind out to the fleet before
the first host writes one.**
