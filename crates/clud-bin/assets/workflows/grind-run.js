// managed-by: clud
export const meta = {
  name: 'grind-run',
  description: 'Engine behind clud /grind (start it with /grind): plan, write, review, integrate and land a list of goals, in parallel worktrees or sequentially in the local checkout, with per-role caps.',
  whenToUse: 'Started by the /grind router skill, which gathers the goals, mode, models and CI choice first. Do not start it directly.',
  phases: [
    { title: 'Classify', detail: 'grind-planner plan-only: classify children, feature groups, threshold' },
    { title: 'Prework', detail: 'grind-prework: post the plan JSON comment on the meta issue' },
    { title: 'Gate', detail: 'grind-gate: observe one approved publication check' },
    { title: 'Plan', detail: 'grind-planner: split each goal into disjoint file tasks' },
    { title: 'Work', detail: 'grind-worker: read/write files only' },
    { title: 'Review', detail: 'grind-reviewer: read/edit only' },
    { title: 'Integrate', detail: 'grind-integrator: one at a time; lint, build, test, push' },
    { title: 'Land', detail: 'grind-lander: PR checks green, then enqueue (the goal is done) or hand back for fixes' },
    { title: 'Chaperone', detail: 'grind-chaperone (haiku): bounded pr_merge_watch --until merged per queued PR; MERGED / NEEDS-FIXUP / STUCK' },
  ],
}

// args: {
//   repo: '/abs/repo', github_repo: 'owner/name', main: 'main', mode: 'parallel' | 'sequential',
//   // github_repo may be omitted when plan.repo supplies the same forge identity.
//   goals: [{ id, title, brief }], ci: false, lane?: 'job-id',
//   models?: { planner, worker, reviewer, integrator },   // omitted = session model
//   scripts?: { lint?: './lint', test?: './test' },     // omitted = planner picks verify
//   queueWatchSeconds?: 540, queueFixups?: 2,           // merge-queue chaperone watch deadline and automatic fixups (#2467)
//   maxAgents?: 4, maxFixRounds?: 10, agentErrors?: 3,   // agentErrors: tool-misuse allowance, apart from fix rounds (#1331)
//   gateAttempts?: 3, gateBackoffMs?: 1000, gateEpoch?: 0, gateEpochs?: {goalId: 0},
//   planOnly?: true, meta?: '<meta issue number>',
//   orchestration?: { session_repo: 'owner/repo', goals: [
//     { id, repo, main, checkout?: 'local' | 'worktree', blockers: [{goal, when, check?}] }
//   ], proposed_goals: [], risks: [] }, // run graph, separate from grind-plan/v1 stages
//   base?: 'main',   // default base branch for goals in no plan stage; omitted = main
//                    // a plan stage's `base` wins for its children (#1409)
//   // Stages (#1409): the router starts grind-run once per stage. The bug-stage
//   // call passes `plan` (prework posts it) and the bug children as `goals`
//   // (none when the plan is all feature: that call only posts the plan). The
//   // feature-stage call comes after the router cut the feature branch from the
//   // updated main, and passes `plan`, the feature children, `feature`,
//   // `feature_merge`, `problem_reporting` and:
//   plan_url?: '<url>',   // the plan comment the bug-stage call posted; prework is skipped
//   stuck_bugs?: ['103'], // bug children that did not merge in the bug-stage call;
//                         // feature goals listing one in depends_on_bugs are blocked
//   plan?: { schema: 'grind-plan/v1', run_id, meta, original, repo, main, mode,
//            preflight, structure, stages, deferred_groups, feature_merge,
//            problem_reporting, models, ci, scripts, rules, waiting_on_pr },
//            // deferred_groups: [{ group, sub_meta, children }] feature groups
//            // skipped this run; waiting_on_pr: an open feature PR number that,
//            // with rules.no_overlap 'bugs_only', limits the run to bug stages.
//            // At most one feature stage runs per run (#1412).
//            // the router's assembled plan; when present, grind-prework posts it
//            // on the meta issue before any worker runs (#1408)
//   feature?: { branch, worktree, pr },   // set by the router after feature setup (#1410):
//                    // the feature branch, its absolute worktree
//                    // (<repo>/.clud/grind/worktrees/feature) and its draft PR
//   feature_merge?: 'auto' | 'decide_later' | 'comment_only',
//                    // what happens to the feature PR once its goals settle;
//                    // args.plan.feature_merge is also read; default decide_later
//   problem_reporting?: 'issue' | 'comment',   // args.plan.problem_reporting is also read
// }
// problems (#1411): roles return them; the router files them per problem_reporting.
// With a plan the result is { goals, plan_url, problems, problem_reporting } (plus
// `feature` in a feature stage); without one it is the goal summary array.
// A plan-carrying call may have no goals: it only posts the plan.
const POSTS_PLAN = !!(args && args.plan && !args.plan_url && !args.planOnly)
if (!args || !args.repo || !Array.isArray(args.goals) || (!args.goals.length && !POSTS_PLAN)) {
  throw new Error('grind needs args {repo, mode, goals:[{id,title,brief}]}; start it with /grind')
}
if (!args.planOnly && args.mode !== 'parallel' && args.mode !== 'sequential') {
  throw new Error(`grind mode must be "parallel" or "sequential" (got ${JSON.stringify(args.mode)}); cron mode is driven by /grind-cron`)
}

