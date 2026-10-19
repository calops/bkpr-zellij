// Real temporary git repositories; only GitHub HTTP is stubbed. No live writes.
import assert from 'node:assert/strict';
import { execFileSync, spawnSync } from 'node:child_process';
import { mkdtempSync, readFileSync, writeFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const self = fileURLToPath(import.meta.url);
const script = join(dirname(self), 'upstream-release.mjs');
const branch = 'update/zellij-v1.2.3';
function git(cwd, ...args) {
  return execFileSync('git', args, { cwd, encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] }).trim();
}

if (process.env.RELEASE_FIXTURE) {
  // Loaded with --import inside the automation subprocess before its entrypoint.
  const file = process.env.RELEASE_FIXTURE;
  globalThis.fetch = async (url, options = {}) => {
    const state = JSON.parse(readFileSync(file, 'utf8'));
    const path = new URL(url).pathname.replace('/repos/', '');
    const method = options.method || 'GET';
    const body = options.body ? JSON.parse(options.body) : undefined;
    let result;
    let status = 200;
    if (method === 'GET' && path === 'zellij-org/zellij/releases/latest') {
      result = { tag_name: 'v1.2.3', draft: false, prerelease: false };
    } else if (method === 'GET' && path === 'calops/bkpr-zellij/pulls') {
      result = state.pulls;
    } else if (method === 'GET' && path === `calops/bkpr-zellij/git/ref/heads/${branch}`) {
      const ref = spawnSync('git', ['rev-parse', '--verify', `refs/heads/${branch}`], { cwd: state.origin, encoding: 'utf8' });
      status = ref.status === 0 ? 200 : 404;
      result = ref.status === 0 ? { object: { sha: ref.stdout.trim() } } : { message: 'Not Found' };
    } else if (method === 'POST' && path === 'calops/bkpr-zellij/pulls') {
      result = { ...body, number: state.pulls.length + 1, state: 'open', html_url: 'https://github.com/calops/bkpr-zellij/pull/1' };
      state.pulls.push(result);
    } else if (method === 'POST' && path.startsWith('calops/bkpr-zellij/statuses/')) {
      state.statuses.push({ sha: path.split('/').at(-1), ...body });
      result = {};
    } else if (method === 'POST' && path === 'calops/bkpr-zellij/actions/workflows/rust.yml/dispatches') {
      state.dispatches.push(body);
      status = 204;
    } else {
      throw new Error(`Unexpected HTTP request: ${method} ${url}`);
    }
    writeFileSync(file, JSON.stringify(state));
    return new Response(status === 204 ? null : JSON.stringify(result), { status });
  };
} else {
  const { test } = await import('node:test');
  function fixture(kind) {
    const root = mkdtempSync(join(tmpdir(), 'release-smoke-'));
    const work = join(root, 'work');
    const origin = join(root, 'origin.git');
    const upstream = join(root, 'upstream.git');
    git(root, 'init', '--quiet', '-b', 'main', work);
    git(work, 'config', 'user.name', 'Release smoke');
    git(work, 'config', 'user.email', 'smoke@example.invalid');
    writeFileSync(join(work, 'shared.txt'), 'base\n');
    git(work, 'add', '.');
    git(work, 'commit', '--quiet', '-m', 'base');
    git(work, 'checkout', '--quiet', '-b', 'upstream-release');
    writeFileSync(join(work, kind === 'conflict' ? 'shared.txt' : 'upstream.txt'), 'upstream\n');
    git(work, 'add', '.');
    git(work, 'commit', '--quiet', '-m', 'release');
    git(work, 'tag', '-a', 'v1.2.3', '-m', 'stable release');
    const releaseSha = git(work, 'rev-parse', 'v1.2.3^{commit}');
    git(root, 'clone', '--quiet', '--bare', work, upstream);
    git(work, 'checkout', '--quiet', 'main');
    writeFileSync(join(work, kind === 'conflict' ? 'shared.txt' : 'fork.txt'), 'fork\n');
    git(work, 'add', '.');
    git(work, 'commit', '--quiet', '-m', 'fork customization');
    if (kind === 'integrated') git(work, 'merge', '--quiet', '--no-ff', 'v1.2.3', '-m', 'already integrated');
    const mainSha = git(work, 'rev-parse', 'main');
    git(root, 'clone', '--quiet', '--bare', '--no-tags', '--single-branch', '--branch', 'main', work, origin);
    const config = join(root, 'gitconfig');
    writeFileSync(config, `[url "${origin}"]\n\tinsteadOf = https://github.com/calops/bkpr-zellij.git\n[url "${upstream}"]\n\tinsteadOf = https://github.com/zellij-org/zellij.git\n`);
    const stateFile = join(root, 'state.json');
    writeFileSync(stateFile, JSON.stringify({ origin, pulls: [], statuses: [], dispatches: [] }));
    return {
      root, work, origin, releaseSha, mainSha,
      state: () => JSON.parse(readFileSync(stateFile, 'utf8')),
      save: (state) => writeFileSync(stateFile, JSON.stringify(state)),
      run: (...args) => {
        const result = spawnSync(process.execPath, ['--import', self, script, ...args], {
          cwd: root,
          env: { ...process.env, GH_TOKEN: 'fixture-only', GITHUB_TOKEN: '', GITHUB_STEP_SUMMARY: '', GIT_CONFIG_GLOBAL: config, GIT_CONFIG_NOSYSTEM: '1', RELEASE_FIXTURE: stateFile },
          encoding: 'utf8',
        });
        assert.equal(result.status, 0, `${result.stdout}\n${result.stderr}`);
        return result.stdout;
      },
      cleanup: () => rmSync(root, { recursive: true, force: true }),
    };
  }

  test('already-integrated release is a write-free no-op', () => {
    const f = fixture('integrated');
    try {
      f.run();
      assert.deepEqual(f.state(), { origin: f.origin, pulls: [], statuses: [], dispatches: [] });
      assert.equal(git(f.origin, 'for-each-ref', '--format=%(refname)', 'refs/tags'), '');
    } finally { f.cleanup(); }
  });

  test('clean release creates a two-parent merge, one PR and SHA-bound CI dispatch', () => {
    const f = fixture('clean');
    try {
      f.run();
      const head = git(f.origin, 'rev-parse', `refs/heads/${branch}`);
      assert.equal(git(f.origin, 'show', '-s', '--format=%P', head), `${f.mainSha} ${f.releaseSha}`);
      assert.equal(git(f.origin, 'for-each-ref', '--format=%(refname)', 'refs/tags'), '');
      const state = f.state();
      assert.equal(state.pulls.length, 1);
      assert.equal(state.pulls[0].base, 'main');
      assert.equal(state.pulls[0].draft, false);
      assert.equal(state.statuses[0].sha, head);
      assert.deepEqual(state.dispatches[0], { ref: 'main', inputs: { checkout_ref: head } });
      f.run();
      assert.equal(f.state().pulls.length, 1);
      assert.equal(git(f.origin, 'rev-parse', `refs/heads/${branch}`), head);
    } finally { f.cleanup(); }
  });

  test('conflict drafts preserve human resolution commits and PR edits on rerun', () => {
    const f = fixture('conflict');
    try {
      f.run();
      assert.equal(git(f.origin, 'rev-parse', `refs/heads/${branch}`), f.releaseSha);
      let state = f.state();
      assert.equal(state.pulls[0].draft, true);
      git(f.work, 'checkout', '--quiet', '-b', 'human-resolution', f.releaseSha);
      const merge = spawnSync('git', ['merge', '--no-ff', f.mainSha, '-m', 'resolve'], { cwd: f.work, encoding: 'utf8' });
      assert.equal(merge.status, 1);
      writeFileSync(join(f.work, 'shared.txt'), 'human resolved both changes\n');
      git(f.work, 'add', '.');
      git(f.work, 'commit', '--quiet', '-m', 'human resolution');
      const resolved = git(f.work, 'rev-parse', 'HEAD');
      git(f.work, 'push', f.origin, `HEAD:refs/heads/${branch}`);
      state.pulls[0].title = 'Reviewed human resolution';
      state.pulls[0].draft = false;
      f.save(state);
      f.run();
      state = f.state();
      assert.equal(state.pulls.length, 1);
      assert.equal(state.pulls[0].title, 'Reviewed human resolution');
      assert.equal(state.pulls[0].draft, false);
      assert.equal(git(f.origin, 'rev-parse', `refs/heads/${branch}`), resolved);
      assert.equal(state.dispatches.at(-1).inputs.checkout_ref, resolved);
    } finally { f.cleanup(); }
  });

  test('dry-run has no remote effects and closed PRs are not recreated', () => {
    const f = fixture('clean');
    try {
      f.run('--dry-run');
      assert.deepEqual(f.state(), { origin: f.origin, pulls: [], statuses: [], dispatches: [] });
      assert.equal(git(f.origin, 'for-each-ref', '--format=%(refname)', `refs/heads/${branch}`), '');
      f.run();
      const state = f.state();
      state.pulls[0].state = 'closed';
      f.save(state);
      f.run();
      assert.deepEqual(f.state(), state);
    } finally { f.cleanup(); }
  });
}
