// Run every generator. Each generator exports `outputs()` returning
// [{ path: 'docs/…/x.md', content }]. Generated pages are committed, so they
// read on GitHub too.
//   node scripts/gen/index.mjs           write outputs
//   node scripts/gen/index.mjs --check   fail if committed outputs are stale
import { readFileSync, writeFileSync, mkdirSync, existsSync } from 'node:fs';
import path from 'node:path';
import { REPO } from './lib.mjs';

const GENERATORS = [
  // Register generators here (relative module paths).
  './http-api.mjs', // docs/architecture/http-api.md from the bridge's router
];

const check = process.argv.includes('--check');
let stale = 0, written = 0;
for (const mod of GENERATORS) {
  const gen = await import(mod);
  for (const { path: rel, content } of await gen.outputs()) {
    const abs = path.join(REPO, rel);
    const old = existsSync(abs) ? readFileSync(abs, 'utf8') : null;
    if (old === content) continue;
    if (check) { console.error(`stale: ${rel}`); stale++; continue; }
    mkdirSync(path.dirname(abs), { recursive: true });
    writeFileSync(abs, content);
    written++;
  }
}
if (stale) { console.error(`${stale} generated file(s) are stale. Run: cd docs-site && npm run gen`); process.exit(1); }
console.log(check ? 'generated docs are up to date' : `wrote ${written} file(s)`);
