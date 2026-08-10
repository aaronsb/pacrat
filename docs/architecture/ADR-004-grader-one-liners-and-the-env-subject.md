# ADR-004: grader one-liners and the env subject

**Status:** Accepted (2026-08-10 — decided by Aaron during the first real
host setup; implementation follows this document)
**Date:** 2026-08-10
**Extends:** ADR-001 (the grading contract), ADR-003 (the setup interview)

## Context

The setup interview's first real run asked for the adapter's path — the
binary was installed to `~/.local/bin`, so walking up from `current_exe()`
found no `contrib/`. And registering a grader today requires the argv-array
form:

```toml
cmd = ["/path/to/yay-friend-grade",
       "--package", "{package}", "--tree", "{tree}", "--commit", "{commit}"]
```

which is exact but hostile to the common case Aaron named: someone with a
totally different analyzer wants to write *one line* — for all we know it
goes through `jq`, `sed`, and `awk` — that produces the prescribed shape.
The array form cannot express a pipeline at all (there is deliberately no
shell on that path), and the placeholder flags are ceremony when the tool
could just be told the subject some simpler way.

## Decision

### The subject moves to the environment

For every grader run, pacrat exports the subject as environment variables:

- `PACRAT_PACKAGE` — the package name
- `PACRAT_TREE` — the staged tree's absolute path
- `PACRAT_COMMIT` — the commit id the grading will be filed under

Both invocation forms get them. The contract document gains the convention
as an addition — reports are unchanged, `pacrat-grade/v1` stays v1, and
the flag placeholders keep working — so existing graders and adapters run
unmodified.

### `cmd` accepts a one-line string

```toml
[[graders]]
name = "my-grader"
cmd = "mytool --tree \"$PACRAT_TREE\" | jq '{contract: \"pacrat-grade/v1\", …}'"
```

A string `cmd` runs as `sh -c <string>`, verbatim. pacrat substitutes
**nothing** into it — no placeholders, ever — which is what preserves the
argv form's security argument: the string is authored wholly by the config's
owner and edited by nobody, so there is still no injection surface. The
subject arrives through the environment; the shell the user asked for does
the user's quoting. A string containing `{package}`-style placeholders is a
config error naming the env convention, so nobody ships the old ceremony
into the new form and silently greps a literal brace.

The argv-array form remains, unchanged, for graders that want exec-exact
argument boundaries. Everything downstream is identical for both: same
timeout, same cache, same visibility rule — a string cmd is printed as-is
before it runs, because it *is* the invocation.

### The adapter becomes a simple string

`contrib/graders/yay-friend-grade` learns to read the `PACRAT_*` variables
when its flags are absent (flags win when both are present, so nothing
breaks). Registration collapses to:

```toml
cmd = "/path/to/contrib/graders/yay-friend-grade"
```

and the day yay-friend itself emits the contract, the string becomes
`"yay-friend grade"` and the adapter is deleted, exactly as its header
promised.

### The interview finds contrib itself

Adapter path resolution gains the rung the first run was missing:

1. ancestors of `current_exe()` — the development checkout;
2. `<store>/pacrat/contrib/graders/` — the installed case; the store is on
   every machine at a place `Ctx` already knows, and pacrat nests in it;
3. ask, as today, only when both miss.

An interview meeting an existing argv-form yay-friend entry offers to
modernize it to the string form (and only that — other graders are never
touched, per ADR-003).

## Consequences

- One-line graders open the contract to any tool that can print JSON,
  which was the contract's point; the spec document stays the source of
  truth and now shows both forms.
- The env variables are part of the contract surface from here on —
  renaming them is a breaking change and gets the same care as the report
  shape.
- `sh -c` enters the grader path deliberately and only for the string
  form; the module docs state the trade plainly rather than hiding it.
- The adapter's flag parsing survives until yay-friend goes native, then
  both go together.
