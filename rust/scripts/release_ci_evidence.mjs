// These jobs include the six shared release-gate lanes and CI's platform checks.
export const requiredCiJobs = Object.freeze([
  'TypeScript', 'Release Wiring', 'Pool Migration Systemd', 'Rust Tests',
  'macOS Updater Security', 'Windows Storage', 'Rust FUSE Smoke',
  'Rust Peripheral Tests', 'Rust FIPS WebRTC Tests',
]);

// The caller falls back to running the gates when evidence is absent or the API fails.
export async function findReusableCiRun({ github, repository, sha }) {
  const [owner, repo] = repository.split('/');
  if (!owner || !repo || !/^[a-f0-9]{40}$/.test(sha)) return null;
  const actions = github.rest.actions;
  const { data: workflow } = await actions.getWorkflow({ owner, repo, workflow_id: 'ci.yml' });
  if (!Number.isSafeInteger(workflow.id) || workflow.state !== 'active'
    || workflow.path !== '.github/workflows/ci.yml') return null;

  const runs = await github.paginate(actions.listWorkflowRuns, {
    owner, repo, workflow_id: workflow.id, event: 'push', head_sha: sha, per_page: 100,
  });
  for (const run of runs) {
    if (run.workflow_id !== workflow.id || run.path !== workflow.path
      || run.repository?.full_name !== repository || run.head_repository?.full_name !== repository
      || run.head_sha !== sha || run.event !== 'push'
      || !(run.head_branch === 'master' || /^codex\/release-[^/]+$/.test(run.head_branch ?? ''))
      || run.status !== 'completed' || run.conclusion !== 'success'
      || !Number.isSafeInteger(run.id) || !Number.isSafeInteger(run.run_attempt)) continue;

    const jobs = await github.paginate(actions.listJobsForWorkflowRun, {
      owner, repo, run_id: run.id, filter: 'latest', per_page: 100,
    });
    if (jobs.some(job => job.run_id !== run.id || job.run_attempt !== run.run_attempt
      || job.head_sha !== sha || job.status !== 'completed' || job.conclusion !== 'success')) continue;
    if (!requiredCiJobs.every(name => jobs.filter(job => job.name === name).length === 1)) continue;
    return run;
  }
  return null;
}
