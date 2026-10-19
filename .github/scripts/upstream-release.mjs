#!/usr/bin/env node
// Run only from trusted main. Never execute files from the fetched release.
import { execFileSync, spawnSync } from 'node:child_process';
import { appendFileSync, mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const repository = 'calops/bkpr-zellij';
const upstream = 'zellij-org/zellij';
const dryRun = process.argv.includes('--dry-run');
if (process.argv.slice(2).some((arg) => arg !== '--dry-run')) {
  throw new Error('Usage: node .github/scripts/upstream-release.mjs [--dry-run]');
}
const token = process.env.GH_TOKEN || process.env.GITHUB_TOKEN;
if (!token) throw new Error('Set GH_TOKEN (or GITHUB_TOKEN) to a GitHub token.');

async function api(path, { method = 'GET', body, allowMissing = false } = {}) {
  const response = await fetch(`https://api.github.com/repos/${path}`, {
    method,
    headers: {
      Accept: 'application/vnd.github+json',
      Authorization: `Bearer ${token}`,
      'X-GitHub-Api-Version': '2022-11-28',
      ...(body ? { 'Content-Type': 'application/json' } : {}),
    },
    body: body ? JSON.stringify(body) : undefined,
  });
  if (allowMissing && response.status === 404) return null;
  if (!response.ok) {
    throw new Error(`GitHub ${method} ${path}: ${response.status} ${await response.text()}`);
  }
  return response.status === 204 ? null : response.json();
}

function report(message) {
  console.log(message);
  if (process.env.GITHUB_STEP_SUMMARY) {
    appendFileSync(process.env.GITHUB_STEP_SUMMARY, `${message}\n\n`);
  }
}

async function main() {
  const release = await api(`${upstream}/releases/latest`);
  if (release.draft || release.prerelease || !/^v\d+\.\d+\.\d+$/.test(release.tag_name)) {
    throw new Error(`Expected a stable vX.Y.Z release; received ${release.tag_name}`);
  }
  const tag = release.tag_name;
  const branch = `update/zellij-${tag}`;
  const source = `https://github.com/${upstream}/releases/tag/${tag}`;
  const directory = mkdtempSync(join(tmpdir(), 'bkpr-zellij-release-'));
  // Pass authentication only to git itself, never persist it in a checkout.
  const gitEnv = {
    ...process.env,
    GIT_CONFIG_COUNT: '4',
    GIT_CONFIG_KEY_0: 'http.https://github.com/.extraheader',
    GIT_CONFIG_VALUE_0: `AUTHORIZATION: basic ${Buffer.from(`x-access-token:${token}`).toString('base64')}`,
    GIT_CONFIG_KEY_1: 'core.hooksPath',
    GIT_CONFIG_VALUE_1: '/dev/null',
    GIT_CONFIG_KEY_2: 'commit.gpgsign',
    GIT_CONFIG_VALUE_2: 'false',
    GIT_CONFIG_KEY_3: 'push.followTags',
    GIT_CONFIG_VALUE_3: 'false',
    GIT_TERMINAL_PROMPT: '0',
  };
  function git(...args) {
    return execFileSync('git', args, {
      cwd: directory, env: gitEnv, encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'],
    }).trim();
  }
  function ancestor(older, newer) {
    const result = spawnSync('git', ['merge-base', '--is-ancestor', older, newer], {
      cwd: directory, env: gitEnv, encoding: 'utf8',
    });
    if (result.error) throw result.error;
    if (result.status !== 0 && result.status !== 1) {
      throw new Error(`Cannot compare release ancestry: ${result.stderr}`);
    }
    return result.status === 0;
  }
  try {
    git('init', '--quiet');
    git('config', 'user.name', 'github-actions[bot]');
    git('config', 'user.email', '41898282+github-actions[bot]@users.noreply.github.com');
    git('remote', 'add', 'origin', `https://github.com/${repository}.git`);
    git('fetch', '--no-tags', 'origin', '+refs/heads/main:refs/remotes/origin/main');
    // Fetch only the exact release tag into a private local ref, including full
    // ancestry. Never create or push a refs/tags/* ref in the fork.
    git('fetch', '--no-tags', `https://github.com/${upstream}.git`, `refs/tags/${tag}:refs/upstream/release`);
    const releaseSha = git('rev-parse', 'refs/upstream/release^{commit}');
    if (ancestor(releaseSha, 'origin/main')) {
      report(`${tag} (${releaseSha}) is already an ancestor of main; nothing to do.`);
      return;
    }

    const pulls = await api(`${repository}/pulls?state=all&head=calops:${encodeURIComponent(branch)}&base=main&per_page=100`);
    const existingPull = pulls.find((pull) => pull.state === 'open') || pulls[0];
    if (existingPull && existingPull.state !== 'open') {
      report(`${existingPull.html_url} is ${existingPull.merged_at ? 'merged' : 'closed'}; respecting the human decision, not recreating it.`);
      return;
    }
    const existingRef = await api(`${repository}/git/ref/heads/${branch}`, { allowMissing: true });
    let conflicts = false;
    let headSha;
    if (existingRef) {
      git('fetch', '--no-tags', 'origin', `refs/heads/${branch}:refs/upstream/update`);
      headSha = git('rev-parse', 'refs/upstream/update');
      if (!ancestor(releaseSha, headSha)) {
        throw new Error(`${branch} does not contain ${tag}; refusing to overwrite existing work.`);
      }
      conflicts = headSha === releaseSha;
      report(`Preserving existing ${branch} at ${headSha}; no branch or PR edits.`);
    } else {
      if (existingPull) {
        throw new Error(`Open PR ${existingPull.html_url} has no head branch; refusing to replace it.`);
      }
      git('checkout', '--quiet', '-b', branch, 'origin/main');
      try {
        git('merge', '--no-ff', '--no-edit', '-m', `Merge upstream Zellij ${tag}`, releaseSha);
      } catch (error) {
        if (!git('ls-files', '--unmerged')) throw error;
        git('merge', '--abort');
        // A conflict draft starts at the release so a human can merge main and
        // resolve conflicts while retaining the upstream release's ancestry.
        git('reset', '--hard', releaseSha);
        conflicts = true;
      }
      headSha = git('rev-parse', 'HEAD');
      if (!dryRun) {
        // Empty expected value is a create-only lease: never replace a branch
        // that appeared after our lookup, even with a fast-forward.
        git('push', `--force-with-lease=refs/heads/${branch}:`, 'origin', `HEAD:refs/heads/${branch}`);
      }
      report(`${dryRun ? 'Would create' : 'Created'} ${branch} at ${headSha}${conflicts ? ' (merge conflicts; draft required)' : ' (merge commit)'}.`);
    }

    let pull = existingPull;
    if (!pull) {
      const body = [
        `Upstream stable release: [${tag}](${source})`,
        `Upstream release commit: \`${releaseSha}\`.`,
        '',
        conflicts
          ? '**Merge conflicts — manual resolution required.** This draft branch points at the upstream release. Merge `origin/main` into it locally, resolve conflicts, commit and push. Do not replace the release ancestry.'
          : 'This branch merges the upstream release into `main` with a merge commit, preserving upstream ancestry.',
        '',
        '**Human review and merge only. This automation never merges PRs or enables auto-merge.**',
        '- Review the entire diff and resolve conflicts, then mark the draft ready if applicable.',
        '- Review the SHA-bound `Rust release verification` status and its linked Rust workflow run. The GITHUB_TOKEN-created PR does not automatically trigger normal pull_request workflows.',
        '- After any human resolution push, wait for CI on the new head SHA. To rerun explicitly: `gh workflow run rust.yml --repo calops/bkpr-zellij --ref main -f checkout_ref=<FULL_PR_HEAD_SHA>`.',
        '- Run the manual Nix/build/runtime smoke checks in `docs/NIX.md`, and any missing end-to-end checks. Rust dispatch is not the pull_request merge-ref or e2e suite.',
        '- Merge with **Create a merge commit**, never squash or rebase: future release detection depends on the upstream tag remaining an ancestor of main.',
      ].join('\n');
      if (dryRun) {
        report(`Would open ${conflicts ? 'a conflict-marked draft' : 'an'} PR targeting main: ${source}`);
      } else {
        pull = await api(`${repository}/pulls`, {
          method: 'POST',
          body: { title: `${conflicts ? '[conflicts] ' : ''}Merge upstream Zellij ${tag}`, head: branch, base: 'main', body, draft: conflicts, maintainer_can_modify: true },
        });
      }
    }
    if (pull) report(`Human review required: ${pull.html_url}`);
    if (conflicts) {
      report('Resolve conflicts and merge main into the update branch before trusting verification or marking the PR ready.');
    }
    if (dryRun) {
      report(`Would set pending Rust release verification on ${headSha} and dispatch trusted rust.yml on main with checkout_ref=${headSha}. No remote changes made.`);
      return;
    }
    await api(`${repository}/statuses/${headSha}`, {
      method: 'POST',
      body: {
        state: 'pending', context: 'Rust release verification',
        description: 'Trusted main Rust workflow requested; human validation still required',
        ...(process.env.GITHUB_RUN_ID ? { target_url: `https://github.com/${repository}/actions/runs/${process.env.GITHUB_RUN_ID}` } : {}),
      },
    });
    await api(`${repository}/actions/workflows/rust.yml/dispatches`, {
      method: 'POST', body: { ref: 'main', inputs: { checkout_ref: headSha } },
    });
    report(`Dispatched trusted main Rust workflow for ${headSha}; it will publish a commit status on that exact SHA. No PR was merged.`);
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
}

main().catch((error) => {
  console.error(error.message);
  process.exitCode = 1;
});
