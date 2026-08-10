# pacrat-grade/v1

The interface between pacrat and any tool that evaluates a package tree.

This is the normative specification. ADR-001 decided the contract and argues
for it; this document is what you implement against, and it is deliberately
free of any particular tool — a grader is any program that satisfies what
follows, and pacrat contains no knowledge of which one you chose.

## The division of labor

A grader answers one question: **how alarming is this tree, on a scale it
declares?** That is all it is permitted to say.

PROCEED / WARN / BLOCK is **pacrat's** data, derived from that number by the
host's own thresholds and nowhere else. A grader does not recommend, does not
gate, and cannot approve. This split is the whole point of the contract: risk
posture is a config edit on one machine, not a property of the tool that did
the reading, and two hosts with different thresholds must be able to reach
different decisions from the same grading.

Anything the grader wants a human to *see* rather than *decide with* goes in
`findings` and `meta`, which are advisory and never move the grade.

## Invocation

pacrat runs the grader as a subprocess, once per (package, commit), with a
working directory you must not rely on.

**argv.** The `cmd` in config is an argv template, not a command line. Every
element is substituted independently and handed straight to `exec`; there is
no shell on this path, so a value containing spaces, quotes or `;` stays
exactly one argument and there is nothing to escape for. Three placeholders
are substituted anywhere they appear in an argument:

| Placeholder | Value |
|---|---|
| `{package}` | The package name. `[A-Za-z0-9@._+-]`, never leading `-` or `.` |
| `{tree}` | Absolute path to the tree to read: PKGBUILD, `.SRCINFO`, install scripts, patches |
| `{commit}` | The upstream commit the tree is at, as a hex object id (7–64 chars) |

The program itself — `cmd[0]` — may not contain a placeholder. What runs is
never chosen by the subject being judged.

You may declare the placeholders in any order and may omit ones you do not
need, but a grader that ignores `{commit}` cannot honor the subject-match
rule below, and a grader that ignores `{tree}` is not reading the bytes
pacrat is asking about.

**stdout** carries exactly one JSON object: the report. Nothing else, ever —
no progress lines, no banner, no trailing log. If your tool prints, redirect
its output to stderr.

**stderr** is for diagnostics. pacrat captures it, bounds it at 64 KB, and is
never parsed. One truncated line of it is shown **only** when the grader
exits nonzero or dies of a signal — those are the two failures where the
grader's own words are the best available explanation. Every other failure
(timeout, unparseable stdout, the 4 MB cap, the wrong subject, a scale that
does not match the pin, a spawn error) reports pacrat's reason and discards
stderr, so do not rely on stderr to explain a malformed report: put the
explanation in the exit status instead, by declining rather than printing
something that will not parse.

**stdin** is `/dev/null`. A grader that prompts fails fast rather than
hanging on a terminal that is not listening. Do not prompt; there is no human
attached, and in the update loop there may not be one for hours.

**Exit status.** Zero means "the JSON on stdout is my answer". Any nonzero
exit means "I have no answer", and pacrat records the grader as having
produced nothing. That is the correct way to decline — see *Refusing to
answer*.

**Environment** is inherited. Read your own config and cache from the usual
XDG locations; pacrat does not pass any of its own state.

## The report

```json
{ "contract": "pacrat-grade/v1",
  "grader": "example-grader",
  "subject": { "package": "mdcat", "commit": "3f9c21ab55de", "version": "2.1.1-1" },
  "grade": 0,
  "scale": { "min": 0, "max": 4 },
  "findings": [ { "level": 0, "title": "no network in build()", "span": "PKGBUILD:3" } ],
  "meta": { "provider": "claude", "duration_s": 41, "cached": true } }
```

**`contract`** (string, required) — exactly `"pacrat-grade/v1"`. Compared
byte for byte: no trailing space, no other case, no other version. A report
written to a contract pacrat does not know is not read at all, because
guessing at the meaning of an unknown dialect is how a grade gets
misinterpreted in the safe direction.

**`grader`** (string, required, non-empty) — who produced this. Informational;
pacrat files the grading under the name in *its own* config, and only
mentions yours if the two differ.

**`subject`** (object, required) — what was graded.

- `package` (string, required, non-empty) — the package name you were given.
- `commit` (string, required, non-empty) — the commit you were given. May be
  abbreviated; seven characters is the shortest prefix that means anything.
- `version` (string, optional) — the package version, if you know it.
  Display only.

**`grade`** (integer, required) — how alarming the tree is, on `scale`.
Higher is worse. Must lie within your own declared scale.

