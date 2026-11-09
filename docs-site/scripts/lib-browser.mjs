// Shared headless-browser helpers for QA scripts. Uses the system Chromium
// (set CHROMIUM=/path/to/chrome to override) through playwright-core.
import { chromium } from 'playwright-core';
import { spawn } from 'node:child_process';
import { existsSync } from 'node:fs';
import { createServer } from 'node:http';
import { readFile, stat } from 'node:fs/promises';
import path from 'node:path';
import { DOCS_BASE } from '../content-sources.mjs';

const CANDIDATES = [process.env.CHROMIUM, '/usr/bin/chromium', '/usr/bin/chromium-browser', '/usr/bin/google-chrome', '/opt/google/chrome/chrome'].filter(Boolean);

/** First system browser, else playwright-core's own Chromium (`npx playwright-core install chromium`). */
function browserPath() {
  const system = CANDIDATES.find((p) => existsSync(p));
  if (system) return system;
  try { const bundled = chromium.executablePath(); if (bundled && existsSync(bundled)) return bundled; } catch { /* not installed */ }
  throw new Error('No Chromium found. Install one, set CHROMIUM=/path/to/chrome, or run: npx playwright-core install chromium');
}

export async function launch() {
  return chromium.launch({ executablePath: browserPath(), args: ['--no-sandbox', '--disable-gpu'] });
}

const TYPES = { '.html': 'text/html', '.js': 'text/javascript', '.css': 'text/css', '.svg': 'image/svg+xml', '.json': 'application/json', '.woff2': 'font/woff2', '.png': 'image/png', '.webp': 'image/webp', '.wasm': 'application/wasm', '.pf_meta': 'application/octet-stream' };

/** The file under `root` that URL path `pathname` names, or null if it is malformed or would leave `root`. */
function inside(root, pathname) {
  let decoded;
  try { decoded = decodeURIComponent(pathname); } catch { return null; }
  if (decoded.includes('\0') || decoded.includes('\\')) return null;
  const file = path.resolve(root, `.${path.posix.sep}${decoded}`);
  return file === root || file.startsWith(root + path.sep) ? file : null;
}

/**
 * `pathname` with the site's base path removed (`/passporter/app/` -> `/app/`),
 * or null if it is not under the base. An empty base serves from the root.
 */
export function stripBase(pathname, base) {
  if (!base) return pathname;
  if (pathname === base) return '/';
  return pathname.startsWith(`${base}/`) ? pathname.slice(base.length) : null;
}

/**
 * Minimal static server over dist/ (no dependency on astro preview), serving it
 * under `base` as the published site is (DOCS_BASE by default). Never serves a
 * file outside `dir`. The returned `url` includes the base, so `url + '/app/'`
 * is a page.
 */
export async function serveDist(dir, { port = 0, base = DOCS_BASE } = {}) {
  const root = path.resolve(dir);
  const notFound = async (res) => {
    res.writeHead(404, { 'content-type': 'text/html' });
    res.end(await readFile(path.join(root, '404.html')).catch(() => 'not found'));
  };
  const server = createServer(async (req, res) => {
    const pathname = new URL(req.url, 'http://x').pathname;
    if (base && pathname === base) {
      res.writeHead(301, { location: `${base}/` });
      res.end();
      return;
    }
    const sitePath = stripBase(pathname, base);
    let file = sitePath === null ? null : inside(root, sitePath);
    try {
      if (file === null) throw new Error('outside dist/ or the base path');
      if ((await stat(file)).isDirectory()) file = path.join(file, 'index.html');
      res.writeHead(200, { 'content-type': TYPES[path.extname(file)] ?? 'application/octet-stream' });
      res.end(await readFile(file));
    } catch {
      await notFound(res);
    }
  });
  await new Promise((r) => server.listen(port, '127.0.0.1', r));
  return { server, url: `http://127.0.0.1:${server.address().port}${base}` };
}
