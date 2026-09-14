// Artifact listings span attempts; downloading by name can overwrite new results with old ones.
module.exports = async function selectBenchmarkArtifact({ github, context }) {
  const manual = context.eventName === 'workflow_dispatch';
  const inputs = context.payload.inputs || {};
  const eventRun = context.payload.workflow_run;
  const runId = positiveId(manual ? inputs.run_id : eventRun.id);
  const requestedArtifact = manual ? positiveId(inputs.artifact_id) : null;
  const repo = context.repo;
  const { data: run } = await github.rest.actions.getWorkflowRun({ ...repo, run_id: runId });
  if (run.path !== '.github/workflows/bench-pr-run.yml' || run.event !== 'pull_request' ||
      run.status !== 'completed' || run.conclusion !== 'success' ||
      run.head_repository?.full_name !== `${repo.owner}/${repo.repo}`) {
    throw new Error('Expected a successful same-repository bench-pr-run pull request run');
  }
  if (!manual && (run.run_attempt !== eventRun.run_attempt || run.head_sha !== eventRun.head_sha)) {
    throw new Error('Benchmark completion event has been superseded by another attempt');
  }
  const jobs = await github.paginate(
    'GET /repos/{owner}/{repo}/actions/runs/{run_id}/attempts/{attempt_number}/jobs',
    { ...repo, run_id: runId, attempt_number: run.run_attempt, per_page: 100 },
  );
  const benchmarks = jobs.filter(job => job.name === 'benchmark' &&
    job.status === 'completed' && job.conclusion === 'success');
  if (benchmarks.length !== 1) throw new Error('Expected exactly one successful benchmark job');
  const job = benchmarks[0];
  const artifacts = await github.paginate(github.rest.actions.listWorkflowRunArtifacts,
    { ...repo, run_id: runId, per_page: 100 });
  // Legacy uploads have no attempt in their name. The successful job's upload time window
  // identifies its artifact without trusting list order or falling back to an older attempt.
  const candidates = artifacts.filter(artifact => artifact.name === 'benchmark_results' &&
    Date.parse(artifact.created_at) >= Date.parse(job.started_at) &&
    Date.parse(artifact.created_at) <= Date.parse(job.completed_at));
  candidates.sort((a, b) => Date.parse(b.created_at) - Date.parse(a.created_at) || b.id - a.id);
  const selected = candidates[0];
  if (!selected || selected.expired) throw new Error('Latest attempt has no available benchmark artifact');
  if (requestedArtifact !== null && selected.id !== requestedArtifact) {
    throw new Error('Requested artifact is not the latest successful attempt artifact');
  }
  return { runId, artifactId: selected.id, headSha: run.head_sha, attempt: run.run_attempt };
};

function positiveId(value) {
  if (!/^[1-9][0-9]*$/.test(String(value)) || !Number.isSafeInteger(Number(value))) {
    throw new Error('Expected a positive integer run/artifact ID');
  }
  return Number(value);
}