**`scale`** (object, optional; defaults to `{"min":0,"max":4}`) — the range
`grade` lives on. `min` must be less than `max`. Declare it explicitly even
when it is pacrat's own: it costs one line and it makes a later change to
your output visible instead of silent.

> **Every number in this document is an integer in 0–255.** `grade`,
> `scale.min`, `scale.max` and a finding's `level` are all read as `u8`.
> A negative number, a number above 255, or a fractional one is not an
> out-of-range value that gets clamped or rounded — it fails to parse, and
> the **whole grading** is discarded with a message about types rather than
> about grades. Two practical consequences: a scale wider than 0–255 cannot
> be expressed at all, and a single malformed finding level throws away the
> grade that came with it. Clamp your own numbers before you print them.

**`findings`** (array, optional, defaults to empty) — things a human should
look at. Advisory: findings never move the grade, the grade moves the
verdict.

- `level` (integer, optional, defaults to 0) — severity on your scale. Not
  validated *against your declared scale*, deliberately: a grader that
  mislabels one annotation should still have its grade honored, since
  throwing away a whole grading over a cosmetic bug would convert it into a
  hold. This is tolerance about the scale only — the 0–255 bound above still
  applies, and a level outside it does discard the grading.
- `title` (string, optional, defaults to empty) — one line. See *Text*.
- `span` (string, optional) — where to look, e.g. `PKGBUILD:3`. Free text.

pacrat shows the five worst findings per grader and counts the rest, so rank
them yourself if order matters to you: ties keep your order.

**`meta`** (object, optional) — anything you want recorded about the run:
provider, model, duration, your own cache hit. Opaque to pacrat and preserved
verbatim, with one convention — a `meta.note` string is printed under the
result, so put your one-line summary there if you have one.

**Unknown fields are fine**, at every level. Hosts in a fleet run different
pacrat versions, and a newer grader adding a field must not break an older
reader. Forward compatibility is tolerance about *additions* only: it does
not extend to changing what an existing field means.

## What pacrat rejects

Grader output is untrusted twice over — malformed by accident, and steerable
by the very PKGBUILD the grader was asked to judge. Each of the following
makes the grading nothing at all, not a bad grading:

1. **A foreign contract string.** As above.
2. **A grade outside your own declared scale.** `7` on a `0-4` scale is not a
   very bad grade, it is a broken grader.
3. **A degenerate scale**, where `min >= max` — a grade on it carries no
   information.
4. **A grading that identifies nothing** — empty `grader`, empty
   `subject.package`, or empty `subject.commit`.
5. **A grading of another subject.** `subject.package` must equal the package
   you were asked about, and `subject.commit` must match the commit you were
   asked about (prefix comparison, case-insensitive, seven characters
   minimum). The cache is keyed by pacrat's idea of the subject; a report
   about something else would be filed under that key and read later as a
   grading of this tree.
6. **A scale that moved**, when the host pinned one. See *Pinning*.
7. **Too much output.** stdout is capped at 4 MB. A grading is a few
   kilobytes of JSON; the bound exists because the grader chooses that number
   otherwise, and an unbounded read from a pipe is a request for all the
   memory on the machine at exactly the moment pacrat is judging something
   hostile.
8. **Taking too long.** The host configures a timeout, default 300 seconds.
   A timeout is reported as a timeout, never as a partial grading.
9. **Anything that is not JSON**, or is JSON but not this shape.

There is no lenient path and no partial credit. A grading pacrat does not
fully understand is not a grading.

One exception, and it is about bytes rather than meaning: stdout is decoded
**lossily**, so invalid UTF-8 inside a string becomes U+FFFD and the grading
is accepted with that string mangled. Encoding is your responsibility — a
title spliced out of a PKGBUILD in some other encoding will be displayed
with replacement characters, not rejected.

## Requirements on a grader

**Answer about the tree you were given.** `{tree}` is the bytes under
judgement — not upstream HEAD, not the AUR's current copy, not a version
resolved by name. If your tool can only look at something else, compare what
it looked at against `{commit}` and decline when they differ. Filing an
analysis of one tree under another tree's name is worse than no analysis: it
puts a grading pacrat trusts in front of bytes nobody read.

**Be deterministic per (package, commit).** The same tree should get the same
grade twice. This is a design expectation, not something pacrat can enforce,
and it is what makes a cached grading meaningful. Graders backed by a
language model will drift somewhat; keep the drift inside a grade band rather
than across one, and cache so the question is asked once.

