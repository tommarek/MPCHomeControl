---
name: cleanliness-review
description: Sweep the code for cleanliness with a panel of subagents and fix it — comments that narrate history/process or restate the code, redundant/excessive comments, duplicated code, and best-practice nits — then re-verify iteratively until clean. Protects genuinely useful why/units/invariant comments. Use for "clean up the comments", "remove duplicated code", "no useless/excessive comments", "tidy this up", or a pre-merge polish pass.
---

# Cleanliness review

Make the code and comments read cleanly as the code *is now* — not narrate how it got there — and
remove duplication, with a panel of subagents verifying each finding so genuinely-useful comments
survive.

## What counts as cruft (fix it)

- **history / process narration** — comments that describe a *past change*, a *rejected alternative*,
  or the review/PR process ("previously / used to / no longer / now does X instead", "NOTE: do not
  also…", "(can't happen)" defensiveness, "matching the … arm", "a regression for…"). Rewrite to
  plainly state current behaviour, or delete if the code already says it.
- **redundant** — restates what the code / type / name already says.
- **excessive** — three lines where one would do; over-explaining the obvious.
- **duplicated code** — the same non-trivial logic copy-pasted where it could be one function / const /
  shared module.
- **best-practice** — dead code, misleading names, non-idiomatic constructs, leftover scaffolding,
  incomplete tests that assert tautologies.

## What to protect (do NOT strip)

A comment that explains a non-obvious **why**: domain/physics rationale, an invariant, units, a subtle
correctness reason, a gotcha, a safety note — anything not evident from the code itself. Module /
function doc comments that state purpose, contract, or shape. When in doubt, keep it.

## Run a round

1. **Launch the workflow.** Call `Workflow` with `cleanliness-workflow.js` in this skill directory. It
   fans area-scoped readers over the code, each finding adversarially verified (a conservative second
   agent that keeps useful comments), and returns the confirmed set grouped by category. Pass
   `args.areas` to scope it (defaults to whole-tree buckets).
2. **Apply judgment, don't mass-delete.** Re-read each item: trim/rewrite cruft, **keep** load-bearing
   comments even if flagged (the verifier is sometimes over-aggressive). For duplication, extract to a
   shared function / module — including a new workspace crate when two binaries genuinely can't share
   otherwise — but don't force a big refactor for a one-liner.
3. **Fix regardless of origin.** Clean up pre-existing cruft too; the goal is clean code, not blame.
4. **Keep it green, commit clean.** Run the project's gates (format, lint-as-errors, full tests, any
   structural gate). After a dedup, also drop now-unused deps / imports. Commit per the repo's
   conventions, listing what was trimmed and what was deduped.

## Repeat until clean

Re-run with fresh agents. Early rounds often *grow* (a deeper read finds more); stop when a round
returns only over-aggressive suggestions you'd reject — i.e. nothing genuinely worth changing. Proactively
fix whole classes (e.g. the same duplicated pattern everywhere it appears) so the next round can't
re-find them one at a time.
