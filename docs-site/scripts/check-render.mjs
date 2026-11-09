// Load every built page in headless Chromium and fail on:
//   - Mermaid diagrams that did not render (syntax errors)
//   - uncaught page errors
// Usage: npm run check:render [-- /only/this/page/ ...]
import { launch, serveDist } from './lib-browser.mjs';
import { readdir } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const dist = path.join(path.dirname(fileURLToPath(import.meta.url)), '..', 'dist');
async function walk(dir, base = '') {
  const out = [];
  for (const e of await readdir(dir, { withFileTypes: true })) {
    if (e.isDirectory() && !e.name.startsWith('_') && e.name !== 'pagefind') out.push(...(await walk(path.join(dir, e.name), `${base}/${e.name}`)));
    else if (e.name === 'index.html') out.push(`${base}/`);
  }
  return out;
}
const only = process.argv.slice(2);
const pages = only.length ? only : await walk(dist);
const { server, url } = await serveDist(dist);
const browser = await launch();
const ctx = await browser.newContext({ reducedMotion: 'reduce' });
const problems = [];
let diagrams = 0;
const queue = [...pages];
await Promise.all(Array.from({ length: 6 }, async () => {
  const page = await ctx.newPage();
  const errors = [];
  page.on('pageerror', (e) => errors.push(String(e.message)));
  for (let p; (p = queue.shift()); ) {
    errors.length = 0;
    await page.goto(url + p, { waitUntil: 'load' });
    const hasMermaid = await page.locator('pre.mermaid').count();
    if (hasMermaid) await page.waitForFunction(() => [...document.querySelectorAll('pre.mermaid')].every((e) => e.hasAttribute('data-processed')), null, { timeout: 20000 }).catch(() => problems.push(`${p}: mermaid timed out`));
    const res = await page.evaluate(() => ({
      mermaid: [...document.querySelectorAll('pre.mermaid[data-error]')].map((e) => e.getAttribute('data-error')),
      n: document.querySelectorAll('pre.mermaid').length,
    }));
    diagrams += res.n;
    for (const m of res.mermaid) problems.push(`${p}: mermaid: ${m?.split('\n').slice(0, 3).join(' ')}`);
    for (const m of errors) problems.push(`${p}: page error: ${m}`);
  }
}));
await browser.close();
server.close();
console.log(`checked ${pages.length} pages, ${diagrams} diagrams`);
if (problems.length) { console.error(problems.join('\n')); process.exit(1); }
