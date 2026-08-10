# ADR-006: TUI/CLI parity — every action completes in the surface it started in

**Status:** Accepted (2026-08-10 — the principle is Aaron's, stated on first
real use; implementation follows this document)
**Date:** 2026-08-10
**Extends:** ADR-001 (surfaces; the always-print rule), ADR-002 (the
select → action → apply flow), ADR-003 (the sudo flow, the elevate session)

## Context

Pressing `s` on the hosts screen printed a suggestion to leave the TUI and
run `pacrat sync` in a shell. Aaron's reading: *asking the user to switch
out of the TUI to perform actions is incorrect — we should be able to
fully work in the TUI, or the CLI.*

The suggest-a-command pattern was honest when it was written: the screens'
module doc explains that every command pacrat runs is printed first, that
a verb driven behind the alternate screen prints into a buffer ratatui
owns, and that two verbs stop to ask on stdin, which raw mode cannot
answer. All three reasons have since been dissolved by machinery this
project built for other decisions:

- the **jobs log** captures chatter behind the alternate screen and shows
  it on `[5]`, satisfying always-print (the updates screen's review
  already routes through it);
- the **confirm overlay** asks in the TUI what a verb would ask on stdin
  (ADR-002's applies already do);
- the **elevate session** shows, confirms, and runs sudo commands
  (ADR-003), and sudo's one hard requirement — a real terminal for its
  password prompt — is met by *suspending* the TUI, not by leaving it.

What remains are leftovers: six `suggest_*` sites that print a command
and tell the reader the rest is theirs.

## Decision

### The principle

Every action pacrat offers is completable in the surface it was started
in. The CLI has full verbs; the TUI drives those same verbs' own
functions — one writer, one shape, per the record_override and
ADR-002-apply precedents — and never answers a keypress with homework.
Suggested command lines may still *appear* (they teach the CLI spelling),
but always beside a working key, never instead of one.

### The suspension pattern

For flows that need the real terminal — sudo's password prompt, and any
verb whose output is a document to read at full width (vendor's PKGBUILD
review) — the TUI **suspends**: leaves the alternate screen, restores the
terminal, runs the flow exactly as the CLI would (same printing, same
confirms), then re-enters and reloads. The pattern tig and magit
established; the reader never loses their place, and the always-print
rule is satisfied on the primary screen where those lines belong.

### The mapping

| Screen · key | Today | Becomes |
|---|---|---|
| hosts · `s` | suggests `pacrat sync` | suspend; walk the plan through the elevate session (`sync --run`'s own walk); re-enter, reload |
| browse · `t` | suggests `pacrat add` | apply through `add::adopt` with the naming confirm (the hosts `A` precedent) |
| browse · `v` | suggests `pacrat vendor` | suspend; run vendor's own interactive review; re-enter, reload |
| browse · `i` | suggests `sudo pacman -S` | suspend; one elevate-session command; re-enter, reload |
| updates · `a` | suggests `pacrat adopt-update` | job-queue the fetch (the review step already does), confirm overlay for the adopt question, apply through `review::adopt`'s core; chatter to the jobs log |
| updates · `x` | suggests `pacrat reject` | confirm overlay naming the commit, apply through `review::reject`'s core |
| jobs · retry/probe | suggests `pacrat push --retry` / probe | job-queue the same functions; the queue pane is already the natural home |

Order of implementation follows irritation: hosts `s` first (the reported
case), then browse `t`/`i`, then updates `a`/`x`, then vendor's
suspension, then jobs. Each lands with its scenario in the ADR-005 rig as
the rig comes up.

### What does not change

- The CLI remains the scripting surface and the reference spelling;
  nothing becomes TUI-only.
- pacrat still never elevates unasked and never holds credentials; the
  suspension hands sudo the same terminal the CLI would have.
- Actions that print more than they do (sync without `--run`) keep their
  read-only forms; the TUI's key drives the acting form.

## Consequences

- The screens' module-doc rationale for suggest-only is rewritten to
  state this principle and the three pieces of machinery that retired it.
- The TUI gains one new primitive (suspend/resume around a closure) with
  the terminal-state discipline the childlock review already tests for
  (stty sane on every path out).
- `suggest_*` functions shrink to the "beside a working key" teaching
  line or disappear.
- The demo eventually re-records showing work completing inside the TUI.
