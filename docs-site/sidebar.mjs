// Sidebar, built from the files on disk.
//
// The groups below follow the reader's path: what Passporter is, getting it
// running, using it, how it works, keeping it safe and running, and working on
// it. Each group lists entries in order:
//   - 'docs/x.md'            one page, labelled by frontmatter `sidebar.label`,
//                            else its title (frontmatter `title:` or first H1);
//   - ['docs/x.md', 'Label'] one page with a label chosen here, so pages that
//                            predate the site need no frontmatter;
//   - { label, items }       a literal Starlight group;
//   - 'docs/dir/'            every page in that directory, by frontmatter
//                            `sidebar.order` (lower first, unordered last), then
//                            by label. Adding a page there needs no change here.
// Frontmatter `sidebar.hidden: true` leaves a page out.
//
// The build fails if a group is empty, if an entry names a missing or
// unpublished file, or if a published page appears nowhere: a new loose page
// in docs/ must be given a place here, so nothing is published unreachable.
//
// Starlight's `autogenerate` cannot be used: it matches pages by their path
// under src/content/docs, and the portable loader's pages live in ../docs and
// beside the code instead.
import { existsSync, readdirSync, readFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import picomatch from 'picomatch';
import { parse as parseYaml } from 'yaml';
import { CONTENT_INCLUDE, CONTENT_EXCLUDE } from './content-sources.mjs';
import { pathToSlug, publishedPages } from './src/lib/paths.mjs';

const repoRoot = fileURLToPath(new URL('../', import.meta.url));
const isPublished = picomatch(CONTENT_INCLUDE, { ignore: CONTENT_EXCLUDE, dot: false });

/** Frontmatter and first H1 of a Markdown file (same rules as the portable loader). */
function readPage(repoPath) {
  const text = readFileSync(path.join(repoRoot, repoPath), 'utf8');
  let fm = {};
  const m = /^---\r?\n([\s\S]*?)\r?\n---\r?\n/.exec(text);
  if (m) {
    try { fm = parseYaml(m[1]) ?? {}; } catch { fm = {}; }
  }
  let h1;
  let inFence = false;
  for (const line of text.slice(m ? m[0].length : 0).split(/\r?\n/)) {
    if (/^\s*(```|~~~)/.test(line)) inFence = !inFence;
    if (inFence) continue;
    const h = /^#\s+(.+?)\s*#*\s*$/.exec(line);
    if (h) { h1 = h[1].replace(/`/g, '').trim(); break; }
  }
  const sb = typeof fm.sidebar === 'object' && fm.sidebar ? fm.sidebar : {};
  const title = typeof fm.title === 'string' ? fm.title : h1 ?? path.basename(repoPath, '.md');
  return {
    label: typeof sb.label === 'string' ? sb.label : title,
    order: typeof sb.order === 'number' ? sb.order : Number.MAX_SAFE_INTEGER,
    hidden: sb.hidden === true,
  };
}

const placed = new Set();
const link = (repoPath, label) => {
  if (!existsSync(path.join(repoRoot, repoPath))) throw new Error(`sidebar.mjs: ${repoPath} does not exist`);
  if (!isPublished(repoPath)) throw new Error(`sidebar.mjs: ${repoPath} is not published (content-sources.mjs)`);
  placed.add(repoPath);
  const slug = pathToSlug(repoPath);
  return { label: label ?? readPage(repoPath).label, link: slug === 'index' ? '/' : `/${slug}/` };
};

const byOrderThenLabel = (a, b) => a.order - b.order || a.label.localeCompare(b.label, 'en');

/** Every published page directly in `dir` (no trailing slash), README first. */
function dirItems(dir) {
  const files = readdirSync(path.join(repoRoot, dir))
    .filter((f) => f.endsWith('.md') && isPublished(`${dir}/${f}`))
    .map((f) => ({ path: `${dir}/${f}`, ...readPage(`${dir}/${f}`) }));
  for (const p of files) if (p.hidden) placed.add(p.path);
  return files
    .filter((p) => !p.hidden)
    .sort((a, b) => (b.path.endsWith('/README.md') - a.path.endsWith('/README.md')) || byOrderThenLabel(a, b))
    .map((p) => link(p.path, p.label));
}

const entry = (e) => {
  if (typeof e === 'object' && !Array.isArray(e)) return [e];
  if (Array.isArray(e)) return [link(e[0], e[1])];
  if (e.endsWith('/')) return dirItems(e.slice(0, -1));
  return [link(e)];
};

const group = (label, entries, { collapsed = false } = {}) => ({ label, collapsed, items: entries.flatMap(entry) });

/** READMEs beside the code (crates/, deploy/), labelled by repository path. */
function besideTheCode() {
  const found = [];
  const walk = (rel) => {
    for (const e of readdirSync(path.join(repoRoot, rel), { withFileTypes: true })) {
      if (e.name.startsWith('.') || e.name === 'node_modules' || e.name === 'target') continue;
      const p = `${rel}/${e.name}`;
      if (e.isDirectory()) walk(p);
      else if (e.name.endsWith('.md') && isPublished(p)) found.push(p);
    }
  };
  for (const top of ['crates', 'deploy']) if (existsSync(path.join(repoRoot, top))) walk(top);
  return found.sort().map((p) => link(p, p.replace(/\/README\.md$/, '/')));
}

export const sidebar = [
  group('Overview', [['docs/README.md', 'What is Passporter?']]),
  group('Getting started', ['docs/start/', ['docs/deploy.md', 'Deploy']]),
  group('Using the app', ['docs/app/']),
  group('Architecture', [
    ['docs/design.md', 'Design'],
    'docs/architecture/',
    ['docs/hooks.md', 'Claude Code hooks'],
    { label: 'Beside the code', collapsed: true, items: besideTheCode() },
  ]),
  group('Security', [['docs/security.md', 'Security controls']]),
  group('Operations', ['docs/operations/']),
  group('Development', [
    'docs/development/contributing.md',
    ['docs/testing.md', 'Testing'],
    'docs/development/e2e.md',
    'docs/development/docs.md',
    ['docs/corpus.md', 'Transcript corpus'],
    ['docs/eval.md', 'Summary eval'],
  ]),
];

// Fail the build rather than ship a sidebar with an empty group.
(function assertNoEmptyGroups(items, trail = []) {
  for (const it of items) {
    if (!('items' in it)) continue;
    const here = [...trail, it.label];
    if (!it.items.length) throw new Error(`sidebar.mjs: group "${here.join(' › ')}" has no pages`);
    assertNoEmptyGroups(it.items, here);
  }
})(sidebar);

// Fail the build rather than publish a page no sidebar entry reaches.
const unplaced = publishedPages().filter((p) => !placed.has(p));
if (unplaced.length) {
  throw new Error(`sidebar.mjs: published but not in the sidebar: ${unplaced.join(', ')}. Add each to a group in docs-site/sidebar.mjs, or give it frontmatter sidebar.hidden: true.`);
}
