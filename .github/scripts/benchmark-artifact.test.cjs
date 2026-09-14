const { test } = require('node:test');
const assert = require('node:assert/strict');
const select = require('./benchmark-artifact.cjs');

function fixture() {
  const run = { path: '.github/workflows/bench-pr-run.yml', event: 'pull_request',
    status: 'completed', conclusion: 'success', run_attempt: 2, head_sha: 'head',
    head_repository: { full_name: 'owner/repo' } };
  const context = { eventName: 'workflow_run', repo: { owner: 'owner', repo: 'repo' },
    payload: { workflow_run: { id: 123, run_attempt: 2, head_sha: 'head' } } };
  const jobs = [{ name: 'benchmark', status: 'completed', conclusion: 'success',
    started_at: '2026-09-14T17:14:07Z', completed_at: '2026-09-14T17:23:57Z' }];
  const artifacts = [
    { id: 10359339811, name: 'benchmark_results', created_at: '2026-09-14T17:23:52Z', expired: false },
    { id: 10358680745, name: 'benchmark_results', created_at: '2026-09-14T16:41:57Z', expired: false },
  ];
  const github = { rest: { actions: {
    getWorkflowRun: async () => ({ data: run }), listWorkflowRunArtifacts: 'artifacts',
  } }, paginate: async (endpoint, args) => {
    assert.equal(args.run_id, 123);
    if (endpoint === 'artifacts') return artifacts;
    assert.equal(args.attempt_number, 2);
    return jobs;
  } };
  return { run, context, jobs, artifacts, github };
}

test('same-name artifacts select the rerun in either API order', async () => {
  const f = fixture();
  for (const reverse of [false, true]) {
    if (reverse) f.artifacts.reverse();
    assert.deepEqual(await select(f), { runId: 123, artifactId: 10359339811, headSha: 'head', attempt: 2 });
  }
});
test('manual repair accepts only the exact artifact from the latest successful attempt', async () => {
  const f = fixture();
  f.context.eventName = 'workflow_dispatch';
  f.context.payload.inputs = { run_id: '123', artifact_id: '10359339811' };
  assert.equal((await select(f)).artifactId, 10359339811);
  f.context.payload.inputs.artifact_id = '10358680745';
  await assert.rejects(select(f), /Requested artifact/);
});
test('missing or expired current artifact never falls back to old successful data', async () => {
  for (const missing of [true, false]) {
    const f = fixture();
    if (missing) f.artifacts.shift(); else f.artifacts[0].expired = true;
    await assert.rejects(select(f), /no available benchmark artifact/);
  }
});
test('newest upload wins deterministically, ID breaks timestamp ties', async () => {
  const f = fixture();
  f.artifacts.push({ ...f.artifacts[0], id: 10359339812 });
  assert.equal((await select(f)).artifactId, 10359339812);
  f.artifacts.push({ ...f.artifacts[0], id: 10359339813, created_at: '2026-09-14T17:23:58Z' });
  assert.equal((await select(f)).artifactId, 10359339812);
});
test('stale completion events cannot publish a newer attempt', async () => {
  const f = fixture();
  f.context.payload.workflow_run.run_attempt = 1;
  await assert.rejects(select(f), /superseded/);
});
test('wrong workflow, fork, unsuccessful and incomplete runs are refused', async () => {
  for (const patch of [{ path: '.github/workflows/other.yml' }, { event: 'push' },
    { head_repository: { full_name: 'fork/repo' } }, { conclusion: 'failure' }, { status: 'in_progress' }]) {
    const f = fixture(); Object.assign(f.run, patch);
    await assert.rejects(select(f), /Expected a successful/);
  }
});
test('no successful benchmark job cannot reuse another job artifact', async () => {
  const f = fixture(); f.jobs[0].conclusion = 'skipped';
  await assert.rejects(select(f), /exactly one successful benchmark/);
});
test('invalid manual IDs are rejected before API access', async () => {
  const f = fixture(); f.context.eventName = 'workflow_dispatch';
  f.context.payload.inputs = { run_id: '123; echo bad', artifact_id: '10359339811' };
  await assert.rejects(select(f), /positive integer/);
});
