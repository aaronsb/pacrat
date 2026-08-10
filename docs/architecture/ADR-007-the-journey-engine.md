# ADR-007: the journey engine — use-case flows as the UX

**Status:** Accepted (2026-08-10 — direction is Aaron's: the use cases guide
the UX flow, and the flows are the next work; implementation follows this
document)
**Date:** 2026-08-10
**Extends:** ADR-002 (selection), ADR-006 (parity — actions complete where
they started)
**Source of truth:** `docs/use-cases.md` — the six journeys this ADR turns
into surfaces. Issue #28 is the running requirements thread.

## Context

The parts exist; the narrative does not. An operator on a fresh host — or
mid-loop — has no surface that answers "what do I do now?" The use-cases
document names six journeys and states that every surface decision must be
traceable to one of them. This ADR is the tracing: what gets built so the
journeys are walkable without reading the documentation.

## Decision

### The next-step engine

A pure function from store-and-host state to a short, prioritized list of
next steps — each step a sentence, a key (or command), and the journey it
belongs to. Pure and screen-free, so it is testable as a table and both
surfaces read the same answer (`pacrat status` gains the same "next" lines
the overview shows; two surfaces, one answer, the house rule).

The rungs, in priority order (first match leads; the list shows at most a
handful):

1. **Not set up** → "run `pacrat setup`" (the gate already enforces this;
   the engine only has to say it kindly on `about`'s hint line).
2. **Enrollment** — unmanaged AUR packages installed here → "N AUR
   packages are in nobody's manifest — track them all" (journey 1; bulk,
   once).
3. **Threat** — a BLOCK grading on file for a *store* tree (installed
   bytes, not a candidate) → name the package, point at the incident
   journey (journey 5's installed case; until the advisory ADR exists,
   the step points at review + the manual response).
4. **Drift to decide** — pending candidates → "N candidates drifted —
   `[3]`, enter reviews" (journey 2); held rows are named separately from
   fresh ones.
5. **Curation debt** — vendored-but-ungraded, adopted-but-unbuilt →
   the owed verb, per package counts (journey 2).
6. **Manifest drift** — `✗`/`+` rows on this host → "reconcile on `[4]`"
   (journey 3).
7. **All quiet** → say so, with the last update run's outcome — silence
   earned is stated, not implied.

### Enrollment, made one act

- **Overview**: when rung 2 leads, its step is pressable — a key on the
  overview opens the enrollment confirm (count + names via the
  no-silent-clip list), and apply goes through `add::adopt` exactly as
  hosts `A` does. One confirm, no childlock (ADR-002's amendment applies:
  a manifest edit, git-revertable).
- **Hosts screen**: the selection language gains **class select** —
  select every row in a drift class. Keys chosen at implementation
  against the keymap (candidate: `*` cycles all → class-of-cursor-row →
  none, or a dedicated key); the decision here is that "all unmanaged
  AUR" is one keystroke, not seventy-five.
- **CLI**: `pacrat add --all-installed` gains `--source aur|native|flatpak`
  scoping. Enrollment's spelling is `pacrat add --all-installed --source
  aur`; bare `--all-installed` keeps its current meaning (everything) —
  a flag narrowing, not a behavior change.

### The journeys the engine does not automate

Retirement (journey 6) and the installed-threat response (journey 5's
second half) remain manual paths documented in use-cases.md; their rungs
in the engine point at the documentation's steps until their own ADRs
land. The engine states what is owed; it does not invent verbs this ADR
has not designed.

### Placement

The engine lives in pacrat-core (state in, steps out — no I/O); the
overview renders it as the screen's first region, `pacrat status` prints
it as a trailing `next` block. The overview keeps its existing attention
surfaces below the steps; the steps are the narrative, not a replacement
for the data.

## Consequences

- The overview's question — "what needs my attention?" — finally has the
  journey-shaped answer; a fresh machine's first five minutes are
  self-explanatory.
- The engine's table test is the use-cases document made executable: each
  journey's states map to its rungs, and a new journey means a new rung
  with a test.
- The rig (ADR-005) gains a scenario per rung as they land — enrollment
  first: fixture with unmanaged AUR rows → the step leads → the key
  applies → the step retires itself.
- `--source` scoping touches `add`'s door only; the writer and the
  grammar are unchanged.
- Issue #28 closes when the engine and enrollment land; class-select
  rides the same branch.
