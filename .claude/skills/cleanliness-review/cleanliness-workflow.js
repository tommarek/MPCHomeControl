// Code-cleanliness sweep: area-scoped readers flag cruft (history/process comments, redundant /
// excessive comments, duplicated code, best-practice nits); each finding is adversarially verified by
// a conservative second agent so genuinely-useful comments are kept.
//
// Optional `args`:
//   areas : [{label, area}] — review buckets (default: a generic whole-tree split). `area` is a
//                             plain-language scope the agent reads ("src/foo/*, the X module").
export const meta = {
  name: 'cleanliness-review',
  description: 'Cleanliness sweep: history/process comments, redundant/excessive comments, duplication, best practices',
  phases: [{ title: 'Scan' }, { title: 'Verify' }],
}

const FINDINGS = {
  type: 'object',
  properties: {
    findings: {
      type: 'array',
      items: {
        type: 'object',
        properties: {
          file: { type: 'string' },
          line: { type: 'integer' },
          category: {
            type: 'string',
            enum: ['history_comment', 'redundant_comment', 'excessive_comment', 'duplicated_code', 'best_practice'],
          },
          excerpt: { type: 'string', description: 'the exact offending comment/code (short)' },
          why: { type: 'string', description: 'why it is cruft / a cleanliness issue' },
          fix: { type: 'string', description: 'rewrite to / delete / extract — the concrete change' },
        },
        required: ['file', 'category', 'excerpt', 'why', 'fix'],
      },
    },
  },
  required: ['findings'],
}

const VERDICT = {
  type: 'object',
  properties: {
    real: { type: 'boolean', description: 'true ONLY if genuinely cruft worth changing (not a useful comment)' },
    reason: { type: 'string' },
  },
  required: ['real', 'reason'],
}

const RUBRIC = `Clean up a mature, well-commented codebase. The author wants comments and code to read cleanly as the
code *is now* — not narrate how it got here.

FLAG these (the goal):
- **history_comment**: narrates a *past change*, a *rejected alternative*, or the review process. Tells:
  "previously / used to / no longer / now does X instead", "NOTE: do not also …", "(can't happen)" /
  "unreachable defensive" framing, "matching the … arm", "a regression for…", or anything only meaningful
  if you know what changed. Rewrite to state current behaviour, or delete if the code already says it.
- **redundant_comment**: restates what the code / type / name already says ("// increment i" above i += 1).
- **excessive_comment**: three lines where one would do; over-explaining the obvious.
- **duplicated_code**: the same non-trivial logic copy-pasted where it could be one function / const / shared
  module.
- **best_practice**: dead code, misleading names, non-idiomatic constructs, leftover scaffolding, tests that
  assert tautologies.

Do NOT flag (these are GOOD — leave them):
- A comment explaining a non-obvious **why**: domain rationale, an invariant, units, a subtle correctness
  reason, a gotcha, a safety note — anything not evident from the code itself.
- Module / function doc comments that state purpose, contract, or shape.
- Necessary duplication across **separate crates/modules** that genuinely cannot share code without a new
  shared unit — note it (best_practice) but don't demand a big refactor for a one-liner.

Report concrete items at file:line with the exact excerpt. Quality over quantity — only real cruft. Empty
list if an area is already clean.`

const cfg = args || {}
const reviewers = cfg.areas || [
  { label: 'clean:1', area: 'the core logic / algorithm modules' },
  { label: 'clean:2', area: 'data access, IO, serialization, and integration code' },
  { label: 'clean:3', area: 'the app / API / service entrypoints and wiring' },
  { label: 'clean:4', area: 'shared libraries / utilities — look hard for duplicated logic between modules' },
  { label: 'clean:5', area: 'tests, fixtures, and any UI / docs / config prose (stale or redundant text)' },
]

const reviewed = await pipeline(
  reviewers,
  (r) =>
    agent(`${RUBRIC}\n\nReview ONLY for cleanliness: ${r.area}\nRead the actual files.`, {
      label: r.label,
      phase: 'Scan',
      schema: FINDINGS,
      agentType: 'Explore',
    }),
  (res, r) =>
    parallel(
      (res?.findings || []).map((f) => () =>
        agent(
          `Decide if this code-cleanliness finding is genuinely worth changing. Read the actual code at the
location. Be conservative: a comment that conveys a non-obvious *why*, an invariant, units, or a real gotcha
must be KEPT (real=false). Mark real=true ONLY for genuine cruft: narrating a past change / rejected
alternative / review process, restating the obvious, needless verbosity, true duplicated logic, or a clear
best-practice violation. If unsure, real=false.

Finding (${r.label}):
- ${f.file}${f.line ? ':' + f.line : ''}  [${f.category}]
- excerpt: ${f.excerpt}
- why: ${f.why}
- proposed: ${f.fix}`,
          { label: `verify:${(f.excerpt || '').slice(0, 32)}`, phase: 'Verify', schema: VERDICT, agentType: 'Explore' }
        ).then((v) => ({ ...f, reviewer: r.label, verdict: v }))
      )
    )
)

const all = reviewed.flat().filter(Boolean)
const confirmed = all.filter((f) => f.verdict?.real)
log(`cleanliness scan done — ${all.length} raw, ${confirmed.length} confirmed`)
return {
  confirmed_count: confirmed.length,
  raw_count: all.length,
  by_category: confirmed.reduce((m, f) => ({ ...m, [f.category]: (m[f.category] || 0) + 1 }), {}),
  confirmed: confirmed.map((f) => ({
    file: f.file, line: f.line, category: f.category, excerpt: f.excerpt,
    why: f.why, fix: f.fix, reviewer: f.reviewer, verify_reason: f.verdict?.reason,
  })),
}
