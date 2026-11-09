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
// is not published on the site). Set it to your fork or the project's home.
export const REPO_URL = 'https://github.com/OWNER/passporter';
export const REPO_BRANCH = 'main';

// Where the built site is served. A placeholder until it has a home.
export const SITE_URL = 'https://docs.passporter.example';

// Extra privacy patterns for scripts/check-repo.mjs: a file of regular
// expressions, one per line, in an unpublished place. Used when present.
export const PRIVATE_PATTERNS = 'docs/local/private-patterns.txt';