**Cache on your side if the work is expensive.** pacrat caches too, but its
cache and yours answer different questions: pacrat's remembers what you said,
yours avoids the work when pacrat asks again after a `--refresh`, a digest
change, or on another host with the same tree. Key yours by commit, or by
content — anything that changes when the bytes change.

**Refuse rather than approximate.** Declining is a first-class outcome. Exit
nonzero with a reason on stderr whenever you cannot answer *the question that
was asked*: dependency missing, provider unreachable, tree unreadable,
analysis about a different commit. UNGRADED holds; a confident answer about
the wrong thing does not.

**Text.** Every string you return is printed on pacrat's own report, and it
has just passed through a file an attacker may have written. pacrat neuters
control characters and flattens each string onto one line before display, so
a newline in a title cannot forge a line of pacrat's output — but do not rely
on that being enough. Keep titles to one line and a sane length yourself.

**Say nothing on stdout but the report.** Worth repeating because it is the
single most common way an otherwise working adapter fails.

## What pacrat guarantees in return

- **The verdict is pacrat's, from your number.** Thresholds are host config
  (default: WARN at 2, BLOCK at 4, on 0-4). A grader cannot set them, and a
  grader that fails cannot lower them.
- **Your scale is honored, not assumed.** A grade on a foreign scale is
  mapped onto pacrat's 0-4 before it meets a threshold, rounding **up**,
  toward risk: a grading that lands between two pacrat grades takes the worse
  of the two. Your own number is preserved and displayed as you sent it.
- **Failure is never PROCEED.** A grader that fails, times out, exits
  nonzero, or answers about the wrong subject contributes nothing, and a
  subject with nothing is UNGRADED, which holds in automatic mode. Nothing is
  inferred from the shape of a failure.
- **Worst wins.** With several graders configured, the worst grade decides —
  but only gradings from a currently configured grader (or a human) can make
  a verdict *exist*. A leftover grading from a grader the host has since
  removed can still make a verdict worse and can never make one appear.
- **Caching you can predict.** An accepted grading is cached under (grader
  name, package, commit) **and the tree's content digest**. A commit says
  what upstream published; the digest says what is actually in the store. A
  read whose digest no longer matches is a miss, so an edited tree is
  re-graded rather than served an answer about bytes that are gone. Your JSON
  is stored verbatim, unknown fields and all.
- **Every call is visible.** The argv is printed, shell-quoted, before the
  grader runs — before, because a slow grader may sit for its whole timeout
  and a call nobody can see is a call nobody can interrupt. There is no
  hidden invocation and no silent retry.
- **Only failures pacrat can explain are recorded as failures**, with their
  reason, so a later run can say why you contributed nothing without running
  you again.

## Configuration

A host adds a grader to `~/.config/pacrat/config.toml`:

```toml
[[graders]]
name = "example-grader"
cmd = ["/usr/bin/example-grader", "--package", "{package}",
       "--tree", "{tree}", "--commit", "{commit}"]
timeout_s = 300
scale = { min = 0, max = 4 }
```

- **`name`** — how the grader is named in output, and a filename in the grade
  cache: `[A-Za-z0-9_.+-]`, no leading `.` or `-`, not `manual` (reserved for
  a human's own reading), not `failed` or `*.failed` (those name the cache's
  failure records). Unique across graders, because it is the cache key.
- **`cmd`** — the argv template.
- **`timeout_s`** — default 300. Raise it for anything that calls a model.
  `0` is a config error, not "no limit": a grader killed before it ran would
  turn every package into a hold.
- **`scale`** — optional **pin**. When set, a report declaring any other
  scale is UNGRADED with a reason rather than rescaled. Worth setting for
  anything whose output format could change under you: a grader that silently
  moved from 0-4 to 0-100 would otherwise keep producing plausible verdicts,
  with every grade quietly four times less alarming.

Graders run in the order configured. A host with none configured has no
external graders at all, which is a supported state — `pacrat grade
<package> --grade N --note "…"` records a human's own reading, and a human
always answers.

## Example producers

- `contrib/graders/yay-friend-grade` — an adapter that translates
  [yay-friend](https://github.com/aaronsb/yay-friend)'s analysis cache into
  this contract. It lives in `contrib/` rather than in pacrat because it is
  yay-friend's side of the boundary: pacrat's own code contains nothing
  specific to it, and the day yay-friend emits `pacrat-grade/v1` itself, the
  adapter is deleted and nothing else changes.
