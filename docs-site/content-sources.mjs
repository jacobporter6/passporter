// Which Markdown files become site pages. Paths are relative to the repo root.
// Everything under docs/ is the handbook, except what CONTENT_EXCLUDE keeps
// out. READMEs beside the code are published under /repo/ so they are
// searchable alongside the handbook.
export const CONTENT_ROOT = 'docs';

export const CONTENT_INCLUDE = [
  'docs/**/*.md',
  'crates/*/README.md',
  'deploy/**/README.md',
];

export const CONTENT_EXCLUDE = [
  // Deployment-specific notes and private-only pages, where a checkout has
  // them: never published.
  'docs/local/**',
  'docs/orchestrator.md',
  '**/node_modules/**',
  '**/target/**',
  '**/tests/fixtures/**',
  '**/CHANGELOG.md',
  '**/LICENSE*.md',
];

// GitHub location used for "Edit page" links and links to source files (code
// is not published on the site). A fork sets its own.
export const REPO_URL = 'https://github.com/jacobporter6/passporter';
export const REPO_BRANCH = 'main';

// Where the built site is served: the origin (`site`) and the path under it
// (`base`, '' or '/' for the root of a custom domain). GitHub Pages serves a
// project site at https://<owner>.github.io/<repo>/. A fork or a custom domain
// changes these two lines, or sets DOCS_SITE / DOCS_BASE for one build.
export const DOCS_SITE = process.env.DOCS_SITE || 'https://jacobporter6.github.io';
export const DOCS_BASE = normalizeBase(process.env.DOCS_BASE ?? '/passporter');

/** `base` as '' (site root) or '/segment' with no trailing slash. */
export function normalizeBase(base) {
  const trimmed = base.trim().replace(/^\/+|\/+$/g, '');
  return trimmed ? `/${trimmed}` : '';
}

/** A root-relative site path (`/app/triage/`) as served under DOCS_BASE. */
export function withBase(sitePath, base = DOCS_BASE) {
  return `${base}${sitePath.startsWith('/') ? sitePath : `/${sitePath}`}`;
}

// Extra privacy patterns for scripts/check-repo.mjs: a file of regular
// expressions, one per line, in an unpublished place. Used when present.
export const PRIVATE_PATTERNS = 'docs/local/private-patterns.txt';
