export const meta = {
  name: 'grind-coord',
  description: 'Coordinator template behind clud /grind: one worker at a time in the central checkout turns tasks into pushed PRs, Sonnet chaperones watch each PR and report problems, and the coordinator schedules every fixup back through the central checkout before new work.',
  whenToUse: 'Started by the /grind router after triage and the question round, from a script it tailored to the run and passed through clud tool run grind/lint_workflow.py and an LLM review. Do not start it directly.',
  phases: [
    { title: 'Produce', detail: 'one worker at a time in the central checkout: task -> green local gate -> pushed PR' },
    { title: 'Chaperone', detail: 'a Sonnet watcher per PR reports merged, waiting or a problem to the coordinator' },
    { title: 'Finish', detail: 'final checkout reset and the long-horizon feature PR, when there is one' },
  ],
}
// managed-by: clud
//
// The coordinator contract (docs/architecture/grind.md#coordinator-workflow,
// talos-hq/clud#2873). This file is the TEMPLATE: the router copies it,
// tailors TASKS/POLICY and prompt text to the run, and must pass the copy
// through `clud tool run grind/lint_workflow.py` (static rules plus a
// simulated run against mock agents) and an LLM review before launch.
// A tailored copy keeps these invariants, which the linter checks:
//   1. Central checkout: only one agent works in `args.repo` at a time, and
//      no agent uses a worktree. Builds share one warm cache.
//   2. Fixups first: a reported problem is fixed in the central checkout
//      before any new task starts (backpressure).
//   3. In-flight cap: no new task starts while `policy.maxInFlight` PRs are
//      pushed and unsettled; the coordinator waits for a chaperone event.
//   4. Every pushed head gets a chaperone; chaperones only watch and report.
//   5. Bounded: watches, fixups and agent counts per PR are capped, and a
//      problem already fixed on the same head is never fixed twice.
//   6. Clean between tasks: every central agent ends on the base branch,
//      clean and synced with origin; otherwise a cleanup agent runs next.

const A = args || {}
const REPO = A.repo
const GH = A.github_repo
if (!REPO || !GH) throw new Error('grind-coord needs args.repo (the central checkout) and args.github_repo (OWNER/REPO)')
const MAIN = A.main || 'main'
const FEATURE = A.feature && A.feature.branch ? A.feature : null
// Long-horizon work lands on a feature branch: PRs target it, the checkout
// returns to it between tasks, and one final PR carries it to MAIN.
const BASE = FEATURE ? FEATURE.branch : MAIN
const LAND = A.land || 'auto'
if (!['auto', 'merge_queue', 'merge'].includes(LAND)) throw new Error(`args.land must be auto, merge_queue or merge, got ${LAND}`)
const P = A.policy || {}
const int = (v, d, lo, hi) => Math.min(hi, Math.max(lo, Math.floor(v ?? d)))
const POLICY = {
  maxInFlight: int(P.maxInFlight, 6, 1, 32),
  maxFixups: int(P.maxFixups, 3, 0, 8),
  maxWatches: int(P.maxWatches, 12, 1, 40),
  watchSeconds: int(P.watchSeconds, 540, 60, 540),
  maxCleanups: int(P.maxCleanups, 2, 0, 5),
  chaperoneModel: P.chaperoneModel || 'sonnet',
  workerModel: P.workerModel || null,
}
if (!['haiku', 'sonnet'].includes(POLICY.chaperoneModel)) throw new Error('policy.chaperoneModel must be haiku or sonnet: chaperones only watch')
const GATE = A.gate || {}
const GATE_TEXT = GATE.push
  ? `LOCAL GATE: push only with \`${GATE.push}\`; it runs the repository's checks and records the attestation on the pushed head. Never push with plain git push. Before it, run: ${(GATE.commands || []).map(c => `\`${c}\``).join(', ') || 'nothing else'}.`
  : `LOCAL GATE, all green before any push: ${(GATE.commands || []).map(c => `\`${c}\``).join(', ') || 'the planner-verified focused test'}. Never push red.`

const TASKS = (A.tasks || []).map(t => ({ ...t, id: String(t.id), after: (t.after || []).map(String) }))
const IDS = new Set(TASKS.map(t => t.id))
for (const t of TASKS) for (const d of t.after) if (!IDS.has(d)) throw new Error(`task ${t.id} waits on unknown task ${d}`)

const HEADER = `Central checkout: ${REPO}. GitHub repository: ${GH} (pass --repo ${GH}). Base branch: ${BASE}${FEATURE ? ` (a long-horizon feature branch; PRs target it, not ${MAIN})` : ''}.`
const CLEAN_END = `Before you return, leave the central checkout ready for the next task: \`git -C ${REPO} switch --discard-changes ${BASE}\`, \`git -C ${REPO} fetch origin ${BASE}\`, \`git -C ${REPO} merge --ff-only origin/${BASE}\`, then \`clud tool run git/state.py\`. Report checkout_clean=true only when the tree is clean and HEAD equals origin/${BASE}; ignored build caches stay.`
const NO_WAIT = 'Never wait for CI or the merge queue, never enqueue, never merge: a chaperone does that and reports back to the coordinator.'

