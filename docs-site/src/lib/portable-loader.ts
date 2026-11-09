/**
 * Portable content loader.
 *
 * The handbook is plain GitHub-flavoured Markdown so it reads on GitHub and can be
 * moved to any other site generator. This loader adapts it to Starlight:
 *
 *  - `README.md` is a directory's index page (GitHub's convention).
 *  - The title comes from frontmatter `title:` or, failing that, the first `# H1`.
 *  - READMEs that live next to the code (crates/, deploy/) are
 *    published under `/repo/<path>/` so the site and GitHub show the same text.
 *
 * Swapping site generators means re-implementing these three rules, nothing more.
 *
 * It also sets each page's "Edit page" link and "Last updated" date. Starlight
 * derives both from a path under src/content/docs, which these pages do not have.
 * Site links in frontmatter (hero actions) are written root-relative, as on a
 * site at `/`; the loader puts the site's base path in front of them.
 */
import { glob } from 'astro/loaders';
import { readFileSync } from 'node:fs';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import type { Loader } from 'astro/loaders';
import { CONTENT_INCLUDE, CONTENT_EXCLUDE, REPO_URL, REPO_BRANCH } from '../../content-sources.mjs';
import { baseFrontmatterLinks, pathToSlug } from './paths.mjs';

export const REPO_ROOT = fileURLToPath(new URL('../../../', import.meta.url));

export function firstH1(markdown: string): string | undefined {
  let inFence = false;
  for (const line of markdown.split(/\r?\n/)) {
    if (/^\s*(```|~~~)/.test(line)) inFence = !inFence;
    if (inFence) continue;
    const m = /^#\s+(.+?)\s*#*\s*$/.exec(line);
    if (m) return m[1].replace(/`/g, '').trim();
  }
  return undefined;
}

/** Newest commit time of every Markdown file in the repository, from one `git log`. */
function commitDates(): Map<string, Date> {
  const dates = new Map<string, Date>();
  const res = spawnSync('git', ['log', '--format=t:%ct', '--name-only', '-z', '--', '*.md'], {
    cwd: REPO_ROOT, encoding: 'utf-8', maxBuffer: 64 * 1024 * 1024,
  });
  if (res.error || res.status !== 0) return dates;
  let t = 0;
  for (const raw of res.stdout.split('\0')) {
    const entry = raw.replace(/^\n+/, '');
    if (!entry) continue;
    if (/^t:\d+$/.test(entry)) { t = Number(entry.slice(2)) * 1000; continue; }
    if (!dates.has(entry)) dates.set(entry, new Date(t)); // log is newest first
  }
  return dates;
}

export function portableLoader(): Loader {
  let dates: Map<string, Date> | undefined;
  const inner = glob({
    base: REPO_ROOT,
    pattern: [...CONTENT_INCLUDE, ...CONTENT_EXCLUDE.map((p) => `!${p}`)],
    generateId: ({ entry, data }) =>
      typeof data.slug === 'string' ? data.slug : pathToSlug(entry),
  });
  return {
    name: 'portable-loader',
    load: (ctx) =>
      inner.load({
        ...ctx,
        parseData: async (props) => {
          const data = baseFrontmatterLinks(structuredClone(props.data ?? {})) as Record<string, unknown>;
          if (props.filePath) {
            const rel = props.filePath.startsWith(REPO_ROOT)
              ? props.filePath.slice(REPO_ROOT.length)
              : props.filePath.replace(/^(\.\.\/)+/, '');
            if (typeof data.title !== 'string') data.title = firstH1(readFileSync(props.filePath, 'utf8')) ?? rel;
            if (data.editUrl === undefined) data.editUrl = `${REPO_URL}/edit/${REPO_BRANCH}/${rel}`;
            dates ??= commitDates();
            const updated = dates.get(rel);
            if (data.lastUpdated === undefined && updated) data.lastUpdated = updated;
          }
          return ctx.parseData({ ...props, data });
        },
      }),
  };
}
