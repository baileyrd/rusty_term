// Live-bridge E2E for the tab-activity-badge feature: only real OSC 133
// events (from an actual shell) can finish a command in a *background*
// tab, so the demo-mode suite (e2e.mjs) can't cover this — this test runs
// the real rusty_term_web_bridge binary with a bash whose rcfile emits the
// A/B/C/D marks, types a slow command into tab 1's live shell, opens tab 2
// while it runs, and asserts the background tab gets a badge.
//
// Prerequisite: build the bridge first, from the repo root (one level up):
//   cargo build --features web-bridge
//
// Run from web/: node e2e/live-bridge.mjs   (builds the frontend if needed)
import { existsSync } from 'node:fs';
import { mkdtemp, writeFile, chmod, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { spawn, spawnSync } from 'node:child_process';
import { chromium } from 'playwright';

const REPO_ROOT = new URL('../..', import.meta.url);
const WEB_ROOT = new URL('..', import.meta.url);
const BRIDGE_BIN = join(
  new URL(REPO_ROOT).pathname,
  'target',
  'debug',
  process.platform === 'win32' ? 'rusty_term_web_bridge.exe' : 'rusty_term_web_bridge',
);
const BRIDGE_PORT = 7704;
const PREVIEW_PORT = 4181;

const assert = (cond, msg) => {
  if (!cond) {
    console.error(`FAIL: ${msg}`);
    process.exitCode = 1;
  } else {
    console.log(`ok: ${msg}`);
  }
};

if (!existsSync(BRIDGE_BIN)) {
  console.error(
    `live-bridge E2E: ${BRIDGE_BIN} does not exist.\n` +
      'Build it first from the repo root: cargo build --features web-bridge',
  );
  process.exit(1);
}

// A bash whose rcfile emits the OSC 133 shell-integration marks the bridge's
// command-card tracking relies on (documented in web/README.md).
const workdir = await mkdtemp(join(tmpdir(), 'rusty-term-live-bridge-'));
const rcPath = join(workdir, 'osc-rc.sh');
const shellPath = join(workdir, 'osc-bash');
await writeFile(
  rcPath,
  [
    "PS0='\\033]133;C\\007'",
    'PROMPT_COMMAND=\'printf "\\033]133;D;%s\\007\\033]133;A\\007" "$?"\'',
    "PS1='\\$ \\[\\033]133;B\\007\\]'",
    '',
  ].join('\n'),
);
await writeFile(shellPath, `#!/bin/bash\nexec bash --rcfile ${rcPath} -i\n`);
await chmod(shellPath, 0o755);

if (!existsSync(join(new URL(WEB_ROOT).pathname, 'dist', 'index.html'))) {
  spawnSync('npx', ['vite', 'build'], { cwd: new URL(WEB_ROOT).pathname, stdio: 'inherit' });
}

const bridge = spawn(BRIDGE_BIN, ['--listen', `127.0.0.1:${BRIDGE_PORT}`, '--shell', shellPath], {
  stdio: 'ignore',
  detached: true,
});
const preview = spawn(
  'npx',
  ['vite', 'preview', '--port', String(PREVIEW_PORT), '--strictPort'],
  { cwd: new URL(WEB_ROOT).pathname, stdio: 'ignore', detached: true },
);
await new Promise((r) => setTimeout(r, 2500));

const browser = await chromium.launch();
try {
  const page = await browser.newPage();
  page.on('pageerror', (e) => console.log('PAGEERROR:', e.message));
  await page.goto(`http://127.0.0.1:${PREVIEW_PORT}/?ws=ws://127.0.0.1:${BRIDGE_PORT}`);
  await page.locator('[data-testid="ribbon-env"]').filter({ hasText: 'live' }).waitFor({ timeout: 15000 });

  // Type a slow command into tab 1's live shell, then immediately switch to
  // a fresh tab. The command finishes in the background.
  const term = page.locator('[data-testid="terminal-pane"] .xterm').first();
  await term.click();
  await page.waitForTimeout(800); // let the shell print its first prompt
  await page.keyboard.type('sleep 2; echo bg-done');
  await page.keyboard.press('Enter');
  await page.locator('[data-testid="command-card"][data-status="running"]').waitFor({ timeout: 10000 });
  await page.locator('[data-testid="tab-add"]').click();
  await page.locator('[data-testid="session-tab"]').nth(1).waitFor();
  assert(
    (await page.locator('[data-testid="tab-badge"]').count()) === 0,
    'no badge while the background command still runs',
  );

  // ~2s later the sleep ends and the D mark lands in the hidden tab.
  await page
    .locator('[data-testid="session-tab"]')
    .first()
    .locator('[data-testid="tab-badge"]')
    .waitFor({ timeout: 15000 });
  assert(
    (await page.locator('[data-testid="tab-badge"]').textContent()) === '1',
    'background completion raises a badge of 1 on the inactive tab',
  );
  assert(
    (await page
      .locator('[data-testid="session-tab"][data-active="true"] [data-testid="tab-badge"]')
      .count()) === 0,
    'the active tab never shows a badge',
  );

  // Activating the tab clears the badge and shows the finished card.
  await page.locator('[data-testid="session-tab"]').first().locator('button').first().click();
  await page.locator('[data-testid="tab-badge"]').waitFor({ state: 'detached', timeout: 5000 });
  assert(
    (await page
      .locator('[data-testid="command-card"][data-status="success"]')
      .filter({ hasText: 'sleep 2; echo bg-done' })
      .count()) === 1,
    'switching back clears the badge and shows the finished card',
  );
} finally {
  await browser.close();
  try {
    process.kill(-preview.pid);
  } catch {
    preview.kill();
  }
  try {
    process.kill(-bridge.pid);
  } catch {
    bridge.kill();
  }
  await rm(workdir, { recursive: true, force: true });
}
console.log(process.exitCode ? 'LIVE-BRIDGE E2E FAILED' : 'LIVE-BRIDGE E2E PASSED');
