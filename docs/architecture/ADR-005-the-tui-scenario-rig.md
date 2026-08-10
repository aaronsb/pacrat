# ADR-005: the TUI scenario rig

**Status:** Accepted (2026-08-10 — shape agreed with Aaron in conversation;
implementation follows this document)
**Date:** 2026-08-10
**Extends:** ADR-001 (every external call is argv — the property that makes
hermetic scenarios possible), ADR-002 (the selection language this must
prove)

## Context

First real use of the TUI surfaced two defects in one sitting — a search
whose results the arrows could not walk, and focus jumps nothing on screen
made visible — and both were of the class *a key does not do what the
screen advertises*. Unit tests cannot catch that class: the keymap table
was tested, the screens were tested, and the seam between them — focus,
geometry, what the terminal actually renders — was tested by nobody but
the first human.

Aaron's ask, from having built exactly this before: a harness that drives
the TUI with injected keys, reads the UI back in a form assertions can
hold onto, runs as a pile of scenarios, and leaves a real record — and it
must isolate the terminal environment completely and exercise real
geometries, narrow and wide.

Prior art places the ask: `expect` (the pty-driving granddaddy),
Microsoft's tui-test (Playwright-style scenarios over a headless
terminal), Charm's VHS/teatest (scripted keys, golden renders), and in
ratatui's own world, `TestBackend` buffer assertions. asciinema is not a
test runner — it cannot inject or assert — but its cast format is the
right *evidence* format: replayable by a human when a golden diff needs
judging.

## Decision

### A pty rig is the authority

Scenarios run the **real binary** under a real pseudo-terminal at a chosen
geometry, inside the demo-grade sandbox — `env -i`, temp store and XDG
paths, `PACRAT_SETUP_GATE=off`, and PATH shims standing in for pacman,
git, and the AUR (the always-argv rule is what makes this possible; an
HTTP call would have needed a mock server, an argv call needs a shell
script). Keys are written to the pty; the byte stream is fed to a
terminal state machine; assertions read the resulting **cell grid** —
text, attributes, cursor — which is "the values in the UI in the right
form".

- **`portable-pty`** (wezterm's) spawns, sizes, and resizes the pty —
  mid-scenario resize included, so SIGWINCH and the redraw path are
  exercised for real.
- **`vt100`** consumes the stream and holds the grid. What it renders is
  what a capable terminal renders; if pacrat emits broken escapes, the
  grid shows it, which no `TestBackend` test can.
- **The geometry matrix is part of the scenario**, not a fixture detail:
  the default set is 80×24, 130×42, and 40×15, and a scenario may add its
  own. A screen that collapses at 40 columns fails here, not on a phone
  ssh session.

Both crates are dev-dependencies only; nothing ships in the binary.

### The record is dual

Every checkpoint can snapshot the grid to a plain-text golden file
(diffable, reviewed like any code change), and every run can emit an
asciinema cast (v2 is JSON lines — the rig writes it directly). The
golden answers "did it change"; the cast answers "what did the human
watching it see". Goldens live beside the scenarios; casts are artifacts,
written to the target directory and kept by CI on failure.

### Scenarios are code first, data later

The first form is Rust: `cargo test` functions over a small driver API —
`spawn(geometry, fixture)`, `send(keys)`, `wait_for(pattern)`,
`snapshot(name)`, `resize(w, h)` — because that ships inside the existing
test battery and CI with zero new toolchain. A data-driven scenario file
(the Office-9 shape: a list of keys and expectations someone writes
without recompiling) can grow on top of the API once it has settled;
inventing the file format first is how harnesses die young.

Waiting is by condition, never by sleep: `wait_for` polls the grid for a
pattern with a deadline. A scenario that needs a fixed delay to pass is a
scenario that flakes on CI, and the rig refuses to offer the primitive.

### `TestBackend` remains the inner layer

Fast in-process tests (keymap dispatch, screen state) keep using ratatui's
`TestBackend` where they already do. The rig does not replace them; it
owns the seam they cannot see. Pass/fail authority for "does the key do
what the screen advertises" is the rig's.

### The first scenarios are the two defects

1. Search on browse, then arrows — the cursor walks rows and the detail
   pane follows (the focus-follows-results fix).
2. Tab around every screen — the inverse-video focus chip is on exactly
   one title, and it moves where Tab says.
3. Enter on a browse row — detail fills and focus stays on the results
   (the focus policy).
4. The hosts selection language — space/`*`/`-`/`!`, marks surviving a
   reload, `A`/`x` confirm overlays naming the count — at 80×24 and
   40×15.

A regression that reached a human first becomes a scenario the same day;
that is the rig's maintenance rule.

## Consequences

- Two dev-dependencies (`portable-pty`, `vt100`), both maintained, both
  small; `cargo audit` runs with the addition.
- CI grows one suite that runs the scenario battery on both jobs; the
  arch job's container needs nothing new — the sandbox carries its own
  world.
- The adversarial-review habit of hand-driving tmux does not go away; it
  is where new scenario ideas come from. The rig is where they stop being
  hand work.
- Goldens are geometry-keyed; a deliberate visual change touches a
  readable text file in the same PR, which is the review surface working.
