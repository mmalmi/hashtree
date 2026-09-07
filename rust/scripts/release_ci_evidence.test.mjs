import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import { findReusableCiRun, requiredCiJobs } from './release_ci_evidence.mjs';

const repository = 'example/hashtree';
const sha = 'a'.repeat(40);

function fixture() {
  const workflow = { id: 12, path: '.github/workflows/ci.yml', state: 'active' };
  const run = {
    id: 34, workflow_id: 12, path: workflow.path, head_sha: sha,
    repository: { full_name: repository }, head_repository: { full_name: repository },
    event: 'push', head_branch: 'codex/release-example', run_attempt: 2,
    status: 'completed', conclusion: 'success',
  };
  const jobs = requiredCiJobs.map(name => ({
    name, run_id: run.id, run_attempt: run.run_attempt, head_sha: sha,
    status: 'completed', conclusion: 'success',
  }));
  const runs = [run];
  const requests = [];
  const actions = {
    getWorkflow: async args => {
      requests.push(args);
      return { data: workflow };
    },
    listWorkflowRuns: Symbol('runs'),
    listJobsForWorkflowRun: Symbol('jobs'),
  };
  const github = {
    rest: { actions },
    paginate: async (endpoint, args) => {
      requests.push(args);
      if (endpoint === actions.listWorkflowRuns) return runs;
      assert.equal(endpoint, actions.listJobsForWorkflowRun);
      return jobs;
    },
  };
  return { workflow, run, runs, jobs, requests, select: () => findReusableCiRun({ github, repository, sha }) };
}

test('reuses all successful jobs from the exact push commit and latest attempt', async () => {
  const f = fixture();
  assert.equal((await f.select()).id, 34);
  assert.deepEqual(f.requests, [
    { owner: 'example', repo: 'hashtree', workflow_id: 'ci.yml' },
    { owner: 'example', repo: 'hashtree', workflow_id: 12, event: 'push', head_sha: sha, per_page: 100 },
    { owner: 'example', repo: 'hashtree', run_id: 34, filter: 'latest', per_page: 100 },
  ]);
  f.run.head_branch = 'master';
  assert.equal((await f.select()).id, 34);
});

for (const [description, mutate] of [
  ['another commit', f => { f.run.head_sha = 'b'.repeat(40); }],
  ['another repository', f => { f.run.repository.full_name = 'other/hashtree'; }],
  ['a fork', f => { f.run.head_repository.full_name = 'fork/hashtree'; }],
  ['another workflow ID', f => { f.run.workflow_id++; }],
  ['another workflow path', f => { f.run.path = '.github/workflows/other.yml'; }],
  ['inactive CI', f => { f.workflow.state = 'disabled_manually'; }],
  ['wrong CI definition', f => { f.workflow.path = '.github/workflows/other.yml'; }],
  ['a pull request', f => { f.run.event = 'pull_request'; }],
  ['an untrusted branch', f => { f.run.head_branch = 'feature/example'; }],
  ['a nested release branch', f => { f.run.head_branch = 'codex/release-example/nested'; }],
  ['a release tag run', f => { f.run.head_branch = 'v0.2.142'; }],
  ['a running workflow', f => { f.run.status = 'in_progress'; }],
  ['a failed workflow', f => { f.run.conclusion = 'failure'; }],
  ['a missing required job', f => { f.jobs.pop(); }],
  ['a skipped job', f => { f.jobs[0].conclusion = 'skipped'; }],
  ['a failed job', f => { f.jobs[0].conclusion = 'failure'; }],
  ['an incomplete job', f => { f.jobs[0].status = 'queued'; }],
  ['a job from another run', f => { f.jobs[0].run_id++; }],
  ['a job from an older attempt', f => { f.jobs[0].run_attempt--; }],
  ['a job from another commit', f => { f.jobs[0].head_sha = 'b'.repeat(40); }],
  ['duplicate job evidence', f => { f.jobs.push({ ...f.jobs[0] }); }],
]) {
  test(`falls back for ${description}`, async () => {
    const f = fixture();
    mutate(f);
    assert.equal(await f.select(), null);
  });
}

test('falls back when there is no matching run', async () => {
  const f = fixture();
  f.runs.length = 0;
  assert.equal(await f.select(), null);
});

test('API failures propagate to the workflow fallback', async () => {
  const github = { rest: { actions: { getWorkflow: async () => { throw new Error('unavailable'); } } } };
  await assert.rejects(findReusableCiRun({ github, repository, sha }), /unavailable/);
});

test('artifact builds require CI evidence or every successful fallback gate', () => {
  const workflow = readFileSync(new URL('../../.github/workflows/release.yml', import.meta.url), 'utf8');
  const expression = workflow.match(/\n  build:[\s\S]*?\$\{\{ (always\(\)[\s\S]*?) \}\}/)?.[1];
  assert.ok(expression, 'release build must have an explicit status condition');
  // The workflow condition uses only boolean operators and property lookups shared with JS.
  const canBuild = new Function('needs', 'always', 'cancelled', `return (${expression});`);
  const gates = ['gate-static', 'gate-typescript', 'gate-rust', 'gate-rust-peripheral', 'gate-fips', 'pool-migration-systemd'];
  const needs = Object.fromEntries(gates.map(name => [name, { result: 'success' }]));
  needs['source-ci'] = { result: 'success', outputs: { reuse: 'false' } };
  const check = (cancelled = false) => canBuild(needs, () => true, () => cancelled);
  assert.equal(check(), true);
  for (const gate of gates) {
    for (const result of ['failure', 'skipped', 'cancelled']) {
      needs[gate].result = result;
      assert.equal(check(), false, `${gate}: ${result} must block fallback builds`);
    }
    needs[gate].result = 'success';
  }
  for (const gate of gates) needs[gate].result = 'skipped';
  needs['source-ci'].outputs.reuse = 'true';
  assert.equal(check(), true);
  assert.equal(check(true), false);
  needs['source-ci'].result = 'failure';
  assert.equal(check(), false);
});
