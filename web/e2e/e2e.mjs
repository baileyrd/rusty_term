// Full-app E2E over the Nebula demo UI: the command palette, history
// search, settings sheet, theme switcher, split panes, session tabs,
// session restore, card groups/filter, transcript export, card quick
// actions, and the Claude assist provider (streaming insights + chat +
// runnable code blocks) against a mocked Anthropic Messages API.
//
// Run from web/: npm test  (builds, then serves dist via vite preview)
// or directly:   npm run build && node e2e/e2e.mjs
//
// Needs a Chromium build Playwright can find (PLAYWRIGHT_BROWSERS_PATH, or
// the default cache from `npx playwright install chromium`).
import { createServer } from 'node:http';
import { spawn } from 'node:child_process';
import { readFile } from 'node:fs/promises';
import { chromium } from 'playwright';

const MOCK_PORT = 7799;
const PREVIEW_PORT = 4173;
let requests = [];

// --- Mock Anthropic Messages API -------------------------------------------
const mock = createServer((req, res) => {
  const cors = {
    'Access-Control-Allow-Origin': '*',
    'Access-Control-Allow-Methods': 'POST, OPTIONS',
    'Access-Control-Allow-Headers': '*',
  };
  if (req.method === 'OPTIONS') {
    res.writeHead(204, cors);
    return res.end();
  }
  if (req.method === 'POST' && req.url.startsWith('/v1/messages')) {
    let body = '';
    req.on('data', (c) => (body += c));
    req.on('end', () => {
      const parsed = JSON.parse(body);
      requests.push({ headers: req.headers, body: parsed });

      const sse = (event, data) => res.write(`event: ${event}\ndata: ${JSON.stringify(data)}\n\n`);
      const streamText = (chunks, delayMs) => {
        res.writeHead(200, { 'Content-Type': 'text/event-stream', 'Cache-Control': 'no-cache', ...cors });
        sse('message_start', {
          type: 'message_start',
          message: {
            id: 'msg_mock', type: 'message', role: 'assistant', model: 'claude-opus-4-8',
            content: [], stop_reason: null, stop_sequence: null,
            usage: { input_tokens: 1, output_tokens: 0 },
          },
        });
        sse('content_block_start', {
          type: 'content_block_start', index: 0, content_block: { type: 'text', text: '' },
        });
        sse('content_block_delta', {
          type: 'content_block_delta', index: 0, delta: { type: 'text_delta', text: chunks[0] },
        });
        setTimeout(() => {
          for (const chunk of chunks.slice(1)) {
            sse('content_block_delta', {
              type: 'content_block_delta', index: 0, delta: { type: 'text_delta', text: chunk },
            });
          }
          sse('content_block_stop', { type: 'content_block_stop', index: 0 });
          sse('message_delta', {
            type: 'message_delta',
            delta: { stop_reason: 'end_turn', stop_sequence: null },
            usage: { output_tokens: 1 },
          });
          sse('message_stop', { type: 'message_stop' });
          res.end();
        }, delayMs);
      };

      // Chat requests carry no output_config — stream a text reply whose
      // fenced code block is cut open mid-stream (no closing fence yet).
      if (!parsed.output_config) {
        streamText(
          [
            'The failing command lacks permissions. Try:\n```sh\nsudo make install',
            '\n```\nThat should fix it.',
          ],
          1500,
        );
        return;
      }

      const fullText = JSON.stringify({
        insights: [
          {
            kind: 'summary',
            title: 'Mock session read',
            body: 'This insight came from the mocked Messages API.',
          },
          {
            kind: 'tip',
            title: 'Try the suggested command',
            body: 'A command suggestion round-trips through the run button.',
            suggestedCommand: 'echo mock-suggestion',
          },
        ],
      });
      // Split so the first chunk completes insight 1 but cuts insight 2
      // mid-string — the incremental parser must surface exactly one card.
      // Split so the first chunk completes insight 1 but cuts insight 2
      // mid-string, and hold the tail so the browser sits in streaming state.
      const splitAt = fullText.indexOf('round-trips');
      streamText([fullText.slice(0, splitAt), fullText.slice(splitAt)], 2000);
    });
    return;
  }
  res.writeHead(404, cors);
  res.end();
});

const assert = (cond, msg) => {
  if (!cond) {
    console.error(`FAIL: ${msg}`);
    process.exitCode = 1;
  } else {
    console.log(`ok: ${msg}`);
  }
};

await new Promise((r) => mock.listen(MOCK_PORT, '127.0.0.1', r));

// Click a palette action row by its title — immune to the race between
// fill() and the filtered list re-rendering.
const clickAction = (page, text) =>
  page
    .locator('[data-testid="palette-item"][data-group="actions"]')
    .filter({ hasText: text })
    .first()
    .click();

