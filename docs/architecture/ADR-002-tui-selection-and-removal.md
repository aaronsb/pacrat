# ADR-002: TUI selection and package removal

**Status:** Accepted, amended same day (2026-08-10 — the selection language
below stands as written; the removal design is superseded by the Amendment
at the end, which implementation follows)
**Date:** 2026-08-10
**Extends:** ADR-001 (custody ladder, self-only sync, never-elevate, the
childlock amendment)

## Context

The custody ladder has verbs that walk packages up — `add` tracks, `vendor`
curates — and nothing that walks them down. Removal today is implicit:
`pacrat sync --prune` plans an uninstall for anything installed but
untracked, so removing a package means hand-editing the tracked list and
then noticing the prune. There is no way to say "these twelve, gone" as one
deliberate act, the way `pacman -Rns a b c` says it.

Separately, the hosts screen grew pacrat's first multi-select: `space` marks
a row with `•` and appends its name to a "selected" list in the detail pane.
Two problems, both structural. The list displaces the package information
the pane exists to show, and it grows without bound — twelve selections is a
pane of names. And the mechanism is local to one screen, while selection is
plainly coming to others (updates wants "adopt these", jobs may want "dismiss
these"); a second screen inventing its own marks and its own keys would be
two selection languages for one reader.

Bulk removal is also the most destructive thing the TUI will do. A
select-all followed by a reflexive confirm could untrack a host's whole
manifest and print the uninstall for it. ADR-001's amendment to decision 2
already names the house posture for this class of risk: friction is accident
resistance against accidental dismissal, not access control.

## Decision

### Selection is one language, spoken per screen

Every screen that selects uses the same model and the same controls; what a
selection *means* stays screen-local (on hosts it is "these packages, this
host's manifest"; a future updates selection would mean "these candidates").

- **State:** a name-keyed set, as hosts already does — a reload can reorder
  rows, and a selection that silently became different packages is the one
  bug a bulk screen must not have.
- **Controls:** `space` toggles the cursor row, plus one keyset everywhere
  for select-all, select-none, and invert. Exact keys are chosen at
  implementation against the keymap table (candidates: `*` all, `-` none,
  `!` invert) — the decision here is that they are *the same on every
  screen*.
- **Visual:** a selected row gets a background tint *and* keeps a marker
  glyph in column 0. The tint is the affordance; the glyph is the one that
  survives a monochrome terminal, the same rule the verdict marks follow.
- **The detail pane is never the selection's ledger.** It always shows the
  cursor row's information. Selection state is a count in the region title
  (`matrix · 12 marked`), because a count is what the fence below needs a
  reader to have read, and a list of names is what `apply` shows anyway.

### The flow is select → action → apply

An action key over a selection does not act; it shows what applying would do
— the command-block pattern the screens already use — and names the count.
Apply is then a single act over the whole selection. One decision, one
answer, however many packages.

### Removal: untrack here, print the uninstall

New verb `pacrat remove <pkg>… [--yes]`, with the hosts screen's `x` as its
TUI face.

- **What it does:** removes the packages from *this host's* tracked list —
  a store write, through the same writer `add` uses, so the two directions
  of the ladder cannot disagree about the file — and then prints one
  combined `sudo pacman -Rns pkg1 pkg2 …` for the operator to run. pacrat
  never elevates (ADR-001), so the uninstall is always the operator's act.
- **Fleet convergence is self-only** (ADR-001 decision 4): other hosts that
  track the package converge by running `pacrat sync --prune` themselves
  after pulling the store. Removal grows no remote-execution surface.
- **Scope:** removal is about this host's manifest and this host's machine.
  A selected row that is neither tracked here nor installed here is refused
  in plain words rather than silently skipped.
- **Out of scope:** retiring a *curated* package — deleting its store tree,
  its sources-ledger entry, its grading cache — is a different operation
  with fleet-wide consequences, and gets its own ADR when it is wanted.

### The fence: childlock above a configured count

Applying a destructive action to more than `removal_fence` packages
(config.toml key, default **5**) requires the TUI childlock — the same
hold-against-a-real-clock that guards the BLOCK override. At or below the
fence, the normal confirm prompt, naming every package.

Per the ADR-001 amendment, the hold is accident resistance, not access
control: it makes dismissing a large destructive apply deliberate, and does
not pretend to stop automation — the CLI's `--yes` exists on purpose, and a
script that wants to untrack fifty packages may say so. The CLI prompt
(without `--yes`) names every package and the count; with stdin not a
terminal and no `--yes`, the answer is no and the verb holds, the same rule
every other asking verb follows.

## Consequences

- `add`/`remove` symmetry completes the manifest lifecycle; the ladder now
  walks both ways below the curation rungs.
- The hosts screen becomes the second place the TUI writes to the store
  (after the updates screen's decision ledger), and follows the same
  precedent: the CLI verb's own writer, called unchanged — one file, one
  shape, two surfaces.
- The selection rework touches every current selecting screen (hosts today)
  and sets the contract future screens inherit; the keys and the tint land
  in the shared keymap/theme vocabulary, not per screen.
- A new config key (`removal_fence`) joins thresholds in config.toml, with
  the same validation posture: a nonsense value is a parse error, not a
  guess.

## Amendment (2026-08-10): removal is drift reconciliation, not a verb

Ratified by Aaron the same day, before implementation, superseding the
"Removal" and "fence" sections above. The selection language and the
select → action → apply flow stand unchanged.

The rethink, in his words: *"we're only tracking the delta"* — something
*tracked but no longer on the system* is the actual removal surface — and
*"we don't install things with pacrat, so we shouldn't uninstall things
with pacrat."*

pacrat never owned the machine's package state; pacman does, and removal
happens there, with the tool that owns it. What pacrat owns is the
manifest. The drift state **tracked here, not installed here** is genuinely
ambiguous — it means either "new manifest entry, install me" or "I removed
this on purpose and the manifest is stale" — and only a human can say
which. So the interface is not an imperative remove action; it is that
drift, presented, with a resolving act in each direction:

- **Manifest wins:** `pacrat sync` — already exists, prints the install.
- **Reality wins:** **`pacrat untrack <pkg>…`** — new, and nothing more
  than `add`'s exact inverse: a manifest-only edit to this host's tracked
  list, through the same writer, so the two directions cannot disagree
  about the file.

What this deletes from the design above:

- **No `pacrat remove`.** Untrack carries no removal framing: it edits the
  manifest and prints no pacman command. A package untracked while still
  installed is simply demoted to unmanaged, and `sync --prune`'s printed
  plan surfaces the uninstall where it always did — run by the operator,
  never by pacrat.
- **No childlock fence, no `removal_fence` config key.** The store is
  git-backed and an untrack is a revertable file edit; the actually
  destructive step (prune's `-Rns`) is typed by a human anyway. The guard
  that remains is a confirm that names every package and the count — and
  only where the selection was *accumulated* rather than typed:

  - **TUI apply** (hosts `x` over the marked rows): show the names and the
    count, ask, then write through the CLI verb's writer, clear the
    selection, and say what changed and what to do next (commit the store;
    anything still installed appears in `sync --prune`'s plan).
  - **CLI `pacrat untrack`** takes no prompt at all, exactly like `add`:
    the operator typed the names, and repeating them back as a question
    adds nothing. Unknown or untracked names are refused in plain words.

- The hosts matrix already draws the removal surface — the `✗` cell is
  "tracked here, not installed here" — so the screen's job is to say what
  the drift means on the detail pane of such a row: removed by hand?
  untrack accepts that; `pacrat sync` reinstalls it.
