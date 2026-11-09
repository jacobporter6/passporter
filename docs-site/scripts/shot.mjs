// Screenshot pages for visual review.
//   npm run shot -- /app/triage/ /operations/push/  (dark, 1440 wide)
//   npm run shot -- --light --mobile /              (light theme, phone width)
//   npm run shot -- --full /architecture/overview/  (full page)
// Writes PNGs to docs-site/.shots/ (gitignored) and prints their paths.
import { launch, serveDist } from './lib-browser.mjs';
import { mkdir } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const args = process.argv.slice(2);
const flags = new Set(args.filter((a) => a.startsWith('--')));
const pages = args.filter((a) => !a.startsWith('--'));
if (!pages.length) pages.push('/');
const outDir = path.join(here, '..', '.shots');
await mkdir(outDir, { recursive: true });
const { server, url } = await serveDist(path.join(here, '..', 'dist'));
const browser = await launch();
const mobile = flags.has('--mobile');
const ctx = await browser.newContext({ viewport: mobile ? { width: 390, height: 844 } : { width: 1440, height: 900 }, deviceScaleFactor: mobile ? 2 : 1, colorScheme: flags.has('--light') ? 'light' : 'dark', reducedMotion: 'reduce' });
await ctx.addInitScript((theme) => localStorage.setItem('starlight-theme', theme), flags.has('--light') ? 'light' : 'dark');
for (const p of pages) {
  const page = await ctx.newPage();
  await page.goto(url + p, { waitUntil: 'networkidle' });
  await page.waitForTimeout(800);
  const name = (p.replace(/^\/|\/$/g, '').replace(/[\/#]/g, '_') || 'home') + (flags.has('--light') ? '.light' : '') + (mobile ? '.mobile' : '') + '.png';
  const file = path.join(outDir, name);
  await page.screenshot({ path: file, fullPage: flags.has('--full') });
  console.log(file);
  await page.close();
}
await browser.close();
server.close();