const WORKER = { type: 'object', required: ['status', 'summary', 'checkout_clean'], properties: {
  status: { enum: ['pushed', 'covered', 'no_change', 'failed'] },
  pr_url: { type: 'string' }, branch: { type: 'string' }, head_sha: { type: 'string' },
  covered_by: { type: 'string' }, local_gate: { type: 'string' }, failure_log: { type: 'string' },
  checkout_clean: { type: 'boolean' }, summary: { type: 'string' },
  problems: { type: 'array', items: { type: 'object' } },
} }
const WATCH = { type: 'object', required: ['verdict', 'summary'], properties: {
  verdict: { enum: ['merged', 'queued', 'waiting', 'needs_fix', 'stuck', 'base_red'] },
  problem: { enum: ['red_check', 'review', 'conflict', 'ejected', 'parent_merged'] },
  head_sha: { type: 'string' }, evidence_id: { type: 'string' }, enqueued: { type: 'boolean' },
  failure_log: { type: 'string' }, summary: { type: 'string' },
} }
const CLEAN = { type: 'object', required: ['checkout_clean', 'summary'], properties: {
  checkout_clean: { type: 'boolean' }, summary: { type: 'string' },
} }

// ---- state -----------------------------------------------------------------
const status = new Map(TASKS.map(t => [t.id, t.gate === 'human' ? 'waiting_on_person' : 'queued']))
const pr = new Map()          // task -> { url, branch, head, fixups, watches, fixed: Set }
const fixQueue = []           // { task, problem, evidence, head, log }
const events = []             // chaperone reports waiting for the coordinator
let wake = null
const notify = () => { if (wake) { const w = wake; wake = null; w() } }
const nextEvent = () => events.length ? Promise.resolve() : new Promise(r => { wake = r })
let inFlight = 0
let dirty = false
let cleanups = 0
const report = []
const problems = []

const workerOpts = (label) => ({ label, phase: 'Produce', agentType: 'grind-worker', schema: WORKER, ...(POLICY.workerModel ? { model: POLICY.workerModel } : {}) })

// Human gates block their dependents too.
let changed = true
while (changed) {
  changed = false
  for (const t of TASKS) if (status.get(t.id) === 'queued' && t.after.some(d => status.get(d) === 'waiting_on_person')) { status.set(t.id, 'waiting_on_person'); changed = true }
}

// Longest remaining chain first, so the critical path enters the queue early.
const depth = new Map()
const chain = id => {
  if (depth.has(id)) return depth.get(id)
  depth.set(id, 0)
  const d = 1 + Math.max(0, ...TASKS.filter(t => t.after.includes(id)).map(t => chain(t.id)))
  depth.set(id, d)
  return d
}
TASKS.forEach(t => chain(t.id))
const ready = () => TASKS
  .filter(t => status.get(t.id) === 'queued' && t.after.every(d => status.get(d) === 'merged'))
  .sort((a, b) => depth.get(b.id) - depth.get(a.id))
const blockedForever = () => TASKS.filter(t => status.get(t.id) === 'queued' &&
  t.after.some(d => ['failed', 'stuck', 'base_red', 'waiting_on_person', 'blocked', 'left_in_queue'].includes(status.get(d))))

