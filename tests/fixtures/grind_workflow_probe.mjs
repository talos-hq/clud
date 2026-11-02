// Controlled workflow host: no model requests, shell tools, or GitHub access.
import { readFile } from 'node:fs/promises';

const source = await readFile(process.argv[2], 'utf8');
const config = JSON.parse(await readFile(process.argv[3], 'utf8'));
const metadataExport = 'export const meta =';
if (source.split(metadataExport).length !== 2) {
  throw new Error('expected exactly one workflow metadata export');
}
// This controlled host wraps the body in an async function, allowing its
// top-level return. Remove only the metadata export keyword.
const AsyncFunction = Object.getPrototypeOf(async function () {}).constructor;
const execute = new AsyncFunction(
  'args', 'agent', 'log', 'phase', 'parallel', 'pipeline',
  source.replace(metadataExport, 'const meta ='),
);
const prompts = [];
const agent = async (prompt, options) => {
  prompts.push({ prompt, role: options.agentType, label: options.label, model: options.model });
  // Scripted replies (#2467): `script` maps a label prefix to a list of
  // replies, consumed in order; the longest matching prefix wins.
  const keys = Object.keys(config.script || {})
    .filter(key => options.label.startsWith(key)).sort((a, b) => b.length - a.length);
  if (keys.length && config.script[keys[0]].length) {
    const reply = config.script[keys[0]].shift();
    if (reply && reply.throw) throw new Error(reply.throw);
    if (reply && reply.delay_ms !== undefined) {
      await new Promise(resolve => setTimeout(resolve, reply.delay_ms));
      prompts.push({ done: options.label });
      return reply.value;
    }
    prompts.push({ done: options.label });
    return reply;
  }
  if (options.label === 'classify') {
    if (config.classify) return config.classify;
    return {
      confident: true,
      children: config.args.goals.map(goal => ({ id: goal.id, track: 'bug' })),
      groups: [],
      order: config.args.goals.map(goal => goal.id),
    };
  }
  const replies = {
    'grind-prework': {
      posted: true,
      plan_url: 'https://github.com/fork/project/issues/10#issuecomment-1',
      part_urls: ['https://github.com/fork/project/issues/10#issuecomment-1'],
    },
    'grind-planner': {
      checkout: config.args.repo, branch: 'grind/12', depends_on: [], verify: 'true',
      tasks: [{ id: 't1', files: ['src/a.rs'], instructions: 'controlled task' }],
    },
    'grind-worker': { files_touched: ['src/a.rs'], summary: 'controlled work' },
    'grind-reviewer': { approved: true, summary: 'controlled review' },
    'grind-integrator': {
      pushed: true, pr_url: 'https://github.com/fork/project/pull/34', summary: 'controlled push',
    },
    'grind-lander': { status: 'merged', summary: 'controlled landing' },
  };
  if (!(options.agentType in replies)) throw new Error('unexpected agent in workflow probe');
  return replies[options.agentType];
};
try {
  const result = await execute(
    config.args, agent, () => {}, () => {},
    tasks => Promise.all(tasks.map(task => task())),
    // Native pipeline: items run concurrently, each item's stages run in
    // order on the previous result, and a null result stops that item.
    (items, ...stages) => Promise.all(items.map(async item => {
      let value = item;
      for (const stage of stages) {
        value = await stage(value);
        if (value === null || value === undefined) return null;
      }
      return value;
    })),
  );
  console.log(JSON.stringify({ result, prompts }));
} catch (error) {
  console.log(JSON.stringify({ error: String(error.message || error), prompts }));
}
