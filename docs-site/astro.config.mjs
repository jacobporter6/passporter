// @ts-check
// Delivery layer for the handbook. Content lives in ../docs (portable Markdown);
// everything Starlight-specific is in this directory. See docs/development/docs.md.
import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';
import starlightLinksValidator from 'starlight-links-validator';
import { fileURLToPath } from 'node:url';
import remarkPortable from './src/lib/remark-portable.mjs';
import rehypeTableScroll from './src/lib/rehype-table-scroll.mjs';
import { REPO_URL, REPO_BRANCH, DOCS_SITE, DOCS_BASE } from './content-sources.mjs';
import { sidebar } from './sidebar.mjs';

const repoRoot = fileURLToPath(new URL('../', import.meta.url));

export default defineConfig({
  // Served at DOCS_SITE + DOCS_BASE (GitHub Pages by default); both are set in
  // content-sources.mjs. Internal links, assets and scripts all carry the base.
  site: DOCS_SITE,
  base: DOCS_BASE || '/',
  markdown: { remarkPlugins: [[remarkPortable, { repoRoot }]], rehypePlugins: [rehypeTableScroll] },
  vite: {
    // DOCS_VITE_CACHE_DIR lets scripts/check-dev.mjs start a cold dev server
    // without disturbing a running one (default: node_modules/.vite).
    cacheDir: process.env.DOCS_VITE_CACHE_DIR || undefined,
    server: { fs: { allow: [repoRoot] } },
    // Pre-bundle Mermaid when the dev server starts. Otherwise Vite discovers
    // it on the first page with a diagram and re-optimises mid-session, which
    // changes every pre-bundled chunk URL; a tab that already loaded Mermaid
    // then fails to fetch its next diagram module.
    optimizeDeps: { include: ['mermaid'] },
  },
  integrations: [
    starlight({
      title: 'Passporter',
      description: 'A mobile-first, voice-first control surface for coding agents running in herdr: triage, quick replies, dictation and TL;DRs from your phone.',
      logo: { src: './src/assets/logo.svg', replacesTitle: false },
      favicon: '/favicon.svg',
      social: [{ icon: 'github', label: 'Repository', href: REPO_URL }],
      // Each page's edit link is set by the portable loader (its files are not in
      // src/content/docs); this base only applies to pages it does not cover.
      editLink: { baseUrl: `${REPO_URL}/edit/${REPO_BRANCH}/` },
      lastUpdated: true,
      pagination: true,
      tableOfContents: { minHeadingLevel: 2, maxHeadingLevel: 3 },
      customCss: [
        '@fontsource-variable/inter',
        '@fontsource-variable/jetbrains-mono',
        './src/styles/theme.css',
        './src/styles/components.css',
      ],
      expressiveCode: {
        themes: ['tokyo-night', 'github-light'],
        styleOverrides: { borderRadius: '0.6rem', codeFontFamily: "'JetBrains Mono Variable', ui-monospace, monospace" },
      },
      components: {
        Head: './src/overrides/Head.astro',
        PageTitle: './src/overrides/PageTitle.astro',
      },
      sidebar,
      plugins: [starlightLinksValidator({
        errorOnLocalLinks: false,
        errorOnInvalidHashes: true,
        errorOnFallbackPages: false,
      })],
    }),
  ],
});