// Validate the entire graph before classification, prework, or any goal agent.
// This is structural validation. A local checkout still requires a clean-tree
// check, and worktree creation still requires the session's explicit consent.
const nonempty = (s) => typeof s === 'string' && s.trim().length > 0
const graphGoal = (g, sessionRepo) => {
  if (!g || !nonempty(g.id)) throw new Error('run plan: every goal needs an id')
  if (!nonempty(g.repo) || !nonempty(g.main)) throw new Error(`goal ${g.id}: repo and main are required`)
  if (!/^[^\s/]+\/[^\s/]+$/.test(g.repo)) throw new Error(`goal ${g.id}: repo must be owner/name`)
  const checkout = g.checkout || (g.repo === sessionRepo ? 'local' : 'worktree')
  if (checkout !== 'local' && checkout !== 'worktree') throw new Error(`goal ${g.id}: unknown checkout ${JSON.stringify(checkout)}`)
  if (checkout === 'local' && g.repo !== sessionRepo) throw new Error(`goal ${g.id}: a sister repo must use a worktree`)
  if (!Array.isArray(g.blockers)) throw new Error(`goal ${g.id}: blockers must be an array`)
  return { ...g, checkout }
}
const graphBlocker = (blocker, goal, ids) => {
  if (!blocker || !ids.has(blocker.goal)) throw new Error(`unknown goal ${JSON.stringify(blocker && blocker.goal)} in blockers of ${goal.id}`)
  if (!['merged', 'released', 'tag', 'published'].includes(blocker.when)) throw new Error(`unknown condition ${JSON.stringify(blocker.when)}`)
  if (blocker.when !== 'merged' && !nonempty(blocker.check)) throw new Error(`blocker ${blocker.goal} → ${goal.id} (${blocker.when}) has no check`)
}
const graphOrder = (goals, ids) => {
  const done = new Set()
  const active = []
  const ordered = []
  const visit = (id) => {
    if (done.has(id)) return
    const cycle = active.indexOf(id)
    if (cycle >= 0) throw new Error(`dependency cycle: ${[...active.slice(cycle), id].join(' → ')}`)
    active.push(id)
    const goal = ids.get(id)
    goal.blockers.forEach(b => visit(b.goal))
    active.pop()
    done.add(id)
    ordered.push(goal)
  }
  goals.forEach(g => visit(g.id))
  return ordered
}
const validateRunPlan = (plan) => {
  if (!plan || !nonempty(plan.session_repo)) throw new Error('run plan: session_repo is required')
  if (!Array.isArray(plan.goals) || !plan.goals.length) throw new Error('run plan: goals must be a nonempty array')
  const goals = plan.goals.map(g => graphGoal(g, plan.session_repo))
  const ids = new Map()
  goals.forEach(g => {
    if (ids.has(g.id)) throw new Error(`duplicate goal ${JSON.stringify(g.id)}`)
    ids.set(g.id, g)
  })
  goals.forEach(g => g.blockers.forEach(b => graphBlocker(b, g, ids)))
  return { ...plan, goals: graphOrder(goals, ids) }
}
const ORCHESTRATION = args.orchestration === undefined ? null : validateRunPlan(args.orchestration)
const RUN_GOALS = new Map((ORCHESTRATION ? ORCHESTRATION.goals : []).map(g => [g.id, g]))
const validateApprovedContext = () => {
  if (ORCHESTRATION.session_root !== args.repo) throw new Error('run plan: execution root differs from approved session_root')
  for (const g of RUN_GOALS.values()) {
    if (g.repo === ORCHESTRATION.session_repo && g.main !== (args.main || 'main')) {
      throw new Error(`run plan: execution main differs from approved main for ${g.id}`)
    }
  }
  for (const stage of ((args.plan && args.plan.stages) || [])) {
    for (const [id, deps] of Object.entries(stage.depends_on_bugs || {})) {
      const goal = RUN_GOALS.get(String(id).replace(/^#/, ''))
      if (!goal || !Array.isArray(deps)) throw new Error('run plan: invalid classification dependencies')
      for (const dep of deps) {
        if (!goal.blockers.some(b => b.goal === String(dep).replace(/^#/, ''))) {
          throw new Error(`run plan: classification dependency ${dep} → ${id} is absent from the approved graph`)
        }
      }
    }
  }
}
if (ORCHESTRATION) {
  validateApprovedContext()
  const selected = new Map(args.goals.map(g => [String(g.id), g]))
  for (const id of selected.keys()) {
    if (!RUN_GOALS.has(id)) throw new Error(`run plan: selected goal ${id} was not approved`)
  }
  args.goals = ORCHESTRATION.goals.filter(g => selected.has(g.id)).map(g => selected.get(g.id))
}

const REPO = args.repo
const MAIN = args.main || 'main'
// The checkout path and the forge identity serve different purposes (#1897).
// Validate the explicit/recorded identity before any agent can act on a fork.
const githubRepository = (value) => {
  const invalid = () => new Error('GitHub repository must be an explicit OWNER/REPO in github_repo or plan.repo')
  if (typeof value !== 'string') throw invalid()
  const slug = value.trim()
  if (!/^[A-Za-z0-9][A-Za-z0-9_.-]*\/[A-Za-z0-9_.-]+$/.test(slug)
    || slug.split('/').some(part => part === '.' || part === '..')) throw invalid()
  return slug
}
const suppliedRepository = args.github_repo === undefined ? null : githubRepository(args.github_repo)
const recordedRepository = args.plan?.repo === undefined ? null : githubRepository(args.plan.repo)
if (suppliedRepository && recordedRepository
  && suppliedRepository.toLowerCase() !== recordedRepository.toLowerCase()) {
  throw new Error('GitHub repository disagrees with the recorded plan')
}
const GITHUB_REPO = suppliedRepository || recordedRepository
// A current /grind router always passes one, so a missing repository means a
// stale router skill copy, not a missing argument (#2437).
if (!GITHUB_REPO) {
  throw new Error('GitHub repository is missing: pass github_repo or plan.repo as OWNER/REPO. '
    + 'The current /grind router always passes it, so ~/.claude/skills/grind/SKILL.md is probably '
    + 'a stale copy that no longer matches the installed grind-run workflow: check that it still '
    + 'carries the `managed-by: clud` marker, and relaunch clud to refresh it before retrying.')
}
const REPO_HEADER = `Repo: ${REPO} (default branch ${MAIN}).\nGitHub repository: ${GITHUB_REPO}. Pass --repo ${GITHUB_REPO} to gh issue/pr/run/label/search/workflow commands and the PR waiter. Put this repository directly in gh api endpoints.`
const PARALLEL = args.mode === 'parallel'
const CI = !!args.ci
const MAX_AGENTS = Math.max(1, Math.floor(args.maxAgents ?? 4))
const MAX_FIX = Math.max(0, Math.floor(args.maxFixRounds ?? 10))
// A lander's own mistake (a bad flag, a missing tool) is not a CI or review
// failure: it never spends a fix round (#1331). It has its own small allowance
// per goal; past it the goal stops with `gave_up: tool misuse`.
const MAX_AGENT_ERRORS = Math.max(0, Math.floor(args.agentErrors ?? 3))
const MODELS = args.models || {}
const gateInteger = (name, fallback, minimum, maximum) => {
  const value = args[name] ?? fallback
  if (!Number.isInteger(value) || value < minimum || value > maximum) {
    throw new Error(`${name} must be an integer from ${minimum} to ${maximum}`)
  }
  return value
}
const GATE_ATTEMPTS = gateInteger('gateAttempts', 3, 1, 5)
const GATE_BACKOFF_MS = gateInteger('gateBackoffMs', 1000, 1, 30000)
// Each resume replays its history; cap that history before launching any agent.
const MAX_GATE_EPOCH = 64
const GATE_EPOCH = gateInteger('gateEpoch', 0, 0, MAX_GATE_EPOCH)
// Resume only deferred branches: completed gate prompts must retain their native
// cache keys. Epochs select fresh observations; they never assert readiness.
const GATE_EPOCHS = args.gateEpochs ?? {}
if (!GATE_EPOCHS || typeof GATE_EPOCHS !== 'object' || Array.isArray(GATE_EPOCHS)) {
  throw new Error('gateEpochs must map approved goal ids to integer epochs')
}
for (const [id, epoch] of Object.entries(GATE_EPOCHS)) {
  if (!RUN_GOALS.has(id) || !Number.isInteger(epoch) || epoch < 0 || epoch > MAX_GATE_EPOCH) {
    throw new Error(`invalid gate epoch for approved goal ${id}`)
  }
}
let replayEpoch = null
const requestedEpoch = (id) => GATE_EPOCHS[String(id)] ?? GATE_EPOCH
const gateEpoch = (id) => replayEpoch ?? requestedEpoch(id)

// The repo's ./lint and ./test, chosen once by the /grind router (#1336).
const SCRIPTS = (args.scripts && (args.scripts.lint || args.scripts.test)) ? args.scripts : null
const scriptLines = () => [SCRIPTS.lint, SCRIPTS.test].filter(Boolean)
const verifyBlock = (p) => SCRIPTS
  ? `Verify commands, in this order, before every push (fix rounds included):\n` +
    `1. planner's focused test:\n${p.verify}\n` +
    scriptLines().map((c, i) => `${i + 2}. ${c}`).join('\n') +
    `\nThe repo scripts (${scriptLines().join(', ')}) were chosen for this run; run them lint first, then test, and rerun after every fix until green.`
  : `Verify commands:\n${p.verify}`

// Model per role; the lander shares the integrator's choice, prework the planner's.
const opts = (role, label, phase, schema) => {
  const o = { label, phase, schema, agentType: `grind-${role}` }
  const m = MODELS[role === 'lander' ? 'integrator' : role === 'prework' ? 'planner' : role]
  if (m) o.model = m
  return o
}

// Counting semaphore for plan/work/review agents, and a mutex for the
// integrator: builds never overlap, so caches stay warm and the CPU stays sane.
const semaphore = (n) => {
  let free = n
  const queue = []
  // Native pipeline stages restore their branch context after a queued permit
  // resolves. A plain VM await can otherwise inherit the releasing branch's
  // cache chain, even when the queue carries permits rather than callbacks.
  return async (fn) => {
    let acquired = false
    let failure = null
    try {
      const results = await pipeline([true], async () => {
        if (free > 0) free--
        else await new Promise(resolve => queue.push(resolve))
        acquired = true
        return true
      }, async () => {
        try { return { value: await fn() } }
        catch (cause) { failure = { cause }; return {} }
      })
      if (failure) throw failure.cause
      if (!results[0]) throw new Error('native semaphore stage did not complete')
      return results[0].value
    } finally {
      if (acquired) {
        if (queue.length) queue.shift()()
        else free++
      }
    }
  }
}
const light = semaphore(MAX_AGENTS)
const exclusive = semaphore(1)

// Problems found outside a task (#1411): optional on every role's schema,
// recorded once per run however many rounds or roles report them.
const PROBLEMS = { type: 'array', description: 'problems found outside this task (bugs, flaky tests, doc gaps); the router files them, never you', items: { type: 'object', required: ['kind', 'summary'], properties: {
  kind: { type: 'string' }, summary: { type: 'string' }, evidence: { type: 'string' }, related_issue: { type: 'string' } } } }
const problemKey = (p) => [p.kind, p.summary, p.related_issue || ''].map(s => String(s || '').trim().toLowerCase()).join('|')
const PROBLEMS_SEEN = new Map()
const collect = (res, role, goalId) => {
  for (const p of (res && Array.isArray(res.problems) ? res.problems : [])) {
    if (!p || !String(p.summary || '').trim()) continue
    const k = problemKey(p)
    if (PROBLEMS_SEEN.has(k)) continue
    PROBLEMS_SEEN.set(k, { kind: p.kind, summary: p.summary, evidence: p.evidence, related_issue: p.related_issue, role, goal: goalId })
  }
  return res
}
const allProblems = () => [...PROBLEMS_SEEN.values()]
const problemsOf = (id) => allProblems().filter(p => String(p.goal) === String(id))
const logProblems = () => allProblems().forEach(p => log(`problem: [${p.kind}] ${p.summary} (${p.role} ${p.goal})`))

const PLAN = {
  type: 'object', required: ['checkout', 'branch', 'depends_on', 'tasks', 'verify'],
  properties: {
    checkout: { type: 'string', description: 'absolute path the goal is worked in (its worktree, or the repo in sequential mode)' },
    branch: { type: 'string' },
    depends_on: { type: 'array', items: { type: 'string' }, description: 'ids of other goals this one must land after; [] means isolated' },
    verify: { type: 'string', description: 'exact lint/build/test commands for the integrator, newline separated' },
    tasks: { type: 'array', items: { type: 'object', required: ['id', 'files', 'instructions'], properties: {
      id: { type: 'string' }, files: { type: 'array', items: { type: 'string' }, description: 'files this task writes; a check that writes nothing belongs in verify' }, instructions: { type: 'string' } } } },
    context: { type: 'object', description: 'the context pack (#1339): issue text and code excerpts every worker and the reviewer start from', properties: {
      issue: { type: 'string', description: "the goal issue's body and comments, verbatim" },
      excerpts: { type: 'array', items: { type: 'object', required: ['path', 'start', 'end', 'text'], properties: {
        path: { type: 'string' }, start: { type: 'integer' }, end: { type: 'integer' }, why: { type: 'string' },
        text: { type: 'string', description: 'the exact lines start..end' } } } } } },
    covered_by: { type: 'string', description: 'dedup preflight (#2467): an open or merged PR (#N) that already closes this goal; set it and leave tasks empty to skip the goal' },
    problems: PROBLEMS,
  },
}
const WORK = { type: 'object', required: ['files_touched', 'summary'], properties: {
  files_touched: { type: 'array', items: { type: 'string' } }, summary: { type: 'string' }, blocked: { type: 'string' }, problems: PROBLEMS } }
const REVIEW = { type: 'object', required: ['approved', 'summary'], properties: {
  approved: { type: 'boolean' }, summary: { type: 'string' }, fixes_applied: { type: 'array', items: { type: 'string' } },
  must_verify: { type: 'array', items: { type: 'string' }, description: 'checks the integrator must run before pushing (focused test, reproduction, script); an unrun check goes here, never into a rejection' },
  problems: PROBLEMS } }
// Reported artifacts are downstream context, never proof that a publication
// gate is ready. That still requires the approved check to succeed.
const ARTIFACTS = { type: 'object', properties: {
  version: { type: 'string' }, tag: { type: 'string' }, release_url: { type: 'string' },
} }
const GATE = { type: 'object', required: ['ready', 'evidence'], properties: {
  ready: { type: 'boolean' }, evidence: { type: 'string' },
} }
const INTEG = { type: 'object', required: ['pushed', 'summary'], properties: {
  pushed: { type: 'boolean' }, pr_url: { type: 'string' }, summary: { type: 'string' }, failure_log: { type: 'string' },
  artifacts: ARTIFACTS, problems: PROBLEMS } }
const LAND = { type: 'object', required: ['status', 'summary'], properties: {
  status: { type: 'string', enum: ['merged', 'enqueued', 'needs_fix', 'base_red', 'gave_up', 'tool_misuse'] }, summary: { type: 'string' }, failure_log: { type: 'string' },
  head_sha: { type: 'string', description: 'enqueued only: the full head SHA passed to --match-head-commit' },
  command: { type: 'string', description: 'tool_misuse only: the command that failed' }, artifacts: ARTIFACTS, problems: PROBLEMS } }
// The queue chaperone's verdict (#2467, DD-192): MERGED, still queued,
// NEEDS-FIXUP (ejected or conflicting), STUCK, or a red base.
const CHAPERONE = { type: 'object', required: ['verdict', 'summary'], properties: {
  verdict: { type: 'string', enum: ['merged', 'queued', 'needs_fixup', 'stuck', 'base_red'] },
  exit_code: { type: 'integer', description: 'pr_merge_watch exit code (0 merged, 4 timeout, 13 ejected, 14 unmergeable)' },
  summary: { type: 'string' }, failure_log: { type: 'string' }, problems: PROBLEMS } }
const PARK = { type: 'object', required: ['parked', 'clean', 'summary'], properties: {
  parked: { type: 'boolean', description: "true when the goal's changes were committed to the park branch" },
  branch: { type: 'string' }, files: { type: 'array', items: { type: 'string' } },
  clean: { type: 'boolean', description: "git status --porcelain shows none of the goal's paths afterwards" },
  summary: { type: 'string' }, problems: PROBLEMS } }

const CLASSIFY = {
  type: 'object', required: ['children', 'groups', 'order', 'confident'],
  properties: {
    children: { type: 'array', items: { type: 'object', required: ['id', 'track'], properties: {
      id: { type: 'string' }, track: { type: 'string', enum: ['bug', 'feature'] },
      group: { type: 'string', description: 'feature group name; omitted for bugs or an unplaceable feature' },
      read: { type: 'boolean', description: "the child's issue body was read (gh issue view), not classified from its title alone" },
      depends_on_bugs: { type: 'array', items: { type: 'string' } } } } },
    groups: { type: 'array', items: { type: 'object', required: ['name', 'independent', 'children'], properties: {
      name: { type: 'string' }, independent: { type: 'boolean' }, children: { type: 'array', items: { type: 'string' } } } } },
    order: { type: 'array', items: { type: 'string' }, description: 'dependency order of child ids' },
    confident: { type: 'boolean' },
    problems: PROBLEMS,
  },
}

// Plan-only helpers (#1406): one classification line per child, and the
// threshold that decides whether a meta issue is worth regrouping.
// Child ids arrive as numbers (plan stages), strings or '#N' (goals, planner
// output); every comparison between them goes through idKey.
const idKey = (x) => String(x ?? '').trim().replace(/^#/, '')
const slug = (s) => String(s).toLowerCase().replace(/[^a-z0-9]+/g, '-').replace(/^-+|-+$/g, '').slice(0, 40).replace(/-+$/g, '')
const featureBranch = (meta, group) => `grind/meta-${meta}-${slug(group)}`
const classificationLines = (c, meta) => (c.children || []).map(ch => {
  const id = idKey(ch.id)
  return ch.track === 'bug'
    ? `#${id} bug → ${MAIN}`
    : `#${id} feature → ${featureBranch(meta, ch.group || 'feature')}`
})
// #1830: the planner's track flipped for identical input (a one-child meta
// read as bug, then feature, minutes apart). A feature is one part of a
// multi-part change, so a lone child, or one classified without reading its
// issue, takes the bug track; groups keep only the features left.
const stabilizeClassification = (c, goals = []) => {
  const lone = goals.length === 1
  const children = (c.children || []).map(ch => {
    if (ch.track !== 'feature' || (!lone && ch.read === true)) return ch
    const { group, depends_on_bugs, ...rest } = ch
    return { ...rest, track: 'bug' }
  })
  const features = new Set(children.filter(ch => ch.track === 'feature').map(ch => idKey(ch.id)))
  const groups = (c.groups || [])
    .map(g => ({ ...g, children: (g.children || []).filter(id => features.has(idKey(id))) }))
    .filter(g => g.children.length > 0)
  return { ...c, children, groups }
}
// `ids`: every child of the meta issue; one the planner left out is unplaced.
const thresholdVerdict = (c, ids = []) => {
  const children = c.children || []
  const groups = c.groups || []
  const names = new Set(groups.map(g => g.name))
  const classified = new Set(children.map(ch => idKey(ch.id)))
  if (c.confident !== true) return { path: 'simple', reason: 'classification not confident' }
  if (ids.some(id => !classified.has(idKey(id)))) return { path: 'simple', reason: 'a child could not be placed' }
  if (children.some(ch => ch.track === 'feature' && !(ch.group && names.has(ch.group)))) return { path: 'simple', reason: 'a child could not be placed' }
  if (children.length < 8) return { path: 'simple', reason: 'fewer than 8 children' }
  if (groups.filter(g => g.independent === true && (g.children || []).length >= 3).length < 2) return { path: 'simple', reason: 'fewer than 2 independent feature groups of 3+' }
  return { path: 'regroup' }
}

// Prework helpers (#1408): the plan comment is public, so local-only state
// (preflight, absolute paths) never leaves the machine.
const LOCAL_ONLY = ['repo_path', 'checkout', 'worktree', 'stash', 'wip', 'start_branch']
const stripLocal = (v) => Array.isArray(v)
  ? v.map(stripLocal)
  : (v && typeof v === 'object')
    ? Object.fromEntries(Object.entries(v).filter(([k]) => !LOCAL_ONLY.includes(k)).map(([k, x]) => [k, stripLocal(x)]))
    : v
const publicPlan = (p) => {
  const pub = stripLocal(JSON.parse(JSON.stringify(p)))
  if ('preflight' in pub) pub.preflight = 'handled'
  return pub
}
const PLAN_LIMIT = 65536
// Pretty JSON, except that an array element holding no nested array/object
// (a child id, a one-line child entry) stays on one line (#1408).
const planJson = (v, pad = '') => {
  const inner = pad + ' '
  const nests = (x) => !!x && typeof x === 'object' && Object.values(x).some(y => y && typeof y === 'object')
  if (Array.isArray(v)) {
    if (!v.length) return '[]'
    return '[\n' + v.map(x => inner + (nests(x) ? planJson(x, inner) : JSON.stringify(x === undefined ? null : x))).join(',\n') + '\n' + pad + ']'
  }
  if (v && typeof v === 'object') {
    const e = Object.entries(v).filter(([, x]) => x !== undefined && typeof x !== 'function')
    if (!e.length) return '{}'
    return '{\n' + e.map(([k, x]) => inner + JSON.stringify(k) + ': ' + planJson(x, inner)).join(',\n') + '\n' + pad + '}'
  }
  return JSON.stringify(v === undefined ? null : v)
}
// The prework agent posts a body as one single-quoted shell word, and clud's
// command hook reads a backtick, `$(`, `<(` or `>(` as a substitution even
// inside quotes. So a body holds no backtick, single quote, `$`, `<` or `>`
// outside its marker: the fence is `~~~json`, and those characters inside
// JSON strings become \u escapes (JSON.parse restores them).
const inert = (json) => json.replace(/[`'$<>]/g, (c) => '\\u' + c.charCodeAt(0).toString(16).padStart(4, '0'))
// The comment bodies for a plan: one when it fits in `limit` characters,
// else part 1 (every field, stages without their per-child entries) plus
// parts 2..N carrying each stage's children and their depends_on_bugs
// entries. The caller refuses to post if any body still reaches PLAN_LIMIT.
const planBodies = (p, limit = 60000) => {
  const cap = Math.min(limit, PLAN_LIMIT - 1)
  const pub = publicPlan(p)
  const id = pub.run_id
  const fence = (s) => '\n~~~json\n' + inert(s) + '\n~~~'
  const single = `<!-- grind:v1 plan run=${id} -->` + fence(planJson(pub))
  if (single.length <= cap) return [single]
  const stages = Array.isArray(pub.stages) ? pub.stages : []
  const isStage = (s) => !!s && typeof s === 'object' && !Array.isArray(s)
  const kids = (s) => isStage(s) && Array.isArray(s.children) ? s.children : []
  const deps = (s) => isStage(s) && s.depends_on_bugs && typeof s.depends_on_bugs === 'object' ? s.depends_on_bugs : {}
  const cid = (c) => String(c && typeof c === 'object' ? c.id : c)
  const has = (o, k) => Object.prototype.hasOwnProperty.call(o, k)
  // Part 1: every field; each stage keeps only depends_on_bugs entries that
  // name no child of it (per-child entries travel with the child).
  const head = { ...pub, stages: stages.map(s => {
    if (!isStage(s) || !Array.isArray(s.children)) return s
    const ids = new Set(s.children.map(cid))
    const rest = Object.fromEntries(Object.entries(deps(s)).filter(([k]) => !ids.has(String(k))))
    return { ...s, children: [], ...(has(s, 'depends_on_bugs') ? { depends_on_bugs: rest } : {}) }
  }) }
  const items = []
  stages.forEach((s, i) => kids(s).forEach(c => items.push({ i, c, d: has(deps(s), cid(c)) ? deps(s)[cid(c)] : undefined })))
  const body = (list) => {
    const out = []
    list.forEach(({ i, c, d }) => {
      let e = out.find(b => b.index === i)
      if (!e) out.push(e = { index: i, stage: stages[i].stage, group: stages[i].group, children: [] })
      e.children.push(c)
      if (d !== undefined) (e.depends_on_bugs = e.depends_on_bugs || {})[cid(c)] = d
    })
    return planJson({ stages: out })
  }
  // Room for a worst-case marker (part=9999/9999) and the fence. Sizes are
  // counted per item (an over-estimate of its lines), not re-serialized.
  const room = cap - `<!-- grind:v1 plan run=${id} part=9999/9999 -->`.length - 16
  const stageCost = (i) => inert(planJson({ index: i, stage: stages[i].stage, group: stages[i].group, children: [], depends_on_bugs: {} }, '  ')).length + 32
  const itemCost = ({ c, d }) => inert(JSON.stringify(c)).length + 12 + (d === undefined ? 0 : inert(planJson(d, '    ')).length + inert(JSON.stringify(cid(c))).length + 14)
  const chunks = []
  let cur = []
  let size = 32
  let seen = new Set()
  for (const it of items) {
    const cost = () => itemCost(it) + (seen.has(it.i) ? 0 : stageCost(it.i))
    if (cur.length && size + cost() > room) { chunks.push(cur); cur = []; size = 32; seen = new Set() }
    size += cost()
    seen.add(it.i)
    cur.push(it)
  }
  if (cur.length) chunks.push(cur)
  const n = chunks.length + 1
  const mark = (k) => `<!-- grind:v1 plan run=${id} part=${k}/${n} -->`
  return [
    mark(1) + fence(planJson(head)) + `\nThe plan continues in parts 2..${n} (part=k/${n}), posted right after this comment.`,
    ...chunks.map((ch, k) => mark(k + 2) + fence(body(ch))),
  ]
}
const PREWORK = { type: 'object', required: ['posted'], properties: {
  posted: { type: 'boolean' }, plan_url: { type: 'string' }, part_urls: { type: 'array', items: { type: 'string' } }, error: { type: 'string' }, problems: PROBLEMS } }

// Plan-only pass: one read-only grind-planner, no worker/reviewer/integrator/lander.
if (args.planOnly) {
  phase('Classify')
  const c = await agent(
    `Follow your built-in /grind-plan procedure in PLAN-ONLY mode (read-only: no Write/Edit, no worktree, no push, no branch).\n\n${REPO_HEADER} Meta issue: #${args.meta}.\nChildren of the meta issue:\n` +
    args.goals.map(g => `${g.id}: ${g.title}\n${g.brief}`).join('\n\n') +
    `\n\nClassify every child as bug or feature, name the feature groups and whether they are independent, which feature children depend on which bugs, and the dependency order. Classify every child listed above exactly once, using its id as given; set confident to false if any child cannot be placed. Set read to true only for a child whose issue you read; a lone child, or an unread one, takes the bug track. Return it through StructuredOutput.`,
    opts('planner', 'classify', 'Classify', CLASSIFY))
  // log() lines never reach the router (the Workflow result arrives as a
  // notification carrying only the return value), so the classification
  // lines and the keep message are returned too.
  collect(c, 'planner', 'classify')
  if (!c) {
    const keep = `keeping #${args.meta} as is: planner died`
    log(keep)
    return { planOnly: true, path: 'simple', reason: 'planner died', message: keep, lines: [], problems: allProblems() }
  }
  const stable = stabilizeClassification(c, args.goals)
  const classified = new Set((stable.children || []).map(ch => idKey(ch.id)))
  const lines = [...classificationLines(stable, args.meta),
    ...args.goals.filter(g => !classified.has(idKey(g.id))).map(g => `#${idKey(g.id)} unclassified`)]
  lines.forEach(l => log(l))
  const { path, reason } = thresholdVerdict(stable, args.goals.map(g => g.id))
  const nGroups = (stable.groups || []).length
  const message = path === 'simple'
    ? `keeping #${args.meta} as is: ${reason} (${nGroups} feature group${nGroups === 1 ? '' : 's'}, ${args.goals.length} children)`
    : `regroup #${args.meta}: ${nGroups} feature groups, ${args.goals.length} children`
  log(message)
  return { planOnly: true, path, reason, message, lines, classification: stable, problems: allProblems() }
}

// Prework (#1408): record the plan on the meta issue before any worker runs.
// A feature-stage call (#1409) reuses the comment the bug-stage call posted.
let PLAN_URL = args.plan_url || null
if (PLAN_URL) {
  log(`plan: ${PLAN_URL} (posted earlier; prework skipped)`)
} else if (args.plan) {
  phase('Prework')
  const bodies = planBodies(args.plan)
  const oversized = bodies.findIndex(b => b.length >= PLAN_LIMIT)
  if (oversized >= 0) {
    const note = `plan comment part ${oversized + 1} is ${bodies[oversized].length} characters (GitHub's limit is ${PLAN_LIMIT})`
    log(`plan comment not posted: ${note}; stopping before any worker`)
    return { stopped: 'prework', note, problems: allProblems() }
  }
  const pw = collect(await agent(
    `Follow your built-in /grind-prework procedure.\n\n${REPO_HEADER} Meta issue: #${args.plan.meta}.\n` +
    `Post these ${bodies.length} comment bod${bodies.length === 1 ? 'y' : 'ies'} verbatim, in order:\n\n` +
    bodies.map((b, i) => `--- body ${i + 1} of ${bodies.length} ---\n${b}`).join('\n\n'),
    opts('prework', 'prework', 'Prework', PREWORK)), 'prework', 'prework')
  if (!pw || !pw.posted || !pw.plan_url) {
    const note = !pw ? 'prework agent died' : (pw.error || (pw.posted ? 'no plan_url returned' : 'posted=false'))
    log(`plan comment not posted: ${note}; stopping before any worker`)
    logProblems()
    return { stopped: 'prework', note, problems: allProblems() }
  }
  PLAN_URL = pw.plan_url
  log(`plan: ${PLAN_URL}`)
}

// Stages (#1409): the bugs stage lands on main first, then each feature stage
// lands on its own feature branch. A goal's base comes from its stage.
const ALL_STAGES = (args.plan && Array.isArray(args.plan.stages)) ? args.plan.stages.filter(s => s && typeof s === 'object') : []
const isBugStage = (s) => s.stage === 'bugs'
// One feature per run, no overlap (#1412): deferred groups and, under
// rules.no_overlap 'bugs_only', an open feature PR drop feature stages here.
const DEFERRED = new Set((args.plan && Array.isArray(args.plan.deferred_groups) ? args.plan.deferred_groups : [])
  .map(d => d && d.group).filter(Boolean).map(String))
const WAITING_PR = args.plan && args.plan.waiting_on_pr
const BUGS_ONLY = !!(WAITING_PR && args.plan.rules && args.plan.rules.no_overlap === 'bugs_only')
const stageName = (s) => String(s.group || s.name || s.branch || 'feature')
if (BUGS_ONLY) log(`no overlap: feature PR #${WAITING_PR} open under meta ${args.plan.meta || args.meta || '(unknown)'}; bugs only`)
const removedStages = []
let keptFeature = false
const STAGES = ALL_STAGES.filter(s => {
  if (isBugStage(s)) return true
  let note = null
  if (BUGS_ONLY) note = `no overlap: waiting on feature PR #${WAITING_PR}`
  else if (s.group && DEFERRED.has(String(s.group))) note = `deferred group ${s.group}`
  else if (keptFeature) note = `deferred group ${stageName(s)}`
  if (note) {
    log(`deferred: ${stageName(s)}`)
    removedStages.push({ s, note })
    return false
  }
  keptFeature = true
  return true
})
const keptIds = new Set()
STAGES.forEach(s => (Array.isArray(s.children) ? s.children : []).forEach(c => keptIds.add(idKey(c))))
const DEFERRED_GOALS = []
const deferredNote = {}
removedStages.forEach(({ s, note }) => (Array.isArray(s.children) ? s.children : []).forEach(c => {
  const id = idKey(c)
  if (!keptIds.has(id) && !(id in deferredNote)) deferredNote[id] = note
}))
// A deferred group's children never run this time, even when the router left
// its stage out of `stages` (a bugs-only plan moves every group here).
;(args.plan && Array.isArray(args.plan.deferred_groups) ? args.plan.deferred_groups : []).forEach(d => {
  if (!d || !Array.isArray(d.children)) return
  const note = BUGS_ONLY ? `no overlap: waiting on feature PR #${WAITING_PR}` : `deferred group ${stageName(d)}`
  d.children.forEach(c => {
    const id = String(c)
    if (!keptIds.has(id) && !(id in deferredNote)) deferredNote[id] = note
  })
})
args.goals = args.goals.filter(g => {
  const note = deferredNote[idKey(g.id)]
  if (note === undefined) return true
  DEFERRED_GOALS.push({ goal: g.id, id: g.id, merged: false, status: 'deferred', note, problems: [] })
  return false
})
const stageBase = {}
const stageOf = {}
STAGES.forEach(s => (Array.isArray(s.children) ? s.children : []).forEach(c => {
  const id = idKey(c)
  if (s.base && !(id in stageBase)) stageBase[id] = s.base
  if (!(id in stageOf)) stageOf[id] = s
}))
// Feature-branch mode (#1410): feature-stage goals land on args.feature.branch.
// No feature stage survived the #1412 filter: nothing lands on the feature branch.
const NO_FEATURE_LEFT = BUGS_ONLY || (removedStages.length > 0 && !keptFeature)
const FEATURE = (args.feature && args.feature.branch && !NO_FEATURE_LEFT) ? args.feature : null
// The router stores 'auto' | 'later' | 'comment'; the long spellings are accepted too.
const FEATURE_MERGE_ALIASES = { auto: 'auto', later: 'decide_later', decide_later: 'decide_later', comment: 'comment_only', comment_only: 'comment_only' }
const FEATURE_MERGE = [args.feature_merge, args.plan && args.plan.feature_merge]
  .map(m => FEATURE_MERGE_ALIASES[m]).find(Boolean) || 'decide_later'
// Where the router files problems (#1411): 'issue' (default) or 'comment'.
const PROBLEM_REPORTING = [args.problem_reporting, args.plan && args.plan.problem_reporting]
  .find(m => m === 'issue' || m === 'comment') || 'issue'
const isFeatureGoal = (g) => {
  if (!FEATURE) return false
  const s = stageOf[idKey(g.id)]
  return s ? !isBugStage(s) : !STAGES.length
}
const baseOf = (g) => g.base || stageBase[idKey(g.id)] || (isFeatureGoal(g) ? FEATURE.branch : null) || args.base || MAIN
const featureCheckout = (g) => (!PARALLEL && isFeatureGoal(g))
  ? `\nThis is a feature-stage goal: work in the feature worktree ${FEATURE.worktree} (never the user's checkout ${REPO}); that is the goal's checkout.`
  : ''
const featurePlanNote = (g) => !isFeatureGoal(g) ? '' : PARALLEL
  ? `\n\nFeature-stage goal: create the goal worktree and branch from origin/${FEATURE.branch}, not origin/${MAIN}.`
  : `\n\nFeature-stage goal, sequential mode: checkout must be the feature worktree ${FEATURE.worktree}, never ${REPO}; branch from origin/${FEATURE.branch}.`
// #1393: the feature PR number, the run id, and the issue that carries the
// feature's Closes line and marker (a meta of metas' sub-meta, else the meta).
const featurePrNum = () => String(FEATURE.pr || '').replace(/\/+$/, '').replace(/^.*\//, '').replace(/^#/, '')
const FEATURE_RUN_ID = (args.plan && args.plan.run_id) || (FEATURE ? String(FEATURE.branch).replace(/^grind\/meta-\d+-/, '') : '')
const featureMetaOf = (g) => (stageOf[String(g.id)] && stageOf[String(g.id)].sub_meta) || (args.plan && args.plan.meta) || args.meta
const featureIntegrateNote = (g) => !isFeatureGoal(g) ? '' :
  `\n\nFeature-stage goal (feature branch ${FEATURE.branch}, feature PR ${FEATURE.pr || '(none)'}, feature worktree ${FEATURE.worktree}):\n` +
  `1. Before rebasing the goal onto origin/${FEATURE.branch}: git fetch; if origin/${MAIN} has commits not in origin/${FEATURE.branch}, ` +
  `merge ${MAIN} into the feature branch in the feature worktree (first git merge --ff-only origin/${FEATURE.branch}, then git merge --no-ff origin/${MAIN}), then plain push of ${FEATURE.branch} (no force). ` +
  `Never rebase the feature branch.\n` +
  `2. Rebase the goal branch onto origin/${FEATURE.branch}. The goal PR's base is ${FEATURE.branch}, not ${MAIN}; its body uses \`Refs #${g.id}\`, not Closes.\n` +
  `3. Do not edit the feature PR: the lander adds \`Closes #${g.id}\` to it only once this goal has landed on the feature branch, so a goal that never lands is never closed by the feature merge.`
const featureLandNote = (g) => {
  if (!isFeatureGoal(g)) return ''
  const fpr = featurePrNum() || '<feature pr>'
  const meta = featureMetaOf(g)
  const original = args.plan && args.plan.original
  const marker = (goalPr) => `<!-- grind:v1 feature-pr=#${fpr} branch=${FEATURE.branch}${goalPr ? ` goal-pr=#${goalPr}` : ''} run=${FEATURE_RUN_ID || '<run-id>'} -->`
  const others = [meta, original].filter(Boolean).map(n => `#${n}`)
  return `\n\nFeature-stage goal: merge this PR into the feature branch ${FEATURE.branch} with \`gh pr merge <n> --admin --merge --repo ${GITHUB_REPO}\` (never --delete-branch), never into ${MAIN}.\n` +
    `Once it has merged, record the landing so issue #${g.id} is never lost (#1393):\n` +
    `1. gh issue comment ${g.id} --repo ${GITHUB_REPO} --body 'Landed on feature branch ${FEATURE.branch} via #<n>; closes when feature PR #${fpr} merges into ${MAIN}. ${marker('<n>')}'` +
    (others.length ? `. For ${others.join(' and ')}: read its comments (gh issue view <m> --json comments --repo ${GITHUB_REPO}) and, only if none has a marker with feature-pr=#${fpr}, post one: ${marker(null)}` : '') + `.\n` +
    `2. gh pr view ${fpr} --json body --repo ${GITHUB_REPO}, then gh pr edit ${fpr} --repo ${GITHUB_REPO} --body '<body>' with a \`Closes #${g.id}\` line added and the goals table row for #${g.id} updated; keep every other line, including the other Closes lines. ` +
    `View it again and repeat the edit if \`Closes #${g.id}\` is missing (another lander may have edited the body at the same time).\n` +
    `Never create or apply a label (#1958). Never gh issue close; the feature PR's Closes lines close the issues when it merges into ${MAIN}.`
}
// Stuck-bug rule (#1409): `block_dependents_only` is the only rule the spec
// defines, so it also applies when the plan omits rules.stuck_bug.
const STUCK_RULE = args.plan && args.plan.rules && args.plan.rules.stuck_bug
const STUCK_BUG_BLOCKS = !!args.plan && (!STUCK_RULE || STUCK_RULE === 'block_dependents_only')
// Bugs that did not merge in an earlier, bug-stage call of this run.
const STUCK_BUGS = new Set((Array.isArray(args.stuck_bugs) ? args.stuck_bugs : []).map(idKey))
const bugDepsOf = (g) => {
  const s = stageOf[idKey(g.id)]
  if (!s || isBugStage(s) || !s.depends_on_bugs || typeof s.depends_on_bugs !== 'object') return []
  const entry = Object.entries(s.depends_on_bugs).find(([k]) => idKey(k) === idKey(g.id))
  return entry && Array.isArray(entry[1]) ? entry[1].map(idKey) : []
}
// The goal id (as the router spelled it) for a normalized id.
const goalIdOf = {}
args.goals.forEach(g => { goalIdOf[idKey(g.id)] = g.id })

// Only approved dependencies belong in a goal's context. Their outcomes are
// recorded before the landed promise settles, so even parallel planners see
// the final upstream PR and result rather than a guessed branch or URL.
const upstreamContext = (g) => {
  const approved = RUN_GOALS.get(String(g.id))
  if (!approved || !approved.blockers.length) return ''
  const results = approved.blockers.map(b => ({
    goal: b.goal, repo: RUN_GOALS.get(b.goal).repo, condition: b.when,
    result: outcome[b.goal] || null,
  }))
  return `\nUpstream results (approved blockers):\n${JSON.stringify(results, null, 1)}`
}

const ctx = (g) => `${REPO_HEADER} Mode: ${args.mode}. Local CI (act): ${CI ? `on, lane ${args.lane || '(pick from ci.yml)'}` : 'off'}.\nGoal ${g.id}: ${g.title}\n${g.brief}` +
  (PLAN_URL ? `\nPlan comment: ${PLAN_URL} (read it before you start; it is the recorded plan and never changes).` : '') +
  featureCheckout(g) + upstreamContext(g)
const others = (g) => args.goals.filter(o => o.id !== g.id).map(o => `${o.id}: ${o.title}`).join('\n') || '(none)'

const plan = (g) => light(() => agent(
  `Follow your built-in /grind-plan procedure.\n\n${ctx(g)}\nBase branch: ${baseOf(g)} (branch from origin/${baseOf(g)}).\n\nOther goals in this run (for depends_on):\n${others(g)}` +
  (SCRIPTS ? `\n\nRun scripts chosen: ${scriptLines().join(', ')}. The integrator runs them before every push, so verify must hold only the goal's focused test; do not add lint or test commands.` : '') +
  featurePlanNote(g),
  opts('planner', `plan:${g.id}`, 'Plan', PLAN)).then(r => collect(r, 'planner', g.id)))

// The context pack (#1339, #1344): rendered once per goal and placed first
// in every worker's and the reviewer's prompt, so their requests are
// byte-identical up to PACK_END and the prompt cache serves the pack after
// the first agent. Nothing that varies per agent may appear in it.
const PACK_END = '\n--- end of context pack ---\n'
const packOf = (p) => {
  const c = (p && p.context) || {}
  const excerpts = Array.isArray(c.excerpts) ? c.excerpts : []
  const issue = String(c.issue || '').trim()
  if (!issue && !excerpts.length) return ''
  const parts = ['Context pack for this goal. Read it before anything else; do not re-fetch the issue or re-read these ranges.']
  if (issue) parts.push(`Issue:\n${issue}`)
  for (const e of excerpts) parts.push(`${e.path}:${e.start}-${e.end}${e.why ? ` (${e.why})` : ''}\n${String(e.text || '')}`)
  return parts.join('\n\n') + PACK_END
}

// Parallel workers start together on purpose (#1344): Claude Code puts a
// git status snapshot ahead of each agent's prompt, so a worker started
// after another has written files would see a different prefix.
const work = (p, g) => (PARALLEL
  ? parallel(p.tasks.map(t => () => light(() => runTask(p, g, t))))
  : p.tasks.reduce((acc, t) => acc.then(rs => runTask(p, g, t).then(r => [...rs, r])), Promise.resolve([])))
  .then(rs => ({ p, results: rs.filter(Boolean) }))
const runTask = (p, g, t) => agent(
  `${packOf(p)}Follow your built-in /grind-work procedure.\n\n${ctx(g)}\n\nCheckout: ${p.checkout}\nTask ${t.id}. Files you own: ${t.files.join(', ')}\n\n${t.instructions}`,
  opts('worker', `work:${g.id}/${t.id}`, 'Work', WORK)).then(r => collect(r, 'worker', g.id))

const review = ({ p, results }, g) => light(() => agent(
  `${packOf(p)}Follow your built-in /grind-review procedure.\n\n${ctx(g)}\n\nCheckout: ${p.checkout}\nWorker reports:\n${JSON.stringify(results, null, 1)}`,
  opts('reviewer', `review:${g.id}`, 'Review', REVIEW)).then(r => collect(r, 'reviewer', g.id))).then(r => ({ p, review: r }))

const integrate = (p, g, note) => exclusive(() => agent(
  `Follow your built-in /grind-integrate procedure.\n\n${ctx(g)}\n\nCheckout: ${p.checkout}\nBranch: ${p.branch}\n` +
  `Base: origin/${baseOf(g)}${p.depends_on.length ? ` (it already contains ${p.depends_on.join(', ')}, which landed first)` : ''}\n` +
  `${verifyBlock(p)}\n\n${note}${featureIntegrateNote(g)}\nReturn any release version, tag and release URL produced by this goal in artifacts; omit unknown values.`,
  opts('integrator', `integrate:${g.id}`, 'Integrate', INTEG)).then(r => collect(r, 'integrator', g.id)))

// The lander ends at enqueue (#2467, DD-192): once required PR checks are
// green it submits with pr_merge.py and returns `enqueued`; it never waits
// for the merge and never rebases. A chaperone follows the queue.
const ENQUEUE_NOTE = `\nWhen the PR's required checks are green, enqueue it with \`clud tool run github/pr_merge.py -- <n> --repo ${GITHUB_REPO} --match-head-commit <full-head-sha>\`` +
  ` and return status=enqueued with head_sha. Do not wait for the merge: a grind-chaperone follows the queue. If pr_merge.py reports the PR merged at once (no queue), return status=merged.` +
  `\nRed-base breaker: on a failed required check, compare it with ${MAIN}'s latest push and merge_group runs; if the same job fails there, return status=base_red (no fix round, no rerun).`
const land = (p, g, pr, round, label = `land:${g.id}#${round}`) => agent(
  `Follow your built-in /grind-land procedure.\n\n${ctx(g)}\n\nPR: ${pr}\nBranch: ${p.branch}\nFix rounds used: ${round} of ${MAX_FIX}.${isFeatureGoal(g) ? '' : ENQUEUE_NOTE}${featureLandNote(g)}\nReturn any observed release version, tag and release URL in artifacts; omit unknown values.`,
  opts('lander', label, 'Land', LAND)).then(r => collect(r, 'lander', g.id))

// Parking (#1424): in sequential mode every goal shares one checkout, so a
// goal that ends unmerged with work nobody pushed would leak its files into
// the next goal's lint and plan. The workflow has no shell; the integrator
// moves the goal's files to a local wip/grind-<goal> branch, under the build
// lock, and returns the checkout to origin/<base>.
const parkBranch = (g) => `wip/grind-${slug(g.id) || 'goal'}`
const goalFiles = (p, results) => [...new Set([
  ...(p.tasks || []).flatMap(t => t.files || []),
  ...(results || []).flatMap(r => (r && Array.isArray(r.files_touched)) ? r.files_touched : []),
].map(String).filter(Boolean))]
const preflightNote = () => (args.plan && args.plan.preflight)
  ? JSON.stringify(args.plan.preflight)
  : '(none recorded: the checkout was clean when the run started)'
const park = (p, results, g, why) => exclusive(() => agent(
  `Follow the Park procedure of your built-in /grind-integrate instructions (park only: no goal commit, push, PR, lint or test).\n\n${ctx(g)}\n\n` +
  `PARK goal ${g.id}: ${why}. Its changes must not stay in the checkout for the next goal.\n` +
  `Checkout: ${p.checkout}\nPark branch: ${parkBranch(g)} (local only, never pushed)\nReturn to: origin/${baseOf(g)}\n` +
  `Goal files: ${goalFiles(p, results).join(', ') || '(none listed)'}\n` +
  `Preflight (the user's pre-run state; never park, reset or discard it): ${preflightNote()}`,
  opts('integrator', `park:${g.id}`, 'Integrate', PARK)).then(r => collect(r, 'integrator', g.id)))

// Dependents wait until what they depend on has merged, then rebase onto the
// new origin/<base> of their stage (main for bugs, the feature branch for a
// feature stage); isolated goals go straight to origin/<base>.
//
// A goal may depend only on goals listed *before* it. That rules out cycles
// and self-dependencies, and in sequential mode it means every dependency has
// already settled, so no wait can hang.
const landed = {}
const settle = {}
const order = {}
args.goals.forEach((g, i) => {
  order[g.id] = i
  landed[g.id] = new Promise(r => { settle[g.id] = r })
})
const earlierDeps = (p, g) => {
  const approved = RUN_GOALS.get(String(g.id))
  const merged = approved ? approved.blockers.filter(b => b.when === 'merged').map(b => b.goal) : []
  const deps = [...new Set([...(p.depends_on || []), ...merged])].map(d => { const id = goalIdOf[idKey(d)]; return id === undefined ? String(d) : String(id) })
  const kept = deps.filter(d => d in order && order[d] < order[g.id])
  const dropped = deps.filter(d => !kept.includes(d))
  if (dropped.length) log(`goal ${g.id}: ignoring depends_on ${dropped.join(', ')} (unknown, itself, or not listed earlier)`)
  return { ...p, depends_on: kept }
}

// Workers and reviewers cannot run commands (#1397), so a task that writes no
// file is a check: hand it to the integrator's verify instead of a worker.
const routeCheckTasks = (p) => {
  const tasks = p.tasks || []
  const writes = (t) => (t.files || []).length > 0
  const checks = tasks.filter(t => !writes(t))
  if (!checks.length) return { ...p, tasks }
  log(`moving file-less task(s) ${checks.map(t => t.id).join(', ')} into verify`)
  return {
    ...p,
    tasks: tasks.filter(writes),
    verify: [p.verify, ...checks.map(t => `# from planned task ${t.id} (may be prose; run the equivalent checks):\n${t.instructions}`)]
      .filter(Boolean).join('\n'),
  }
}

// Review gate (#1424): the reviewer cannot run anything, so "nothing has been
// run yet" is the integrator's to-do list, not a verdict. Such a rejection is
// overridden; the integrator runs must_verify and is told to treat any real
// defect the summary names as a reason not to push.
const NOT_RUN = new RegExp([
  String.raw`\bnothing (?:has|had) (?:yet )?been (?:run|executed|tested|verified)\b`,
  String.raw`\b(?:has|have|had)(?: not|n[’']t) (?:yet )?been (?:run|executed|tested|verified)\b`,
  String.raw`\bnot (?:been )?(?:run|executed|tested|verified) yet\b`,
  String.raw`\b(?:my role|reviewers?|i) (?:can ?not|can[’']t|may not|am not allowed to|am unable to|is unable to|are unable to) (?:run|build|lint|test|execute)\b`,
].join('|'), 'i')
const mustVerify = (rv) => (rv && Array.isArray(rv.must_verify) ? rv.must_verify : [])
  .map(s => String(s || '').trim()).filter(Boolean)
const reviewGate = (rv) => {
  if (rv && rv.approved) return { ok: true }
  if (rv && NOT_RUN.test(String(rv.summary || ''))) return { ok: true, overridden: true }
  return { ok: false }
}
// must_verify joins the verify commands, so fix rounds rerun it too.
const withMustVerify = (p, rv) => {
  const mv = mustVerify(rv)
  if (!mv.length) return p
  return { ...p, verify: [p.verify, `# reviewer must_verify (the reviewer cannot run anything; run each, and treat a failure as a defect to fix):`, ...mv].filter(Boolean).join('\n') }
}
const reviewNote = (rv, gate) => `Reviewer summary: ${rv.summary}` +
  (mustVerify(rv).length ? `\nThe reviewer's must_verify items are in the verify commands above; run every one before pushing.` : '') +
  (gate.overridden ? `\nThe reviewer returned approved=false only because nothing had been run yet; that is your job, so the rejection was overridden. If the summary names a real defect, fix it before pushing, or return pushed=false.` : '')

// Dependents (#1424): what a settled goal's dependents are told.
const outcome = {}
const depNote = (dep) => {
  const r = outcome[String(dep)]
  return `blocked: dependency ${dep} ${r && r.rejected ? 'was rejected' : r && r.blocked ? 'is blocked' : 'did not land'}`
}
const settledBlock = (p) => {
  const dep = p.depends_on.find(d => String(d) in outcome && !outcome[String(d)].merged)
  return dep === undefined ? null : { merged: false, blocked: true, note: depNote(dep) }
}

const integrateAndLand = async ({ p: planned, review: rv }, g) => {
  const gate = reviewGate(rv)
  if (!gate.ok) return { merged: false, rejected: !!rv, note: `review rejected: ${rv ? rv.summary : 'no review'}` }
  if (gate.overridden) log(`goal ${g.id}: the reviewer only said nothing had been run; integrating (the integrator runs the checks)`)
  const p = withMustVerify(planned, rv)
  for (const dep of p.depends_on) {
    if (!(await landed[dep])) return { merged: false, blocked: true, note: depNote(dep) }
  }
  let integ = await integrate(p, g, reviewNote(rv, gate))
  let agentErrors = 0
  // One watch per push: the first push, then one per fix round.
  for (let fixes = 0; ; fixes++) {
    if (!integ || !integ.pushed) return { merged: false, pushed: false, pr: integ && integ.pr_url, note: integ ? integ.failure_log || integ.summary : 'integrator died' }
    let l = await land(p, g, integ.pr_url, fixes)
    // The lander's own tool mistake: retry the landing, never the integrator,
    // and never spend a fix round (#1331).
    while (l && l.status === 'tool_misuse') {
      agentErrors++
      if (agentErrors > MAX_AGENT_ERRORS) return { merged: false, pushed: true, pr: integ.pr_url, note: `gave_up: tool misuse (${l.command || l.summary})` }
      log(`goal ${g.id}: lander tool misuse ${agentErrors} of ${MAX_AGENT_ERRORS} (${l.command || l.summary}); landing again, no fix round`)
      l = await land(p, g, integ.pr_url, fixes)
    }
    if (l && l.status === 'merged') return {
      merged: true, pushed: true, pr: integ.pr_url, note: l.summary,
      artifacts: { ...(integ.artifacts || {}), ...(l.artifacts || {}) },
    }
    if (l && l.status === 'enqueued') return enqueue(p, g, integ, l)
    if (l && l.status === 'base_red') return baseRed(integ.pr_url, l)
    if (!l || l.status === 'gave_up' || fixes >= MAX_FIX) return { merged: false, pushed: true, pr: integ.pr_url, note: l ? l.failure_log || l.summary : 'lander died' }
    log(`goal ${g.id}: PR not green, fix round ${fixes + 1} of ${MAX_FIX}`)
    integ = await integrate(p, g, `FIX ROUND ${fixes + 1} of ${MAX_FIX}: the PR ${integ.pr_url} failed. Fix, re-verify${SCRIPTS ? ` (focused test, then ${scriptLines().join(', then ')})` : ''}, push to the same branch.\n${l.failure_log || l.summary}`)
  }
}

// Merge-queue follow-through (#2467, DD-192). A goal is done at enqueue; each
// queued PR gets one deadline-bounded `pr_merge_watch --until merged` call,
// run by a checkout-free grind-chaperone on the cheapest model, whose verdict
// the workflow collects. NEEDS-FIXUP goes back to the integrator ahead of new
// work (sequential: between goals; parallel: in the goal's own chain), at most
// MAX_QUEUE_FIXUPS times, then STUCK. Finish gives a still-queued PR one more
// bounded wait and reports it `queued`, which is not a failure.
const QUEUE_WATCH_S = gateInteger('queueWatchSeconds', 540, 60, 3600)
const MAX_QUEUE_FIXUPS = gateInteger('queueFixups', 2, 0, 5)
const QUEUE = []
const baseRed = (pr, l) => ({ merged: false, pushed: true, pr, status: 'base-red',
  note: `base-red: ${l.summary}${l.failure_log ? ` (${l.failure_log})` : ''}; the same required job fails on ${MAIN}, so no fix round was spent` })
const settleOnce = (e, merged) => {
  if (e.settled) return
  e.settled = true
  settle[e.g.id](merged)
}
const finish = (e, status, merged, note) => {
  e.state = 'done'
  Object.assign(e.result, { merged, status, note, enqueued: false })
  settleOnce(e, merged)
}
const watchQueued = (e, tag) => agent(
  `You are following one enqueued /grind PR to its merge.\n${REPO_HEADER}\nGoal ${e.g.id}: ${e.g.title}${PLAN_URL ? `\nPlan comment: ${PLAN_URL}` : ''}\nPR: ${e.pr}\nHead: ${e.head || '(read it with gh pr view)'}\n` +
  `Run once, in the background, and wait for its notification: clud tool run github/pr_merge_watch.py -- ${e.pr} --repo ${GITHUB_REPO} --until merged --timeout ${QUEUE_WATCH_S}\n` +
  `Queue fixups used: ${e.fixups} of ${MAX_QUEUE_FIXUPS}. Return your verdict by the exit code.`,
  opts('chaperone', `chaperone:${e.g.id}#${tag}`, 'Chaperone', CHAPERONE)).then(r => collect(r, 'chaperone', e.g.id))
const judge = (e, v) => {
  const verdict = (v && v.verdict) || 'stuck'
  const why = v ? v.failure_log || v.summary : 'chaperone died'
  if (verdict === 'merged') return finish(e, 'merged', true, v.summary || 'merged by the queue')
  if (verdict === 'base_red') return finish(e, 'base-red', false, `base-red: ${why}`)
  if (verdict === 'queued') { e.state = 'queued'; return null }
  if (verdict === 'needs_fixup' && e.fixups < MAX_QUEUE_FIXUPS) { e.state = 'needs_fixup'; e.why = why; return null }
  if (verdict === 'needs_fixup') return finish(e, 'stuck', false, `stuck: dropped by the merge queue after ${e.fixups} fixup(s): ${why}`)
  return finish(e, 'stuck', false, `stuck: ${why}`)
}
const follow = async (e, tag) => {
  let v = null
  try { v = await watchQueued(e, tag) } catch (err) { v = { verdict: 'stuck', summary: `chaperone failed: ${err && err.message ? err.message : err}` } }
  return judge(e, v)
}
const fixup = async (e) => {
  e.fixups++
  const n = e.fixups
  log(`goal ${e.g.id}: queue fixup ${n} of ${MAX_QUEUE_FIXUPS} for ${e.pr}`)
  const integ = await exclusive(() => agent(
    `Follow your built-in /grind-integrate procedure.\n\n${ctx(e.g)}\n\nCheckout: ${e.p.checkout}\nBranch: ${e.p.branch}\n${verifyBlock(e.p)}\n\n` +
    `QUEUE FIXUP ${n} of ${MAX_QUEUE_FIXUPS}: the merge queue dropped ${e.pr}. Switch to ${e.p.branch}, rebase onto origin/${baseOf(e.g)} (git fetch first), ` +
    `fix what the queue reported, re-verify, push to the same branch (--force-with-lease), then return the checkout to origin/${baseOf(e.g)}.\n${e.why}`,
    opts('integrator', `fixup:${e.g.id}#${n}`, 'Integrate', INTEG)).then(r => collect(r, 'integrator', e.g.id)))
  if (!integ || !integ.pushed) return finish(e, 'stuck', false, `stuck: queue fixup ${n} not pushed: ${integ ? integ.failure_log || integ.summary : 'integrator died'}`)
  const l = await land(e.p, e.g, e.pr, 0, `land:${e.g.id}#fixup${n}`)
  if (l && l.status === 'enqueued') { e.state = 'enqueued'; e.head = l.head_sha || e.head; return null }
  if (l && l.status === 'merged') return finish(e, 'merged', true, l.summary)
  if (l && l.status === 'base_red') return finish(e, 'base-red', false, baseRed(e.pr, l).note)
  return finish(e, 'stuck', false, `stuck: re-enqueue after fixup ${n} failed: ${l ? l.failure_log || l.summary : 'lander died'}`)
}
// One chain per enqueue. Sequential mode never runs a fixup here: the shared
// checkout belongs to the goal in progress, so the sweep between goals does.
const chain = async (e) => {
  for (let watches = 0; ; watches++) {
    e.watches++
    await follow(e, e.watches)
    if (PARALLEL && e.state === 'needs_fixup') { await fixup(e); if (e.state === 'enqueued') continue }
    if (PARALLEL && e.state === 'queued' && watches < 2) continue
    break
  }
  // A dependent never waits on a sweep that cannot run while it does.
  if (e.state !== 'done') settleOnce(e, false)
}
const startChain = (e) => {
  e.idle = false
  e.running = chain(e).catch(err => finish(e, 'stuck', false, `stuck: ${err && err.message ? err.message : err}`)).finally(() => { e.idle = true })
}
const enqueue = (p, g, integ, l) => {
  const result = { merged: false, pushed: true, enqueued: true, status: 'enqueued', pr: integ.pr_url,
    note: l.summary, artifacts: { ...(integ.artifacts || {}), ...(l.artifacts || {}) } }
  const e = { g, p, pr: integ.pr_url, head: l.head_sha, result, fixups: 0, watches: 0, state: 'enqueued', settled: false }
  QUEUE.push(e)
  startChain(e)
  log(`goal ${g.id}: enqueued ${integ.pr_url}; done, the chaperone follows the queue`)
  return result
}
// Between goals (sequential) and at Finish: NEEDS-FIXUP first, ahead of new work.
const sweep = async (final) => {
  for (let progressed = true; progressed;) {
    progressed = false
    if (final) await Promise.all(QUEUE.map(e => e.running))
    for (const e of QUEUE.filter(q => q.idle)) {
      if (e.state === 'needs_fixup') {
        await fixup(e)
        if (e.state === 'enqueued') startChain(e)
        progressed = true
      } else if (e.state === 'queued' && !final) {
        startChain(e)
      } else if (e.state === 'queued' && !e.finalDone) {
        e.finalDone = true
        await follow(e, 'final')
        progressed = true
      }
    }
  }
  if (!final) return
  for (const e of QUEUE.filter(q => q.state !== 'done')) {
    if (e.state === 'queued') finish(e, 'queued', false, `queued: ${e.pr} is still in the merge queue at Finish (not a failure)`)
    else finish(e, 'stuck', false, `stuck: ${e.why || 'unresolved in the merge queue'}`)
  }
}

// Set when a park left the shared sequential checkout dirty: every later
// goal is blocked rather than built on top of it (#1424).
let DIRTY_AFTER = null

const inspectGate = (g, blocker, index, attempt) => light(() => agent(
  `Observe this approved dependency gate once.\n${ctx(g)}\n` +
  `Observation: ${gateEpoch(g.id)}/${attempt}.\n` +
  `Upstream goal: ${blocker.goal}. Required condition: ${blocker.when}.\n` +
  `Exact approved Bash check (copy verbatim):\n${blocker.check}\n` +
  'Report ready only after successful native execution. Include the observed output as evidence. ' +
  'A failed or denied check means not ready. Do not retry, sleep, edit or publish.',
  opts('gate', `gate:${g.id}:${index}`, 'Gate', GATE)))

// The native Workflow owns this bounded schedule. Each checker observes once;
// its semaphore slot is released before the native timer wait. Distinct prompts
// prevent a prior not-ready agent result being replayed as the next observation.
const observeGate = async (g, blocker, index) => {
  let observed = null
  let attempts = 0
  for (let attempt = 1; attempt <= GATE_ATTEMPTS; attempt++) {
    attempts = attempt
    observed = null
    try {
      observed = await inspectGate(g, blocker, index, attempt)
    } catch (error) {
      if (error && error.name === 'AbortError') throw error
      observed = { ready: false, evidence: `check unavailable: ${error && error.message ? error.message : error}` }
      log(`goal ${g.id}: ${observed.evidence}`)
    }
    if (observed && observed.ready === true) break
    if (attempt < GATE_ATTEMPTS) {
      if (typeof setTimeout !== 'function') {
        log(`goal ${g.id}: native timer unavailable; deferring the gate`)
        break
      }
      const delay = Math.min(30000, GATE_BACKOFF_MS * 2 ** (attempt - 1))
      try {
        await new Promise(resolve => setTimeout(resolve, delay))
      } catch (error) {
        if (error && error.name === 'AbortError') throw error
        log(`goal ${g.id}: native timer failed; deferring: ${error && error.message ? error.message : error}`)
        break
      }
    }
  }
  return { ready: !!(observed && observed.ready === true),
    evidence: observed && observed.evidence || 'check unavailable',
    attempts }
}

// Enforce approved dependencies before a planner or worker touches this goal.
// A missing stage or external publication needs a resumable gate, not guessed
// success. Cross-repository execution needs a separately rooted session first.
const approvedDependencies = async (g, approved) => {
  const blocked = (note, gate) => ({ merged: false, blocked: true, note, ...(gate ? { gate } : {}) })
  if (approved.repo !== ORCHESTRATION.session_repo) {
    return blocked(`blocked: ${approved.repo} requires a separately rooted repository session`)
  }
  let dependencies
  try {
    // Subscribe to every blocker together: any hard failure wins immediately,
    // even if an earlier blocker is deferred or has not finished yet.
    dependencies = await Promise.all(approved.blockers.map(async b => {
      if (!(b.goal in landed) || (!PARALLEL && !(b.goal in outcome))) {
        throw blocked(`blocked: dependency ${b.goal} has not settled in this execution stage`, b)
      }
      if (await landed[b.goal]) return null
      const upstream = outcome[b.goal]
      if (upstream && upstream.status === 'deferred') return { blocker: b, upstream }
      throw blocked(depNote(b.goal), b)
    }))
  } catch (error) {
    if (error && error.blocked === true) return error
    throw error
  }
  const deferred = dependencies.find(Boolean)
  if (deferred) {
    const { blocker, upstream } = deferred
    return { ...blocked(depNote(blocker.goal), upstream.gate || blocker), status: 'deferred',
      waiting_on: blocker.goal, gate_evidence: upstream.gate_evidence || 'upstream gate unavailable' }
  }
  return null
}

const approvedChecks = async (g, approved) => {
  for (const [index, b] of approved.blockers.entries()) {
    if (b.when !== 'merged') {
      const observed = await observeGate(g, b, index)
      if (!observed.ready) {
        return { merged: false, blocked: true, gate: b,
          note: `blocked: dependency ${b.goal} still requires ${b.when} evidence`,
          status: 'deferred', gate_evidence: observed.evidence,
          gate_attempts: observed.attempts }
      }
    }
  }
  return null
}

// Restore the native branch before each phase, including after dependency
// promises resolve in another goal's context. Keep terminal results outside
// pipeline's null-means-stop convention, and preserve thrown errors for parking.
const inNativeStages = async (stages) => {
  let stopped = null
  let failure = null
  const results = await pipeline([true], ...stages.map(stage => async () => {
    if (stopped || failure) return true
    try { stopped = await stage() }
    catch (cause) { failure = { cause } }
    return true
  }))
  if (failure) throw failure.cause
  if (!results[0]) throw new Error('native goal stage did not complete')
  return stopped
}

const approvedBlock = (g) => {
  const approved = RUN_GOALS.get(String(g.id))
  return approved ? inNativeStages([
    () => approvedDependencies(g, approved),
    () => approvedChecks(g, approved),
  ]) : null
}

const attemptGoal = async (g, state) => {
  let worked
  let reviewed
  return inNativeStages([
    () => !PARALLEL && DIRTY_AFTER !== null
      ? { merged: false, blocked: true, note: `blocked: checkout not clean after parking goal ${DIRTY_AFTER}` } : null,
    () => approvedBlock(g),
    async () => {
      // Failed bugs block only the feature children that list them (#1409).
      if (STUCK_BUG_BLOCKS) {
        for (const bug of bugDepsOf(g)) {
          const dep = goalIdOf[bug]
          const stuck = dep === undefined ? STUCK_BUGS.has(bug) : !(await landed[dep])
          if (stuck) return { merged: false, blocked: true, note: `blocked: bug #${bug} did not land` }
        }
      }
      return null
    },
    async () => {
      const planned = await plan(g)
      if (!planned) return { merged: false, note: 'planner died' }
      // Dedup preflight (#2467): an open or merged PR already closes this goal.
      if (nonempty(planned.covered_by)) return { merged: false, status: 'covered', note: `covered by ${String(planned.covered_by).trim()}` }
      const p = routeCheckTasks(earlierDeps(planned, g))
      if (!p.tasks.length) return { merged: false, note: 'plan had no file-writing tasks; nothing for workers to do' }
      const early = settledBlock(p)
      if (early) return early
      state.p = p
      return null
    },
    async () => { worked = await work(state.p, g); state.results = worked.results },
    async () => { reviewed = await review(worked, g); state.review = reviewed.review },
    () => integrateAndLand(reviewed, g),
  ])
}

// Did anything write to the checkout? A rejected or blocked goal whose
// workers touched no file and whose reviewer applied no fix left nothing to
// park; any other failure may have (the integrator writes too).
const wroteSomething = (state, result) => {
  if (!result.rejected && !result.blocked) return true
  const touched = (state.results || []).some(r => r && Array.isArray(r.files_touched) && r.files_touched.length)
  const fixed = !!(state.review && Array.isArray(state.review.fixes_applied) && state.review.fixes_applied.length)
  return touched || fixed
}

// Sequential mode: a goal that wrote files and ends unmerged with nothing
// pushed has its changes parked before the next goal starts (#1424).
const parkIfNeeded = async (g, state, result) => {
  if (PARALLEL || !state.p || result.merged || result.pushed) return result
  if (!wroteSomething(state, result)) return result
  const why = result.rejected ? 'its review was rejected' : result.blocked ? 'it was blocked' : 'it failed before its work was pushed'
  let pk = null
  try {
    pk = await park(state.p, state.results, g, why)
  } catch (e) {
    log(`goal ${g.id}: park failed: ${e && e.message ? e.message : e}`)
  }
  if (!pk || !pk.clean) {
    DIRTY_AFTER = String(g.id)
    log(`goal ${g.id}: checkout not clean after parking; blocking the goals after it`)
    return { ...result, note: `${result.note}; checkout NOT clean after parking (${pk ? pk.summary : 'park agent died'})` }
  }
  return pk.parked
    ? { ...result, parked: pk.branch || parkBranch(g), note: `${result.note}; parked on ${pk.branch || parkBranch(g)}` }
    : result
}

// Every goal settles exactly once, whatever happens to it, so a dependent can
// never wait on a goal that died, threw, or was rejected.
const runGoal = async (g) => {
  const state = {}
  let result = { merged: false, note: 'dropped' }
  try {
    result = await attemptGoal(g, state)
  } catch (e) {
    result = { merged: false, note: `failed: ${e && e.message ? e.message : e}` }
  }
  try {
    result = await parkIfNeeded(g, state, result)
  } catch (e) {
    log(`goal ${g.id}: park step failed: ${e && e.message ? e.message : e}`)
  } finally {
    outcome[String(g.id)] = result
    // An enqueued goal settles when its chaperone reports (#2467).
    if (!result.enqueued) settle[g.id](!!result.merged)
  }
  return result
}

const runBatch = async (goals) => {
  if (PARALLEL) return parallel(goals.map(g => () => runGoal(g)))
  // One goal at a time, all in the local checkout, so the build cache is reused.
  const rs = []
  for (const g of goals) {
    await sweep(false)
    rs.push(await runGoal(g))
  }
  return rs
}

// Native resume stops using ALL cached agents at its first cache miss, even
// across parallel branches. Replay complete prior rounds before changing any
// gate prompt. Each round has the same goal list, branch slots and observations
// as that round had in the previous run; completed goals never enter later
// rounds. This also preserves sequential cache chains across independent goals.
const runApprovedRounds = async () => {
  const lastEpoch = Math.max(GATE_EPOCH, ...args.goals.map(g => requestedEpoch(g.id)))
  const byId = {}
  for (let epoch = GATE_EPOCH; epoch <= lastEpoch; epoch++) {
    replayEpoch = epoch
    const goals = args.goals.filter(g => requestedEpoch(g.id) >= epoch)
    for (const g of goals) {
      delete outcome[String(g.id)]
      landed[g.id] = new Promise(resolve => { settle[g.id] = resolve })
    }
    const round = await runBatch(goals)
    goals.forEach((g, i) => { byId[String(g.id)] = round[i] })
  }
  replayEpoch = null
  return args.goals.map(g => byId[String(g.id)])
}

let results
phase('Plan')
const featureStages = STAGES.filter(s => !isBugStage(s))
if (!ORCHESTRATION && STAGES.some(isBugStage) && featureStages.length) {
  // Bug stage first (plus any goal in no stage), then each feature stage in order.
  // The router normally starts one call per stage, so one batch is empty.
  const featureIds = new Set()
  const batches = featureStages.map(s => {
    const ids = new Set((Array.isArray(s.children) ? s.children : []).map(idKey))
    const goals = args.goals.filter(g => ids.has(idKey(g.id)) && !featureIds.has(idKey(g.id)))
    goals.forEach(g => featureIds.add(idKey(g.id)))
    return { label: `stage feature ${s.group || s.name || s.branch || 'feature'}`, goals }
  })
  batches.unshift({ label: 'stage bugs', goals: args.goals.filter(g => !featureIds.has(idKey(g.id))) })
  const byId = {}
  for (const b of batches.filter(b => b.goals.length)) {
    log(`${b.label}: ${b.goals.length} goal(s)`)
    const rs = await runBatch(b.goals)
    b.goals.forEach((g, i) => { byId[idKey(g.id)] = rs[i] })
  }
  results = args.goals.map(g => byId[idKey(g.id)])
} else {
  results = ORCHESTRATION ? await runApprovedRounds() : await runBatch(args.goals)
}

phase('Chaperone')
await sweep(true)
const summary = args.goals.map((g, i) => ({ goal: g.id, ...(results[i] || { merged: false, note: 'dropped' }), problems: problemsOf(g.id) }))
summary.filter(s => !s.merged).forEach(s => log(`NOT merged: goal ${s.goal}: ${s.note}`))
DEFERRED_GOALS.forEach(d => {
  log(`deferred: goal ${d.goal}: ${d.note}`)
  summary.push(d)
})
// The router combines this small manifest with the original approved arguments
// and the actual native Workflow launch references. No completed-goal bypass
// or invented run identifier is accepted from the model.
const remaining = summary.filter(s => ORCHESTRATION && s.status === 'deferred' &&
  s.gate && s.gate.when !== 'merged' && nonempty(s.gate.check)).map(s => String(s.goal))
const nextEpochs = { ...GATE_EPOCHS }
const exhausted = remaining.some(id => gateEpoch(id) >= MAX_GATE_EPOCH)
for (const id of remaining) nextEpochs[id] = Math.min(MAX_GATE_EPOCH, gateEpoch(id) + 1)
const resume = remaining.length ? {
  schema: 'grind-publication-resume/v1', remaining_goals: remaining, gateEpochs: nextEpochs,
  ...(exhausted ? { exhausted: true, reason: 'native replay history limit reached; retain deferred work' } : {}),
} : null
const resumeResult = resume ? { resume } : {}
if (PLAN_URL) log(`plan: ${PLAN_URL}`)
if (allProblems().length) log(`problems go to: ${PROBLEM_REPORTING} (the router files them)`)
if (!FEATURE) {
  logProblems()
  // Problems no goal owns (prework's) ride on one extra entry, so the router files them too.
  const goalIds = new Set(summary.map(s => String(s.goal)))
  const orphans = allProblems().filter(p => !goalIds.has(String(p.goal)))
  if (orphans.length) summary.push({ goal: 'run', status: 'problems', note: 'problems reported outside any goal', problems: orphans })
  // With a plan the router needs plan_url back for the feature-stage call.
  return args.plan || ORCHESTRATION ? { goals: summary, plan_url: PLAN_URL, problems: allProblems(), problem_reporting: PROBLEM_REPORTING, ...resumeResult } : summary
}

// Feature landing (#1410): once every feature-stage goal has settled.
const feature = { branch: FEATURE.branch, pr: FEATURE.pr || null, policy: FEATURE_MERGE, merged: false, note: '' }
if (resume) {
  feature.note = 'waiting for deferred publication goals; feature PR stays draft'
} else if (!FEATURE.pr) {
  feature.note = 'no feature PR'
} else if (FEATURE_MERGE === 'auto') {
  phase('Land')
  const l = collect(await agent(
    `Follow your built-in /grind-land procedure for the FEATURE PR.\n\n${REPO_HEADER}` +
    (PLAN_URL ? `\nPlan comment: ${PLAN_URL} (read it before you start; it is the recorded plan and never changes).` : '') +
    `\nFeature PR: ${FEATURE.pr}\nFeature branch: ${FEATURE.branch}\n\n` +
    `1. gh pr ready ${FEATURE.pr} --repo ${GITHUB_REPO}\n2. Wait for CI with github/pr_merge_watch.\n` +
    `3. gh pr merge ${FEATURE.pr} --merge --repo ${GITHUB_REPO} (no --admin, no --squash, no --delete-branch).\n` +
    `If a review is required and missing, return status 'gave_up' with summary 'waiting for review'; do not retry.`,
    opts('lander', 'land:feature', 'Land', LAND)), 'lander', 'feature')
  feature.merged = !!(l && l.status === 'merged')
  feature.note = l ? (l.failure_log || l.summary) : 'lander died'
} else if (FEATURE_MERGE === 'comment_only') {
  feature.note = 'the router posts the result comment; feature PR stays draft'
} else {
  feature.note = 'feature PR left open for the user; the router marks it ready once every feature goal landed'
}
log(`feature ${feature.branch}: PR ${feature.pr || '(none)'}, policy ${feature.policy}, ${feature.merged ? 'merged' : 'not merged'}: ${feature.note}`)
feature.problems = problemsOf('feature')
logProblems()
return { goals: summary, feature, plan_url: PLAN_URL, problems: allProblems(), problem_reporting: PROBLEM_REPORTING, ...resumeResult }
