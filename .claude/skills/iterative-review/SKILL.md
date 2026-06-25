---
name: iterative-review
description: Iteratively review the current branch/changes with a panel of subagents — several of them blind (no prior knowledge of what changed) to resist bias — adversarially verify every finding, fix all genuine issues regardless of who wrote them, and repeat until a pass is clean. Use for "review this branch", "iterative review", "review until no issues", pre-PR hardening, or when the user asks for a thorough multi-agent code review.
---

# Iterative multi-agent review

Run a panel review of the current changes until a pass surfaces nothing genuinely actionable. The
point of the panel (and of keeping several reviewers **blind**) is to catch real problems without
the bias of knowing what "should" be there.

## Run one round

1. **Scope it.** Figure out what's under review — usually `git diff <main>...HEAD` (or the working
   tree). Note the project's verification gates (build, lint, tests) and any hard invariants from
   the repo's conventions (e.g. `CLAUDE.md`) so you can both feed them to the informed reviewers and
   enforce them after fixing.
2. **Launch the workflow.** Call `Workflow` with `review-workflow.js` in this skill directory. It
   fans out 6 reviewers (3 blind area-scoped, 3 informed) and adversarially verifies every finding,
   returning the confirmed set. Pass context via `args`:
   - `args.narrative` — what the branch does + the hard invariants (for the informed reviewers).
   - `args.blind` — array of area strings to scope the blind reviewers (defaults to core / data-IO /
     app-API buckets).
   - `args.informed` — array of focus strings for the informed reviewers.
   Scale to the ask: a quick check needs fewer finders and a single verify vote; "thorough" / "audit"
   warrants a larger finder pool and a stronger verify pass.
3. **Re-verify each finding yourself.** The adversarial-verify stage still lets false positives
   through — read the actual code at each "confirmed" location before acting. Many plausible findings
   are misreadings, intended-and-defended behaviour, or would *introduce* a bug (e.g. a "missing
   scale factor" that's already applied elsewhere). When you reject one, say why.
4. **Fix every genuine issue — regardless of origin.** Fix pre-existing problems too; the goal is a
   clean tree, not assigning blame. Prefer the smallest change that's correct and matches the
   surrounding code. Reject over-defensive / speculative nits, recording the reasoning.
5. **Keep it green, commit clean.** Run the project's gates (format, lint-as-errors, full tests, and
   any structural gate) and make them pass. Commit with a message that lists what was fixed **and**
   what was reviewed-but-rejected and why. Follow the repo's commit conventions (e.g. no AI
   attribution if required).

## Repeat until clean

Re-run the workflow on the new state. Stop when a round yields only false positives or over-defensive
suggestions you'd reject — i.e. nothing genuinely actionable. Counts won't fall monotonically (each
round's blind reviewers sweep different areas and find different things); judge by whether the
remaining items are real. Watch for the same false positive recurring across rounds — add a short
clarifying code comment so it stops being re-flagged.

## Notes

- Reviewers are read-only `Explore` agents on the real tree.
- If a finding is genuine but its fix is a larger refactor, do it deliberately (and verify), or flag
  it for the user rather than papering over it.
- Proactively sweep for whole *classes* of a recurring finding (e.g. once one missing guard is found,
  fix all siblings) so the next round can't re-find them piecemeal.