// ---- chaperone: watch only, report to the coordinator ----------------------
function watch(taskId) {
  const p = pr.get(taskId)
  p.watches += 1
  const n = p.watches
  const land = LAND === 'merge'
    ? `This repository has no merge queue: once the checks pass, merge with \`clud tool run github/pr_merge.py -- ${p.url} --repo ${GH} --match-head-commit ${p.head}\` and confirm MERGED.`
    : `Once the checks pass, enqueue with \`clud tool run github/pr_merge.py -- ${p.url} --repo ${GH} --match-head-commit ${p.head}\` (it picks the merge queue when ${BASE} requires one, or merges directly), then wait with \`clud tool run github/pr_merge_watch.py -- ${p.url} --repo ${GH} --until merged --timeout ${POLICY.watchSeconds}\`.`
  const prompt = `${HEADER}\nWATCH PR ${p.url} (task ${taskId}), expected head ${p.head}. Watch ${n} of at most ${POLICY.maxWatches}.\n` +
    `Wait for required checks with \`clud tool run github/pr_merge_watch.py -- ${p.url} --repo ${GH} --no-merge-wait --timeout ${POLICY.watchSeconds}\` in the foreground (one Bash call, timeout 600000, never run_in_background: you get no completion notification). ${land}\n` +
    `You only watch and report; never edit, build, rebase or push. Return verdict merged, queued (enqueued, still in the queue), waiting (checks still pending at your deadline; never needs_fix for a check that has not failed), base_red (the same job fails on ${BASE}), stuck, or needs_fix with problem red_check|review|conflict|ejected|parent_merged, the head_sha you judged, an evidence_id (the failing run id, review id or ejection reason; never a log file path) and failure_log (the failing job's last lines or the review comments). A rerun of an infrastructure failure is yours to do once; report it as waiting.`
  return agent(prompt, { label: `chaperone:${taskId}#${n}`, phase: 'Chaperone', agentType: 'grind-chaperone', model: POLICY.chaperoneModel, schema: WATCH })
    .catch(() => null)
    .then(r => { events.push({ task: taskId, r: r || { verdict: 'stuck', summary: 'chaperone returned nothing' } }); notify() })
}

function settle(taskId, final) {
  status.set(taskId, final)
  inFlight -= 1
  report.push({ task: taskId, status: final, pr: pr.get(taskId) && pr.get(taskId).url })
}

// The coordinator's reaction to one chaperone report.
function handle({ task, r }) {
  const p = pr.get(task)
  if (r.verdict === 'merged') return settle(task, 'merged')
  if (r.verdict === 'base_red' || r.verdict === 'stuck') return settle(task, r.verdict)
  if (r.verdict === 'needs_fix') {
    // Dedup on problem kind and head, never on evidence_id alone: a real
    // Sonnet chaperone filled evidence_id with a timestamped log path, which
    // differs on every report. A report about an older head, or about a
    // problem already fixed on this head, is stale: watch again instead of
    // fixing twice (talos-hq/clud#2878).
    const key = `${r.problem}@${p.head}`
    if ((r.head_sha && r.head_sha !== p.head) || p.fixed.has(key)) {
      log(`${task}: stale ${key} on ${r.head_sha || '?'}; watching again`)
    } else if (p.fixups >= POLICY.maxFixups) {
      return settle(task, 'stuck')
    } else {
      fixQueue.push({ task, problem: r.problem, evidence: key, log: [r.evidence_id, r.failure_log || r.summary].filter(Boolean).join('\n') })
      return
    }
  }
  if (p.watches >= POLICY.maxWatches) return settle(task, r.verdict === 'queued' ? 'left_in_queue' : 'stuck')
  watch(task)
}

async function cleanup() {
  cleanups += 1
  const r = await agent(`${HEADER}\nCLEANUP the central checkout after a task left it dirty. Park any uncommitted work you find on a branch named grind/parked-<n> with a commit, never discard it, then: ${CLEAN_END}`,
    { label: `cleanup:${cleanups}`, phase: 'Produce', agentType: 'grind-worker', schema: CLEAN, model: 'sonnet' })
  dirty = !(r && r.checkout_clean)
  if (dirty && cleanups > POLICY.maxCleanups) throw new Error('the central checkout stays dirty after cleanup; stopping before more work lands on it')
}

