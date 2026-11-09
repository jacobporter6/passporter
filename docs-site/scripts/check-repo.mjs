// Repository checks for the published handbook (part of `npm run gen:check`).
//
// Privacy: published pages must not carry deployment specifics: home
// directories, private addresses, internal hostnames, personal names and
// domains. Those belong in an unpublished place that CONTENT_EXCLUDE keeps
// out of the site. This scans every published page, as the site shows it
// (private blocks dropped, see src/lib/oss-blocks.mjs), and fails on each
// hit, printed as `file:line: kind (match)`.
//
// The generic patterns live here. Patterns that would themselves be private
// (a personal domain, a name) go in the file PRIVATE_PATTERNS names
// (content-sources.mjs): one regular expression per line, `#` comments, and a
// leading `(?i)` for case-insensitive, as in Python.
//
// DOCS_PRIVACY_STRICT=0 turns failures into warnings, for a local preview of
// a page in progress.
import { existsSync, readFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { PRIVATE_PATTERNS, REPO_URL } from '../content-sources.mjs';
import { publishedPages } from '../src/lib/paths.mjs';
import { blankPrivate } from '../src/lib/oss-blocks.mjs';

const repoRoot = fileURLToPath(new URL('../../', import.meta.url));

export const GENERIC = [
  { name: 'home directory', re: /\/home\/(?!you\b|YOU\b|user\b)[a-z][\w-]*/g },
  { name: 'private IPv4 address', re: /\b(?:10|192\.168|172\.(?:1[6-9]|2\d|3[01]))\.\d{1,3}\.\d{1,3}(?:\.\d{1,3})?\b/g },
  { name: 'internal hostname', re: /\b[\w-]+\.(?:home\.arpa|lan)\b/g },
];

/** Extra patterns, one regular expression per line (`#` comments, blank lines ignored, `(?i)` prefix). */
export function parsePatterns(text) {
  return text
    .split('\n')
    .map((l) => (/^\s*#/.test(l) ? '' : l.trim()))
    .filter(Boolean)
    .map((src) => {
      const insensitive = src.startsWith('(?i)');
      return { name: 'private pattern', re: new RegExp(insensitive ? src.slice(4) : src, insensitive ? 'gi' : 'g') };
    });
}

/**
 * `text` with every GitHub URL of this project (any owner, the repository
 * name taken from REPO_URL) shown as `github.com/OWNER/<name>`: a fork's
 * clone URL is not a deployment specific.
 */
export function placeholderRepoUrl(text, repoUrl = REPO_URL) {
  const name = /^https:\/\/github\.com\/[^/]+\/([^/]+)$/.exec(repoUrl)?.[1];
  if (!name) return text;
  const escaped = name.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  return text.replace(new RegExp(`github\\.com/[A-Za-z0-9-]+/${escaped}\\b`, 'g'), `github.com/OWNER/${name}`);
}

/** Every private-looking string in `text`, as { line, name, match }. */
export function privacyHits(text, patterns = GENERIC) {
  const hits = [];
  text.split('\n').forEach((line, i) => {
    for (const { name, re } of patterns) for (const m of line.matchAll(re)) hits.push({ line: i + 1, name, match: m[0] });
  });
  return hits;
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const strict = process.env.DOCS_PRIVACY_STRICT !== '0';
  const extra = PRIVATE_PATTERNS && path.join(repoRoot, PRIVATE_PATTERNS);
  const patterns = [...GENERIC, ...(extra && existsSync(extra) ? parsePatterns(readFileSync(extra, 'utf8')) : [])];
  const pages = publishedPages();
  let total = 0;
  for (const rel of pages) {
    const hits = privacyHits(placeholderRepoUrl(blankPrivate(readFileSync(path.join(repoRoot, rel), 'utf8'))), patterns);
    total += hits.length;
    for (const h of hits) console.warn(`check:repo: ${rel}:${h.line}: ${h.name} (${h.match})`);
  }
  if (total && strict) {
    console.error(`check:repo: ${total} private detail(s) in published pages. Move them out of the published content.`);
    process.exit(1);
  }
  console.log(`repository checks passed (${pages.length} published pages, ${patterns.length} patterns${total ? `; ${total} privacy warning(s), see above` : ', 0 hits'})`);
}
