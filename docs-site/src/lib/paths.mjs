// Shared path <-> URL rules, used by the loader, the link rewriter, the
// sidebar and the checks.
import { readdirSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import picomatch from 'picomatch';
import { CONTENT_ROOT, CONTENT_INCLUDE, CONTENT_EXCLUDE } from '../../content-sources.mjs';

const repoRoot = fileURLToPath(new URL('../../../', import.meta.url));
const isPublished = picomatch(CONTENT_INCLUDE, { ignore: CONTENT_EXCLUDE, dot: false });

const slugSegment = (s) =>
  s.toLowerCase().replace(/[^a-z0-9._-]+/g, '-').replace(/\./g, '-').replace(/^-+|-+$/g, '');

/** Repo-relative markdown path -> site slug ('' ... 'index' for the home page). */
export function pathToSlug(repoPath) {
  let p = repoPath.replace(/\\/g, '/').replace(/\.mdx?$/i, '');
  let prefix = '';
  if (p === CONTENT_ROOT + '/README' || p === CONTENT_ROOT + '/index') return 'index';
  if (p.startsWith(CONTENT_ROOT + '/')) p = p.slice(CONTENT_ROOT.length + 1);
  else prefix = 'repo/';
  p = p.replace(/(^|\/)(README|index)$/i, '');
  const slug = prefix + p.split('/').filter(Boolean).map(slugSegment).join('/');
  return slug.replace(/\/$/, '') || 'repo';
}

/** Every published Markdown file in the repository (repo-relative paths, sorted). */
export function publishedPages() {
  const out = [];
  const walk = (rel) => {
    for (const e of readdirSync(path.join(repoRoot, rel || '.'), { withFileTypes: true })) {
      if (e.name.startsWith('.') || e.name === 'node_modules' || e.name === 'target') continue;
      const p = rel ? `${rel}/${e.name}` : e.name;
      if (e.isDirectory()) walk(p);
      else if (e.name.endsWith('.md') && isPublished(p)) out.push(p);
    }
  };
  walk('');
  return out.sort();
}
