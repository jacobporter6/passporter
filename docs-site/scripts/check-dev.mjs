// Dev-server render check: starts `astro dev` with an empty Vite dependency
// cache, then browses pages covering every Mermaid diagram type the docs use
// in ONE tab, the way a reader navigates. Fails if any diagram reports
// an error (for example "Failed to fetch dynamically imported module", which
// happens when Vite re-optimises dependencies mid-session).
//
//   node scripts/check-dev.mjs [extra /paths/ ...]
import { execFileSync } from 'node:child_process';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { launch } from './lib-browser.mjs';

const PORT = Number(process.env.PORT ?? 4471);
const ROOT = new URL('..', import.meta.url).pathname;
const astro = (...args) => execFileSync('npx', ['astro', ...args], { cwd: ROOT, env: { ...process.env, DOCS_VITE_CACHE_DIR: cache }, encoding: 'utf8' });
const PAGES = [
  '/', '/architecture/overview/', '/app/terminal-mode/', '/app/dictation/', '/operations/push/',
  '/development/docs/', '/architecture/http-api/',
  ...process.argv.slice(2),
];

const cache = mkdtempSync(path.join(tmpdir(), 'docs-vite-'));
// Astro 7 runs one dev server per project, in the background. This refuses if
// one is already running here (use a separate worktree to test beside it).
let log = astro('dev', '--background', '--host', '127.0.0.1', '--port', String(PORT));
const m = /http:\/\/127\.0\.0\.1:(\d+)/.exec(log);
if (!m || /already running/i.test(log)) { console.error(log); process.exit(2); }

const base = `http://127.0.0.1:${m[1]}`;
const failures = [];
try {
  let up = false;
  for (let i = 0; i < 240 && !up; i++) {
    up = await fetch(base + '/').then((r) => r.ok, () => false);
    if (!up) await new Promise((r) => setTimeout(r, 500));
  }
  if (!up) throw new Error(`dev server did not start:\n${log.slice(-2000)}`);
  const browser = await launch();
  const page = await browser.newPage();
  page.on('pageerror', (e) => failures.push(`${page.url()}: pageerror ${e.message}`));
  for (const p of PAGES) {
    await page.goto(base + p, { waitUntil: 'load', timeout: 120000 });
    // Wait until every mermaid block has rendered or errored.
    await page.waitForFunction(() => [...document.querySelectorAll('pre.mermaid')].every((el) => el.dataset.processed || el.dataset.error), null, { timeout: 120000 }).catch(() => failures.push(`${p}: diagrams did not finish`));
    const errs = await page.evaluate(() => [...document.querySelectorAll('[data-error]')].map((el) => el.dataset.error || el.textContent.slice(0, 160)));
    for (const e of errs) failures.push(`${p}: ${e}`);
    const n = await page.evaluate(() => document.querySelectorAll('pre.mermaid').length);
    console.log(`${p}  ${n} diagram(s)${errs.length ? `  ${errs.length} error(s)` : ''}`);
  }
  // Stale-module recovery: fail the sequence-diagram module once, as a
  // re-optimised dev server (or a replaced release) would, and expect the page
  // to reload itself and render every diagram.
  const page2 = await browser.newPage();
  let failed = 0;
  await page2.route(/sequenceDiagram-.*\.js/, (route) => (failed++ ? route.continue() : route.fulfill({ status: 404, body: 'gone' })));
  await page2.goto(base + '/app/dictation/', { waitUntil: 'load', timeout: 120000 });
  await page2.waitForFunction(() => [...document.querySelectorAll('pre.mermaid')].every((el) => el.dataset.processed && !el.dataset.error), null, { timeout: 120000 })
    .then(() => console.log(`stale-module recovery: ok (module failed ${failed ? 'once' : 'never'}, page reloaded and rendered)`),
      async () => failures.push(`stale-module recovery failed: ${await page2.evaluate(() => [...document.querySelectorAll('[data-error]')].map((e) => e.dataset.error).join('; '))}`));
  if (!failed) failures.push('stale-module recovery: the sequence-diagram module was never requested');
  await browser.close();
} finally {
  try { log += astro('dev', 'logs'); } catch { /* logs unavailable */ }
  try { astro('dev', 'stop'); } catch { /* already stopped */ }
  rmSync(cache, { recursive: true, force: true });
}
const reopt = (log.match(/optimized dependencies changed|new dependencies optimized/gi) ?? []).length;
if (reopt) console.log(`vite re-optimised dependencies ${reopt} time(s) during the session`);
if (failures.length) { console.error(failures.join('\n')); process.exit(1); }
console.log('dev render check passed');
