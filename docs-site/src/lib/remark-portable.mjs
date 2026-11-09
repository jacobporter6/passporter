/**
 * remark-portable: render portable GitHub-flavoured Markdown on the site.
 *
 *  1. Drops the leading `# H1` (the site renders the title from it already).
 *  2. Rewrites relative links:
 *       - to another published .md file  -> its site URL
 *       - to any other file in the repo  -> the file on GitHub
 *  3. Turns GitHub alerts (`> [!NOTE]`, `[!TIP]`, `[!IMPORTANT]`, `[!WARNING]`,
 *     `[!CAUTION]`) into Starlight asides.
 *  4. Turns ```mermaid fences into <figure class="diagram"><pre class="mermaid">
 *     for the client renderer.
 *
 * A link to a repository file that is not in Git (tracked, or untracked and not
 * ignored) fails the build, except on `pending:` pages.
 *
 * Before anything else it drops `oss:private` blocks (src/lib/oss-blocks.mjs):
 * the published site shows only the public text of every page.
 *
 * Keep this file framework-neutral: it is plain remark/mdast and reusable by any
 * unified-based generator.
 */
import { visit, SKIP } from 'unist-util-visit';
import { existsSync, statSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import path from 'node:path';
import picomatch from 'picomatch';
import { CONTENT_INCLUDE, CONTENT_EXCLUDE, REPO_URL, REPO_BRANCH, withBase } from '../../content-sources.mjs';
import { baseFrontmatterLinks, pathToSlug } from './paths.mjs';
import { dropPrivateNodes } from './oss-blocks.mjs';

const isPublished = picomatch(CONTENT_INCLUDE, { ignore: CONTENT_EXCLUDE, dot: false });

// Files Git knows about (tracked, or untracked and not ignored) and their
// directories. A link must point at one of these, so a link that only works on
// the author's machine (an ignored .local/ file, a deleted file) fails the
// build. Falls back to the filesystem when git is unavailable.
let gitPaths;
function inGit(repoRoot, target) {
  if (gitPaths === undefined) {
    try {
      const out = execFileSync('git', ['-C', repoRoot, 'ls-files', '--cached', '--others', '--exclude-standard'], { encoding: 'utf8', maxBuffer: 64 << 20, stdio: ['ignore', 'pipe', 'ignore'] });
      gitPaths = new Set();
      for (const f of out.split('\n')) {
        if (!f) continue;
        gitPaths.add(f);
        for (let d = path.posix.dirname(f); d !== '.'; d = path.posix.dirname(d)) {
          if (gitPaths.has(d)) break;
          gitPaths.add(d);
        }
      }
    } catch { gitPaths = null; }
  }
  const t = target.replace(/\/+$/, '');
  if (!gitPaths) return existsSync(path.join(repoRoot, t));
  return t === '' || gitPaths.has(t);
}

const ALERTS = {
  NOTE: ['note', 'Note'],
  TIP: ['tip', 'Tip'],
  IMPORTANT: ['note', 'Important'],
  WARNING: ['caution', 'Warning'],
  CAUTION: ['danger', 'Caution'],
};

const escapeHtml = (s) =>
  s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;');

/** Exposed for tests and for the render checker. */
export function resolveLink(url, fromFile, repoRoot, { pending = false } = {}) {
  if (!url || /^[a-z][a-z0-9+.-]*:/i.test(url) || url.startsWith('#') || url.startsWith('//')) return null;
  const [rawPath, hash = ''] = url.split('#');
  if (!rawPath) return null;
  const fromRel = path.relative(repoRoot, path.dirname(fromFile));
  const target = rawPath.startsWith('/')
    ? rawPath.slice(1)
    : path.posix.normalize(path.posix.join(fromRel.split(path.sep).join('/'), decodeURI(rawPath)));
  if (target.startsWith('..')) return null;
  const abs = path.join(repoRoot, target);
  const exists = existsSync(abs) && inGit(repoRoot, target);
  const isDir = exists && statSync(abs).isDirectory();
  const frag = hash ? `#${hash}` : '';
  // A page marked `pending: "PR #NN"` documents code that is not on main yet, so
  // its links to not-yet-merged pages point at GitHub until that PR lands; then
  // the target exists and the link becomes an ordinary internal link by itself.
  if (pending && !exists && /\.mdx?$/i.test(target)) {
    return { href: `${REPO_URL}/blob/${REPO_BRANCH}/${encodeURI(target)}${frag}`, kind: 'source', target, exists };
  }
  if (/\.mdx?$/i.test(target) && isPublished(target)) {
    const slug = pathToSlug(target);
    return { href: withBase(slug === 'index' ? '/' : `/${slug}/`) + frag, kind: 'page', target, exists };
  }
  if (isDir && isPublished(`${target}/README.md`) && existsSync(path.join(abs, 'README.md'))) {
    return { href: withBase(`/${pathToSlug(`${target}/README.md`)}/`) + frag, kind: 'page', target, exists };
  }
  const blob = isDir ? 'tree' : 'blob';
  return { href: `${REPO_URL}/${blob}/${REPO_BRANCH}/${encodeURI(target)}${frag}`, kind: 'source', target, exists };
}

export default function remarkPortable({ repoRoot }) {
  return (tree, file) => {
    const filePath = file.history?.[0] ?? file.path;
    const opts = { pending: Boolean(file.data?.astro?.frontmatter?.pending) };

    // Tell slug-aware tools (starlight-links-validator) where this page lives;
    // they otherwise assume content sits in src/content/docs.
    if (filePath && file.data?.astro?.frontmatter && !file.data.astro.frontmatter.slug) {
      const slug = pathToSlug(path.relative(repoRoot, filePath).split(path.sep).join('/'));
      // The validator keys pages by posix.join(base, slug + '/') minus a leading
      // slash; '//' is the only value that yields the base itself, the key links
      // to the home page (`../README.md` -> `<base>/`) are looked up under. Only the validator reads
      // this: the page's real id comes from the loader.
      file.data.astro.frontmatter.slug = slug === 'index' ? '//' : slug;
    }
    // The link validator reads hero links from this frontmatter; the page itself
    // renders them from the loader's data, which carries the base the same way.
    if (file.data?.astro?.frontmatter) baseFrontmatterLinks(file.data.astro.frontmatter);

    // 0. private blocks, at any level
    const blockErrors = [];
    visit(tree, (node) => {
      if (!Array.isArray(node.children)) return;
      const err = dropPrivateNodes(node);
      if (err) blockErrors.push(err);
    });
    if (blockErrors.length && filePath) file.fail(`${path.relative(repoRoot, filePath)}: ${blockErrors[0]}`);

    // 1. leading H1
    const first = tree.children.findIndex((n) => n.type !== 'yaml' && n.type !== 'toml');
    if (first >= 0 && tree.children[first].type === 'heading' && tree.children[first].depth === 1) {
      tree.children.splice(first, 1);
    }

    const missing = [];
    const checkMissing = !opts.pending && Boolean(filePath);
    visit(tree, (node, index, parent) => {
      // 2. links and images
      if ((node.type === 'link' || node.type === 'definition' || node.type === 'image') && filePath) {
        if (node.type === 'image') return;
        const r = resolveLink(node.url, filePath, repoRoot, opts);
        if (r) {
          node.url = r.href;
          if (r.kind === 'source') {
            if (!r.exists && checkMissing) missing.push(r.target);
            node.data ??= {};
            node.data.hProperties = { ...(node.data.hProperties ?? {}), className: ['source-link'], 'data-missing': r.exists ? undefined : 'true' };
          }
        }
        return;
      }

      // 2b. href/src attributes inside raw HTML (card grids, figures)
      if (node.type === 'html' && filePath && /\shref="/.test(node.value)) {
        node.value = node.value.replace(/(\shref=")([^"]+)(")/g, (all, a, url, b) => {
          const r = resolveLink(url, filePath, repoRoot, opts);
          if (r?.kind === 'source' && !r.exists && checkMissing) missing.push(r.target);
          return r ? `${a}${r.href}${b}` : all;
        });
        return;
      }

      // 3. GitHub alerts
      if (node.type === 'blockquote') {
        const p = node.children[0];
        const t = p?.type === 'paragraph' ? p.children[0] : null;
        const m = t?.type === 'text' ? /^\[!(NOTE|TIP|IMPORTANT|WARNING|CAUTION)\]\s*/.exec(t.value) : null;
        if (!m) return;
        const [variant, label] = ALERTS[m[1]];
        t.value = t.value.slice(m[0].length);
        if (!t.value && p.children[1]?.type === 'break') p.children.splice(0, 2);
        else if (!t.value) p.children.shift();
        if (!p.children.length) node.children.shift();
        node.data = {
          hName: 'aside',
          hProperties: { 'aria-label': label, className: ['starlight-aside', `starlight-aside--${variant}`] },
        };
        node.children = [
          { type: 'paragraph', data: { hName: 'p', hProperties: { className: ['starlight-aside__title'], 'aria-hidden': 'true' } }, children: [{ type: 'text', value: label }] },
          { type: 'blockquote', data: { hName: 'div', hProperties: { className: ['starlight-aside__content'] } }, children: node.children },
        ];
        return;
      }

      // 4. diagram fences
      if (node.type === 'code' && parent && index != null) {
        if (node.lang === 'mermaid') {
          parent.children[index] = {
            type: 'html',
            value: `<figure class="diagram" data-kind="mermaid"><pre class="mermaid">${escapeHtml(node.value)}</pre></figure>`,
          };
          return SKIP;
        }
      }
    });
    if (missing.length) {
      const rel = path.relative(repoRoot, filePath);
      file.fail(`${rel}: link(s) to files not in Git: ${[...new Set(missing)].join(', ')}. Fix the path, or mark the page pending: if the file arrives with an open PR.`);
    }
  };
}
