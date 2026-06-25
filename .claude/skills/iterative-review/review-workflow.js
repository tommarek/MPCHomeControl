// Iterative multi-agent review: a panel of finders (several blind) over the current changes, each
// finding adversarially verified. Returns the confirmed set for the caller to re-verify and fix.
//
// Optional `args`:
//   narrative : string  — what the branch does + its hard invariants (fed to the informed reviewers)
//   blind     : [string]— area scopes for the blind reviewers (default: core / data-IO / app-API)
//   informed  : [string]— focus areas for the informed reviewers
export const meta = {
  name: 'iterative-review',
  description: 'Panel review (several blind) of the current changes; adversarial-verify every finding',
  phases: [{ title: 'Review' }, { title: 'Verify' }],
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
          severity: { type: 'string', enum: ['blocker', 'major', 'minor'] },
          title: { type: 'string' },
          why: { type: 'string', description: 'concrete reason this is a real problem' },
          fix: { type: 'string', description: 'the concrete fix' },
        },
        required: ['file', 'severity', 'title', 'why'],
      },
    },
  },
  required: ['findings'],
}

const VERDICT = {
  type: 'object',
  properties: {
    real: { type: 'boolean', description: 'true ONLY if confirmed a real problem by reading the code' },
    severity: { type: 'string', enum: ['blocker', 'major', 'minor'] },
    reason: { type: 'string' },
  },
  required: ['real', 'reason'],
}

const cfg = args || {}
const narrative =
  cfg.narrative ||
  'Review the current branch — its changes against the main branch. Infer intent from the code and any repo conventions (e.g. CLAUDE.md).'

const blindPrompt = (area) => `You are reviewing an unfamiliar codebase. Do NOT assume anything about what was
recently changed or why; read the code and find REAL problems. Focus on: ${area}

Look for genuine bugs and risks: incorrect logic / math, unit or sign errors, panics / unwrap / expect on
fallible or external data, async/concurrency hazards, resource leaks, error handling that hides failures,
off-by-one, silently-wrong fallbacks, injection / unvalidated input, and anything that would misbehave in
production. Read the actual code (don't guess). Report only concrete issues you can point to at file:line
with a specific reason and a concrete fix. Skip style nits and anything you cannot substantiate. If you
genuinely find nothing real in your area, return an empty findings list.`

const informedPrompt = (focus) => `${narrative}

Review focus for you: ${focus}

Scrutinise hard for REAL correctness / safety problems — read the actual code at each spot. Report concrete
issues only (file:line + why + the fix). Skip style; don't invent problems. Empty list if your area is clean.`

const blind = cfg.blind || [
  'the core logic / algorithms / domain computations',
  'data access, IO, serialization, concurrency, and external integrations',
  'the app / API / UI surface and cross-cutting concerns',
]
const informed = cfg.informed || [
  'correctness and edge cases of the changed logic; could any input break it?',
  'error handling, observability, resource cleanup, and failure modes',
  'security, input validation, secret handling, and configuration safety',
]

const reviewers = [
  ...blind.map((a, i) => ({ label: `blind:${i + 1}`, prompt: blindPrompt(a) })),
  ...informed.map((f, i) => ({ label: `informed:${i + 1}`, prompt: informedPrompt(f) })),
]

const reviewed = await pipeline(
  reviewers,
  (r) => agent(r.prompt, { label: r.label, phase: 'Review', schema: FINDINGS, agentType: 'Explore' }),
  (rev, r) =>
    parallel(
      (rev?.findings || []).map((f) => () =>
        agent(
          `Adversarially verify this code-review finding by READING THE ACTUAL CODE. Be skeptical — many findings
are false positives (a misreading, intended/defended behaviour, or a "fix" that would itself introduce a bug).
Mark real=true ONLY if you confirm a genuine problem at the cited location; otherwise real=false.

Finding (from ${r.label}):
- file: ${f.file}${f.line ? ':' + f.line : ''}
- severity: ${f.severity}
- title: ${f.title}
- why: ${f.why}
${f.fix ? '- proposed fix: ' + f.fix : ''}`,
          { label: `verify:${(f.title || '').slice(0, 40)}`, phase: 'Verify', schema: VERDICT, agentType: 'Explore' }
        ).then((v) => ({ ...f, reviewer: r.label, verdict: v }))
      )
    )
)

const all = reviewed.flat().filter(Boolean)
const confirmed = all.filter((f) => f.verdict?.real)
log(`reviewers done — ${all.length} raw findings, ${confirmed.length} confirmed after adversarial verify`)
return {
  confirmed_count: confirmed.length,
  raw_count: all.length,
  confirmed: confirmed.map((f) => ({
    file: f.file, line: f.line, severity: f.verdict?.severity || f.severity,
    title: f.title, why: f.why, fix: f.fix, reviewer: f.reviewer, verify_reason: f.verdict?.reason,
  })),
}