const preview = spawn('npx', ['vite', 'preview', '--port', String(PREVIEW_PORT), '--strictPort'], {
  cwd: new URL('..', import.meta.url), // web/, regardless of the caller's cwd
  stdio: 'ignore',
  detached: true, // own process group, so cleanup kills the vite child too
});
await new Promise((r) => setTimeout(r, 2500));

const browser = await chromium.launch();
try {
  const page = await browser.newPage();
  page.on('pageerror', (e) => console.log('PAGEERROR:', e.message));
  await page.addInitScript(
    (url) => sessionStorage.setItem('nebula.assistBaseUrl', url),
    `http://127.0.0.1:${MOCK_PORT}`,
  );
  await page.goto(`http://127.0.0.1:${PREVIEW_PORT}/`);

  // --- Command palette -------------------------------------------------------
  // Let the shell mount (and its window keydown listener register) first.
  await page.locator('[data-testid="command-card"]').first().waitFor();
  await page.waitForTimeout(300);
  await page.keyboard.press('Control+k');
  await page.locator('[data-testid="command-palette"]').waitFor();
  assert(
    await page.locator('[data-testid="palette-input"]').evaluate((el) => el === document.activeElement),
    'palette opens on Ctrl+K with the input focused',
  );
  assert(
    (await page.locator('[data-testid="palette-item"][data-group="snippets"]').count()) === 2,
    'both default snippets are listed',
  );

  // Fuzzy filter: "ctw" is a subsequence of "cargo test --workspace".
  await page.locator('[data-testid="palette-input"]').fill('ctw');
  await page.locator('[data-testid="palette-item"][data-group="snippets"]').waitFor();
  const paletteCardsBefore = await page.locator('[data-testid="command-card"]').count();
  // Other rows can also fuzzy-match "ctw" — pick the snippet row explicitly.
  await page.locator('[data-testid="palette-item"][data-group="snippets"]').first().click();
  await page.locator('[data-testid="command-palette"]').waitFor({ state: 'detached' });
  await page.locator('[data-testid="command-card"]').nth(paletteCardsBefore).waitFor();
  assert(
    (await page.locator('[data-testid="command-card"]').last().textContent()).includes(
      'cargo test --workspace',
    ),
    'fuzzy-matched snippet runs on Enter and the palette closes',
  );

  // Raw entry: whatever you typed is always the first, runnable row.
  await page.keyboard.press('Control+k');
  await page.locator('[data-testid="palette-input"]').fill('echo palette-run');
  assert(
    (await page.locator('[data-testid="palette-item"]').first().getAttribute('data-group')) === 'run',
    'raw run row leads the list for a free-form query',
  );
  await page.keyboard.press('Enter');
  await page.locator('[data-testid="command-card"]').nth(paletteCardsBefore + 1).waitFor();
  assert(
    (await page.locator('[data-testid="command-card"]').last().textContent()).includes(
      'echo palette-run',
    ),
    'raw query runs as a command',
  );

  // Esc closes without running anything.
  await page.keyboard.press('Control+k');
  await page.keyboard.press('Escape');
  await page.locator('[data-testid="command-palette"]').waitFor({ state: 'detached' });
  assert(
    (await page.locator('[data-testid="command-card"]').count()) === paletteCardsBefore + 2,
    'Esc closes the palette without side effects',
  );

  // Open the orb's panel: disconnected label + key form visible.
  await page.locator('[data-testid="ai-orb"], button[aria-label^="AI assistant"]').first().click();
  const panel = page.locator('[data-testid="assist-panel"]');
  await panel.waitFor();
  assert(
    (await page.locator('[data-testid="assist-provider-label"]').textContent()).includes(
      'no AI provider connected',
    ),
    'panel starts disconnected',
  );
  assert(
    (await page.locator('[data-testid="assist-insight"][data-source="local"]').count()) > 0,
    'local heuristics insights render',
  );

  // Connect a key: the first AI insight must render while the response is
  // still streaming (the mock holds the tail chunk back for 2s).
  await page.locator('[data-testid="assist-connect"] input').fill('sk-ant-mock-key');
  await page.locator('[data-testid="assist-connect"] button[type="submit"]').click();
  await page.locator('[data-testid="assist-insight"][data-source="ai"]').first().waitFor({ timeout: 15000 });
  assert(
    (await page.locator('[data-testid="assist-ai-status"][data-phase="streaming"]').count()) === 1,
    'panel is in the streaming phase when the first card lands',
  );
  assert(
    (await page.locator('[data-testid="assist-insight"][data-source="ai"]').count()) === 1,
    'exactly one AI insight rendered mid-stream',
  );
  await page
    .locator('[data-testid="assist-insight"][data-source="ai"]')
    .nth(1)
    .waitFor({ timeout: 15000 });
  await page.locator('[data-testid="assist-ai-status"]').waitFor({ state: 'detached', timeout: 15000 });
  assert(true, 'stream settles: status line gone, final set rendered');
  assert(
    (await page.locator('[data-testid="assist-provider-label"]').textContent()).includes('claude-opus-4-8'),
    'label shows the connected model',
  );
  assert(
    (await page.locator('[data-testid="assist-insight"][data-source="ai"]').count()) === 2,
    'both mocked AI insights render',
  );
  assert(
    (await page
      .locator('[data-testid="assist-insight"][data-source="ai"] code')
      .first()
      .textContent()) === 'echo mock-suggestion',
    'suggested command renders',
  );

  // The request the SDK actually sent.
  assert(requests.length >= 1, 'mock API was called');
  const r = requests[0];
  assert(r.headers['x-api-key'] === 'sk-ant-mock-key', 'API key header sent');
  assert(r.body.model === 'claude-opus-4-8', 'model is claude-opus-4-8');
  assert(r.body.thinking?.type === 'adaptive', 'adaptive thinking requested');
  assert(r.body.stream === true, 'request asked for a streaming response');
  assert(
    r.body.output_config?.format?.type === 'json_schema',
    'json_schema structured output requested',
  );
  assert(
    JSON.stringify(r.body.output_config.format.schema).includes('suggestedCommand'),
    'schema constrains the insight shape',
  );

  // --- Chat mode -----------------------------------------------------------
  await page.locator('[data-testid="assist-tab-chat"]').click();
  await page.locator('[data-testid="assist-chat-input"]').fill('why did it fail?');
  await page.locator('[data-testid="assist-chat-send"]').click();
  assert(
    (await page.locator('[data-testid="assist-chat-message"][data-role="user"]').textContent()) ===
      'why did it fail?',
    'user turn renders in the thread',
  );
  // Partial assistant text must show while the mock holds the tail chunk.
  await page
    .locator('[data-testid="assist-chat-message"][data-role="assistant"]')
    .filter({ hasText: 'lacks permissions' })
    .waitFor({ timeout: 15000 });
  assert(
    await page.locator('[data-testid="assist-chat-input"]').isDisabled(),
    'input is locked while the reply streams',
  );
  // Mid-stream the fence is still open — it must already render as code.
  assert(
    (await page.locator('[data-testid="assist-chat-code"] pre').textContent()) ===
      'sudo make install',
    'unterminated code fence renders as a code block mid-stream',
  );
  const partial = await page
    .locator('[data-testid="assist-chat-message"][data-role="assistant"]')
    .textContent();
  assert(!partial.includes('That should fix it'), 'assistant bubble is mid-stream (tail not yet arrived)');
  await page
    .locator('[data-testid="assist-chat-message"][data-role="assistant"]')
    .filter({ hasText: 'That should fix it.' })
    .waitFor({ timeout: 15000 });
  await page.locator('[data-testid="assist-chat-input"]:not([disabled])').waitFor({ timeout: 15000 });
  assert(true, 'reply settles and the input unlocks');
  assert(
    (await page.locator('[data-testid="assist-chat-code"]').count()) === 1 &&
      (await page.locator('[data-testid="assist-chat-code"] pre').textContent()) ===
        'sudo make install',
    'settled reply has one code block, fence markers stripped',
  );

  // Run the block: the command lands in the terminal path (demo mode
  // appends a card) and the sheet stays open for the conversation.
  const cardsBefore = await page.locator('[data-testid="command-card"]').count();
  await page.locator('[data-testid="assist-chat-run"]').click();
  await page
    .locator('[data-testid="command-card"]')
    .nth(cardsBefore)
    .waitFor({ timeout: 5000 });
  assert(
    (await page.locator('[data-testid="command-card"]').last().textContent()).includes(
      'sudo make install',
    ),
    'run submits the code block as a command',
  );
  assert(
    (await page.locator('[data-testid="assist-panel"]').count()) === 1,
    'sheet stays open after running a chat block',
  );

  // Second turn: the request must carry the whole conversation.
  await page.locator('[data-testid="assist-chat-input"]').fill('and how do I fix it?');
  await page.locator('[data-testid="assist-chat-send"]').click();
  await page
    .locator('[data-testid="assist-chat-message"][data-role="assistant"]')
    .nth(1)
    .filter({ hasText: 'That should fix it.' })
    .waitFor({ timeout: 15000 });
  const chatReqs = requests.filter((q) => !q.body.output_config);
  assert(chatReqs.length === 2, 'two chat requests hit the mock');
  const second = chatReqs[1].body;
  assert(second.messages.length === 3, 'second turn carries user/assistant/user history');
  assert(
    second.messages[1].role === 'assistant' &&
      second.messages[1].content ===
        'The failing command lacks permissions. Try:\n```sh\nsudo make install\n```\nThat should fix it.',
    'prior assistant reply is in the history verbatim (fences intact)',
  );
  assert(
    second.messages[2].content.includes('Session history (newest last):') &&
      second.messages[2].content.includes('and how do I fix it?'),
    'latest user turn carries fresh session context',
  );
  assert(second.thinking?.type === 'adaptive' && second.stream === true, 'chat streams with adaptive thinking');

  // Key survives a reload (sessionStorage), then disconnect clears it.
  await page.reload();
  await page.locator('[data-testid="ai-orb"], button[aria-label^="AI assistant"]').first().click();
  await page.locator('[data-testid="assist-insight"][data-source="ai"]').first().waitFor({ timeout: 15000 });
  assert(true, 'key persisted across reload; AI insights re-fetched');

  await page.locator('[data-testid="assist-disconnect"]').click();
  await page.locator('[data-testid="assist-connect"]').waitFor();
  assert(
    (await page.locator('[data-testid="assist-provider-label"]').textContent()).includes(
      'no AI provider connected',
    ),
    'disconnect reverts to local-only',
  );
  assert(
    (await page.evaluate(() => sessionStorage.getItem('nebula.assistApiKey'))) === null,
    'disconnect clears the stored key',
  );

  // Palette action: retarget the (already open) sheet to the chat tab.
  await page.keyboard.press('Control+k');
  await page.locator('[data-testid="palette-input"]').fill('assist chat');
  await page.locator('[data-testid="palette-item"][data-group="actions"]').first().click();
  await page
    .locator('[data-testid="assist-tab-chat"][aria-selected="true"]')
    .waitFor({ timeout: 5000 });
  assert(
    (await page.locator('[data-testid="assist-panel"]').textContent()).includes(
      'Connect an Anthropic API key',
    ),
    'palette opens assist straight to the chat tab (disconnected hint shown)',
  );
  // Leave the panel closed — later sections click the orb expecting it to
  // open, not toggle an already-open panel shut.
  await page.locator('[data-testid="assist-panel"] button[aria-label="Close assist panel"]').click();
  await page.locator('[data-testid="assist-panel"]').waitFor({ state: 'detached' });

  // --- Theme switcher --------------------------------------------------------
  const accentVar = () =>
    page.evaluate(() =>
      getComputedStyle(document.documentElement).getPropertyValue('--nebula-accent').trim(),
    );
  assert((await accentVar()) === '76 225 247', 'nebula accent is the default');
  await page.keyboard.press('Control+k');
  await page.locator('[data-testid="palette-input"]').fill('theme cyber');
  await clickAction(page, 'Theme: cyberpunk');
  await page.locator('[data-testid="command-palette"]').waitFor({ state: 'detached' });
  assert((await accentVar()) === '255 42 109', 'cyberpunk accent applied via palette');
  assert(
    (await page.evaluate(() => document.documentElement.dataset.theme)) === 'cyberpunk',
    'data-theme stamped on <html>',
  );
  assert(
    (await page.evaluate(() => {
      const term = document.querySelector('.xterm-screen canvas, .xterm');
      return term ? getComputedStyle(document.body).backgroundColor : null;
    })) === 'rgb(11, 0, 20)',
    'body canvas follows the theme',
  );

  // Persists across reload, and switching back works.
  await page.reload();
  await page.locator('[data-testid="command-card"]').first().waitFor();
  assert((await accentVar()) === '255 42 109', 'theme choice survives a reload');
  await page.waitForTimeout(300);
  await page.keyboard.press('Control+k');
  await page.locator('[data-testid="palette-input"]').fill('theme nebula');
  await clickAction(page, 'Theme: nebula');
  assert((await accentVar()) === '76 225 247', 'switching back to nebula works');

  // --- Split panes -----------------------------------------------------------
  assert(
    (await page.locator('[data-testid="terminal-pane"]').count()) === 1,
    'one terminal pane to start',
  );
  const splitOnce = async () => {
    await page.keyboard.press('Control+k');
    await page.locator('[data-testid="command-palette"]').waitFor();
    await page.locator('[data-testid="palette-input"]').fill('split terminal');
    await clickAction(page, 'Split terminal pane');
    await page.locator('[data-testid="command-palette"]').waitFor({ state: 'detached' });
  };
  await splitOnce();
  await page.locator('[data-testid="terminal-pane"]').nth(1).waitFor();
  assert(
    (await page.locator('[data-testid="terminal-pane"]').count()) === 2,
    'palette split adds a second pane',
  );
  // Each pane runs its own session — the loopback banner renders per pane.
  await page.waitForTimeout(400);
  assert(
    (await page.locator('[data-testid="terminal-pane"] .xterm').count()) === 2,
    'both panes host their own xterm instance',
  );
  // Cap: two more splits reach 4; the split action then disappears.
  await splitOnce();
  await splitOnce();
  await page.locator('[data-testid="terminal-pane"]').nth(3).waitFor();
  await page.keyboard.press('Control+k');
  await page.locator('[data-testid="palette-input"]').fill('split terminal');
  assert(
    (await page.locator('[data-testid="palette-item"][data-group="actions"]').count()) === 0,
    'split action is gone at the 4-pane cap',
  );
  await page.keyboard.press('Escape');
  // Per-pane ✕ closes that pane; the primary has no close button.
  await page.locator('[data-testid="pane-close"]').last().click();
  assert(
    (await page.locator('[data-testid="terminal-pane"]').count()) === 3,
    'pane ✕ closes the pane',
  );
  assert(
    (await page.locator('[data-testid="terminal-pane"]').first().locator('[data-testid="pane-close"]').count()) === 0,
    'primary pane has no close button',
  );
  // Palette close trims back to a single pane.
  for (let i = 0; i < 2; i++) {
    await page.keyboard.press('Control+k');
    await page.locator('[data-testid="command-palette"]').waitFor();
    await page.locator('[data-testid="palette-input"]').fill('close terminal pane');
    await clickAction(page, 'Close terminal pane');
    await page.locator('[data-testid="command-palette"]').waitFor({ state: 'detached' });
  }
  assert(
    (await page.locator('[data-testid="terminal-pane"]').count()) === 1 &&
      (await page.locator('[data-testid="pane-close"]').count()) === 0,
    'palette close returns to a single pane',
  );

  // --- Session tabs ----------------------------------------------------------
  assert(
    (await page.locator('[data-testid="session-tab"]').count()) === 1 &&
      (await page.locator('[data-testid="tab-close"]').count()) === 0,
    'one session tab to start, with no close affordance',
  );
  const tab1Cards = await page.locator('[data-testid="command-card"]').count();
  await page.locator('[data-testid="tab-add"]').click();
  await page.locator('[data-testid="session-tab"]').nth(1).waitFor();
  assert(
    (await page.locator('[data-testid="session-tab"][data-active="true"]').textContent()).includes(
      'session 2',
    ),
    'new tab opens and becomes active',
  );
  assert(
    (await page.locator('[data-testid="command-card"]').count()) === 0,
    'new tab starts with an empty card stream',
  );
  await page.waitForTimeout(400);
  assert(
    (await page.locator('.xterm').count()) === 2,
    'both tabs keep their terminals mounted',
  );

  // A command run in tab 2 stays in tab 2.
  await page.locator('main form input').fill('echo tab-two');
  await page.locator('main form input').press('Enter');
  await page.locator('[data-testid="command-card"]').first().waitFor();
  assert(
    (await page.locator('[data-testid="command-card"]').textContent()).includes('echo tab-two'),
    'input line targets the active tab',
  );
  await page.locator('[data-testid="session-tab"]').first().locator('button').first().click();
  assert(
    (await page.locator('[data-testid="command-card"]').count()) === tab1Cards &&
      !(await page.locator('[data-testid="command-card"]').allTextContents()).join().includes('echo tab-two'),
    'switching back shows tab 1 cards only',
  );

  // The palette's "Tab: session 2" row switches sessions.
  await page.keyboard.press('Control+k');
  await page.locator('[data-testid="command-palette"]').waitFor();
  await page.locator('[data-testid="palette-input"]').fill('tab: session');
  await clickAction(page, 'Tab: session');
  await page.locator('[data-testid="command-palette"]').waitFor({ state: 'detached' });
  assert(
    (await page.locator('[data-testid="session-tab"][data-active="true"]').textContent()).includes(
      'session 2',
    ) && (await page.locator('[data-testid="command-card"]').textContent()).includes('echo tab-two'),
    'palette switches to the other tab with its own cards',
  );

  // Close tab 2 from its ✕: back to one tab, tab 1's cards restored.
  await page.locator('[data-testid="session-tab"][data-active="true"] [data-testid="tab-close"]').click();
  assert(
    (await page.locator('[data-testid="session-tab"]').count()) === 1 &&
      (await page.locator('[data-testid="tab-close"]').count()) === 0 &&
      (await page.locator('[data-testid="command-card"]').count()) === tab1Cards,
    'closing the tab returns to session 1',
  );

  // --- Session restore -------------------------------------------------------
  // Build a workspace: a second tab with a card of its own and a split pane.
  await page.locator('[data-testid="tab-add"]').click();
  await page.locator('[data-testid="session-tab"]').nth(1).waitFor();
  await page.locator('main form input').fill('echo restore-me');
  await page.locator('main form input').press('Enter');
  await page.locator('[data-testid="command-card"]').first().waitFor();
  await splitOnce();
  await page.locator('[data-testid="terminal-pane"]:visible').nth(1).waitFor();
  // Let the debounced save land, then reload.
  await page.waitForTimeout(700);
  await page.reload();
  await page.locator('[data-testid="command-card"]').first().waitFor();
  assert(
    (await page.locator('[data-testid="session-tab"]').count()) === 2 &&
      (await page.locator('[data-testid="session-tab"][data-active="true"]').textContent()).includes(
        'session',
      ),
    'reload restores both tabs',
  );
  assert(
    (await page.locator('[data-testid="command-card"]').textContent()).includes('echo restore-me'),
    'the active tab comes back with its cards',
  );
  assert(
    (await page.locator('[data-testid="terminal-pane"]:visible').count()) === 2,
    'the split-pane layout survives the reload',
  );
  // The other tab's history is intact too.
  await page.locator('[data-testid="session-tab"]').first().locator('button').first().click();
  assert(
    (await page.locator('[data-testid="command-card"]').count()) === tab1Cards,
    'the background tab comes back with its own cards',
  );
  // Leave a clean single-tab workspace for whoever runs next.
  await page.locator('[data-testid="session-tab"]').nth(1).locator('[data-testid="tab-close"]').click();

  // --- History search --------------------------------------------------------
  await page.keyboard.press('Control+Shift+F');
  await page.locator('[data-testid="search-overlay"]').waitFor();
  await page.locator('[data-testid="search-input"]').fill('staging');
  await page.locator('[data-testid="search-hit"]').first().waitFor();
  assert(
    (await page.locator('[data-testid="search-hit"]').first().textContent()).includes('deploy.sh'),
    'output search finds the demo deploy card',
  );
  assert(
    (await page.locator('[data-testid="search-hit"] mark').first().textContent()) === 'staging',
    'the matching fragment is highlighted',
  );
  await page.keyboard.press('Enter');
  await page.locator('[data-testid="search-overlay"]').waitFor({ state: 'detached' });
  await page.locator('[data-testid="command-card"][data-highlighted]').waitFor();
  assert(
    (await page.locator('[data-testid="command-card"][data-highlighted]').textContent()).includes(
      'deploy.sh',
    ),
    'Enter jumps to the card and flashes it',
  );

  // Cross-tab: a hit in another session switches to that tab.
  await page.locator('[data-testid="tab-add"]').click();
  await page.locator('main form input').fill('echo search-target-xyz');
  await page.locator('main form input').press('Enter');
  await page.locator('[data-testid="command-card"]').first().waitFor();
  await page.locator('[data-testid="session-tab"]').first().locator('button').first().click();
  await page.keyboard.press('Control+Shift+F');
  await page.locator('[data-testid="search-overlay"]').waitFor();
  await page.locator('[data-testid="search-input"]').fill('search-target');
  await page.locator('[data-testid="search-hit"]').first().waitFor();
  await page.keyboard.press('Enter');
  await page.locator('[data-testid="command-card"][data-highlighted]').waitFor();
  assert(
    (await page.locator('[data-testid="command-card"][data-highlighted]').textContent()).includes(
      'search-target-xyz',
    ) &&
      (await page.locator('[data-testid="session-tab"][data-active="true"]').textContent()).includes(
        'session',
      ),
    'a cross-tab hit switches sessions and lands on the card',
  );
  // Esc closes the overlay.
  await page.keyboard.press('Control+Shift+F');
  await page.locator('[data-testid="search-overlay"]').waitFor();
  await page.keyboard.press('Escape');
  await page.locator('[data-testid="search-overlay"]').waitFor({ state: 'detached' });
  assert(true, 'Esc closes the search overlay');
  await page.locator('[data-testid="session-tab"][data-active="true"] [data-testid="tab-close"]').click();

  // --- Collapsible card groups ----------------------------------------------
  // The demo data spans two bursts (~25 min apart), so two group headers.
  assert(
    (await page.locator('[data-testid="card-group-header"]').count()) === 2,
    'demo history renders as two card groups',
  );
  const cardsExpanded = await page.locator('[data-testid="command-card"]').count();
  await page.locator('[data-testid="card-group-header"]').first().click();
  assert(
    (await page.locator('[data-testid="card-group-header"][data-collapsed="true"]').count()) === 1 &&
      (await page.locator('[data-testid="command-card"]').count()) === cardsExpanded - 1,
    'collapsing the old burst hides its single card',
  );
  await page.locator('[data-testid="card-group-header"]').first().click();
  assert(
    (await page.locator('[data-testid="command-card"]').count()) === cardsExpanded,
    'expanding the group brings its cards back',
  );

  // A search jump into a collapsed group auto-expands it.
  await page.locator('[data-testid="card-group-header"]').first().click();
  await page.keyboard.press('Control+Shift+F');
  await page.locator('[data-testid="search-overlay"]').waitFor();
  await page.locator('[data-testid="search-input"]').fill('insertions');
  await page.locator('[data-testid="search-hit"]').first().waitFor();
  await page.keyboard.press('Enter');
  await page.locator('[data-testid="command-card"][data-highlighted]').waitFor();
  assert(
    (await page.locator('[data-testid="command-card"][data-highlighted]').textContent()).includes(
      'git commit',
    ) &&
      (await page.locator('[data-testid="card-group-header"][data-collapsed="true"]').count()) === 0,
    'a search jump auto-expands the collapsed group',
  );

  // --- Transcript export -----------------------------------------------------
  const exportVia = async (query, rowText) => {
    await page.keyboard.press('Control+k');
    await page.locator('[data-testid="command-palette"]').waitFor();
    await page.locator('[data-testid="palette-input"]').fill(query);
    const downloadPromise = page.waitForEvent('download');
    await clickAction(page, rowText);
    await page.locator('[data-testid="command-palette"]').waitFor({ state: 'detached' });
    return downloadPromise;
  };
  const mdDownload = await exportVia('export transcript', 'Export transcript (markdown)');
  assert(
    /^rusty-term-session-1-\d{4}-\d{2}-\d{2}\.md$/.test(mdDownload.suggestedFilename()),
    'markdown export names the file after the session and date',
  );
  const mdText = await readFile(await mdDownload.path(), 'utf8');
  assert(
    mdText.includes('# rusty_term transcript — session 1') &&
      mdText.includes('### `./deploy.sh --env staging`') &&
      mdText.includes('→ rolling staging fleet… 4/4 healthy') &&
      mdText.includes('```text'),
    'markdown transcript carries commands and fenced output',
  );
  const jsonDownload = await exportVia('export transcript', 'Export transcript (json)');
  const parsedTranscript = JSON.parse(await readFile(await jsonDownload.path(), 'utf8'));
  assert(
    parsedTranscript.title === 'session 1' &&
      Array.isArray(parsedTranscript.commands) &&
      parsedTranscript.commands.some((c) => c.command === 'rm /var/log/rusty_term/session.lock'),
    'json transcript carries the raw cards',
  );

  // --- Card quick actions ----------------------------------------------------
  await page.context().grantPermissions(['clipboard-read', 'clipboard-write']);
  const deployCard = page
    .locator('[data-testid="command-card"]')
    .filter({ hasText: './deploy.sh --env staging' })
    .first();
  await deployCard.hover();
  const cardsBeforeRerun = await page.locator('[data-testid="command-card"]').count();
  await deployCard.locator('[data-testid="card-rerun"]').click();
  await page.locator('[data-testid="command-card"]').nth(cardsBeforeRerun).waitFor();
  assert(
    (await page.locator('[data-testid="command-card"]').last().textContent()).includes(
      './deploy.sh --env staging',
    ),
    'card re-run submits the command again',
  );
  await deployCard.hover();
  await deployCard.locator('[data-testid="card-copy-output"]').click();
  assert(
    (await page.evaluate(() => navigator.clipboard.readText())).includes(
      'rolling staging fleet… 4/4 healthy',
    ),
    'card copy puts the output on the clipboard',
  );

  // --- Failures-only filter --------------------------------------------------
  const allCardCount = await page.locator('[data-testid="command-card"]').count();
  await page.keyboard.press('Control+k');
  await page.locator('[data-testid="command-palette"]').waitFor();
  await page.locator('[data-testid="palette-input"]').fill('failures only');
  await clickAction(page, 'Show failures only');
  await page.locator('[data-testid="failures-filter-chip"]').waitFor();
  const errorCards = await page.locator('[data-testid="command-card"]').count();
  assert(
    errorCards > 0 &&
      errorCards < allCardCount &&
      (await page.locator('[data-testid="command-card"]:not([data-status="error"])').count()) === 0,
    'the filter shows only error cards with a chip',
  );
  await page.locator('[data-testid="failures-filter-chip"] button').click();
  await page.locator('[data-testid="failures-filter-chip"]').waitFor({ state: 'detached' });
  assert(
    (await page.locator('[data-testid="command-card"]').count()) === allCardCount,
    'dismissing the chip restores the full stream',
  );

  // A search jump to a non-failure clears the filter so the target renders.
  await page.keyboard.press('Control+k');
  await page.locator('[data-testid="command-palette"]').waitFor();
  await page.locator('[data-testid="palette-input"]').fill('failures only');
  await clickAction(page, 'Show failures only');
  await page.locator('[data-testid="failures-filter-chip"]').waitFor();
  await page.keyboard.press('Control+Shift+F');
  await page.locator('[data-testid="search-overlay"]').waitFor();
  await page.locator('[data-testid="search-input"]').fill('deployed in 42.7s');
  await page.locator('[data-testid="search-hit"]').first().waitFor();
  await page.keyboard.press('Enter');
  await page.locator('[data-testid="command-card"][data-highlighted]').waitFor();
  assert(
    (await page.locator('[data-testid="failures-filter-chip"]').count()) === 0 &&
      (await page.locator('[data-testid="command-card"][data-highlighted]').textContent()).includes(
        'deploy.sh',
      ),
    'a search jump to a success card clears the filter and lands',
  );

  // --- Settings sheet ---------------------------------------------------------
  await page.keyboard.press('Control+,');
  await page.locator('[data-testid="settings-sheet"]').waitFor();
  assert(
    (await page.locator('[data-testid="settings-theme-option"]').count()) === 3,
    'settings sheet lists all three theme presets',
  );
  assert(
    (await page.locator('[data-testid="settings-theme-option"][data-active="true"]').textContent()).includes(
      'nebula',
    ),
    'the active theme is marked in the settings sheet',
  );
  assert(
    (await page.locator('[data-testid="settings-connect"]').count()) === 1,
    'assist section shows the connect form while disconnected',
  );

  // Switching theme from the sheet updates the live custom property.
  await page.locator('[data-testid="settings-theme-option"]').filter({ hasText: 'minimal' }).click();
  assert((await accentVar()) === '143 168 199', 'theme switch from settings applies immediately');
  await page.locator('[data-testid="settings-theme-option"]').filter({ hasText: 'nebula' }).click();
  assert((await accentVar()) === '76 225 247', 'switched back to nebula from settings');

  // Connecting from the settings sheet is the same state the orb's panel
  // reads — no separate "settings API key" to fall out of sync.
  await page.locator('[data-testid="settings-connect"] input').fill('sk-ant-settings-key');
  await page.locator('[data-testid="settings-connect"] button[type="submit"]').click();
  await page.locator('[data-testid="settings-disconnect"]').waitFor();
  await page.keyboard.press('Escape');
  await page.locator('[data-testid="settings-sheet"]').waitFor({ state: 'detached' });
  await page.locator('[data-testid="ai-orb"], button[aria-label^="AI assistant"]').first().click();
  // The panel mounts showing 'disconnected' for one render — aiState only
  // flips once TerminalShell's effect (keyed on apiKey/assistOpen) runs
  // after commit — so wait for the label's *text*, not just its presence.
  await page
    .locator('[data-testid="assist-provider-label"]')
    .filter({ hasText: 'claude-opus-4-8' })
    .waitFor({ timeout: 5000 });
  assert(
    (await page.locator('[data-testid="assist-provider-label"]').textContent()).includes(
      'claude-opus-4-8',
    ),
    'a key connected via settings shows up as connected in the assist panel',
  );
  await page.locator('[data-testid="assist-panel"] button[aria-label="Close assist panel"]').click();

  // Disconnect from the settings sheet reverts the assist panel too.
  await page.keyboard.press('Control+,');
  await page.locator('[data-testid="settings-sheet"]').waitFor();
  await page.locator('[data-testid="settings-disconnect"]').click();
  await page.locator('[data-testid="settings-connect"]').waitFor();

  // Clear pinned snippets.
  const snippetsBefore = await page.locator('[data-testid="settings-sheet"]').textContent();
  assert(snippetsBefore.includes('2 pinned'), 'settings shows the current pinned-snippet count');
  await page.locator('[data-testid="settings-clear-snippets"]').click();
  assert(
    (await page.locator('[data-testid="settings-sheet"]').textContent()).includes('0 pinned') &&
      (await page.locator('[data-testid="settings-clear-snippets"]').isDisabled()),
    'clearing snippets zeroes the count and disables the button',
  );

  // Esc closes; a backdrop click also closes it.
  await page.keyboard.press('Escape');
  await page.locator('[data-testid="settings-sheet"]').waitFor({ state: 'detached' });
  await page.keyboard.press('Control+,');
  await page.locator('[data-testid="settings-sheet"]').waitFor();
  await page.mouse.click(10, 10);
  await page.locator('[data-testid="settings-sheet"]').waitFor({ state: 'detached' });
  assert(true, 'Esc and backdrop click both close the settings sheet');

  // Reachable from the palette too.
  await page.keyboard.press('Control+k');
  await page.locator('[data-testid="command-palette"]').waitFor();
  await page.locator('[data-testid="palette-input"]').fill('open settings');
  await clickAction(page, 'Open settings');
  await page.locator('[data-testid="settings-sheet"]').waitFor();
  assert(true, 'palette action opens the settings sheet');
  await page.keyboard.press('Escape');
} finally {
  await browser.close();
  try { process.kill(-preview.pid); } catch { preview.kill(); }
  mock.close();
}
console.log(process.exitCode ? 'E2E FAILED' : 'E2E PASSED');