async function runFixup(f) {
  const p = pr.get(f.task)
  p.fixups += 1
  phase('Produce')
  const r = await agent(`${HEADER}\nFIXUP PR ${p.url} (task ${f.task}, branch ${p.branch}, head ${p.head}), fix ${p.fixups} of at most ${POLICY.maxFixups}. Problem: ${f.problem}.\n` +
    `Chaperone evidence:\n${f.log || '(none)'}\n` +
    `In the central checkout: \`git -C ${REPO} fetch origin ${p.branch} ${BASE}\`, switch to ${p.branch} at origin/${p.branch}. ${f.problem === 'conflict' || f.problem === 'ejected' || f.problem === 'parent_merged' ? `Rebase onto origin/${BASE} first. ` : ''}` +
    `Read the failure before changing anything; never weaken or skip a test. ${GATE_TEXT} Push to ${p.branch} with --force-with-lease=${p.branch}:${p.head}. ${NO_WAIT} ${CLEAN_END}`,
    workerOpts(`fixup:${f.task}#${p.fixups}`))
  dirty = !(r && r.checkout_clean)
  p.fixed.add(f.evidence)
  if (r && r.status === 'pushed' && r.head_sha) { p.head = r.head_sha; watch(f.task) } else settle(f.task, 'stuck')
}

async function runTask(t) {
  status.set(t.id, 'working')
  phase('Produce')
  const r = await agent(`${HEADER}\nTASK ${t.id}: ${t.title || ''}\n${t.brief || ''}\n` +
    `You are the only agent on this task until its PR is pushed. Dedup first (an open or merged PR already closing it means status covered). Branch from origin/${BASE}, write the focused regression test first (RED), make it pass (GREEN), review your own diff. ${GATE_TEXT} Open the PR against ${BASE}${t.issue ? ` with \`Closes #${t.issue}\`` : ''}. ${NO_WAIT} ${CLEAN_END}`,
    workerOpts(`work:${t.id}`))
  dirty = !(r && r.checkout_clean)
  if (r && Array.isArray(r.problems)) problems.push(...r.problems)
  if (!r || r.status === 'failed') { status.set(t.id, 'failed'); report.push({ task: t.id, status: 'failed', log: r && r.failure_log }); return }
  if (r.status === 'covered' || r.status === 'no_change') { status.set(t.id, 'merged'); report.push({ task: t.id, status: r.status, pr: r.covered_by }); return }
  pr.set(t.id, { url: r.pr_url, branch: r.branch, head: r.head_sha, fixups: 0, watches: 0, fixed: new Set() })
  status.set(t.id, 'in_flight')
  inFlight += 1
  watch(t.id)
}

// ---- the coordinator loop --------------------------------------------------
// One central agent at a time. Priority: cleanup, then chaperone reports,
// then fixups, then new tasks while under the in-flight cap; otherwise wait
// for the next chaperone report.
log(`grind-coord: ${TASKS.length} tasks on ${BASE}; in-flight cap ${POLICY.maxInFlight}, chaperones on ${POLICY.chaperoneModel}`)
for (;;) {
  if (dirty) { await cleanup(); continue }
  while (events.length) handle(events.shift())
  if (fixQueue.length) { await runFixup(fixQueue.shift()); continue }
  for (const t of blockedForever()) { status.set(t.id, 'blocked'); report.push({ task: t.id, status: 'blocked' }) }
  const next = ready()[0]
  if (next && inFlight < POLICY.maxInFlight) { await runTask(next); continue }
  if (inFlight > 0) { await nextEvent(); continue }
  break
}

for (const t of TASKS) if (status.get(t.id) === 'queued') { status.set(t.id, 'blocked'); report.push({ task: t.id, status: 'blocked' }) }
phase('Finish')
let featurePr = null
if (FEATURE && TASKS.some(t => status.get(t.id) === 'merged')) {
  featurePr = await agent(`${HEADER}\nFEATURE: every landed task is on ${BASE}. Open or update the PR from ${BASE} into ${MAIN} (draft unless args say otherwise), listing each task's PR. Merge origin/${MAIN} into ${BASE} first if it is behind and the merge is clean; push that merge with \`${GATE.push || 'git push'}\` after the local gate passes. Never merge the feature PR yourself. ${CLEAN_END}`,
    workerOpts('feature-pr'))
}
const waiting = TASKS.filter(t => status.get(t.id) === 'waiting_on_person').map(t => ({ task: t.id, status: 'waiting_on_person', reason: t.gate_reason || '' }))
return { base: BASE, tasks: [...report, ...waiting], feature_pr: featurePr, problems }
