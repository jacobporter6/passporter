# docs-site

The delivery layer for the documentation in [`../docs`](../docs/README.md).
Content is portable GitHub-flavoured Markdown; this directory is the only
Starlight-specific code, by design. How pages are written, and how this
directory works, is [Writing the docs](../docs/development/docs.md).

```bash
npm ci
npm run dev        # http://127.0.0.1:4321, live reload on docs/ changes
npm run gen        # regenerate docs/architecture/http-api.md from the bridge's router
npm run check      # tests + generated page up to date + privacy scan + build + link check + diagram render check
npm run build && npm run preview   # the static site as served
npm run shot -- /app/triage/ --light  # screenshot into .shots/
```

Node 22 or newer. The render check and screenshots use a system Chromium
(`CHROMIUM=/path/to/chrome` overrides; `npx playwright-core install chromium`
installs one). The site is not part of `just gate`.

`npm run build` writes a static `dist/` that any web server can host. The
site URL in `content-sources.mjs` (`SITE_URL`) is a placeholder until it has
a home.

| Path | What |
| --- | --- |
| `content-sources.mjs` | Which Markdown files are published (`docs/` and the READMEs beside the code, minus `CONTENT_EXCLUDE`); `REPO_URL`, the one place edit and source links come from (a placeholder: set it to your repository); the site URL; the extra privacy patterns file |
| `sidebar.mjs` | The sidebar groups; fails the build if a published page has no place |
| `src/lib/paths.mjs` | Repository path → site URL, and the published page list |
| `src/lib/portable-loader.ts` | README.md → index page, H1 → title, READMEs beside the code → `/repo/…` |
| `src/lib/remark-portable.mjs` | Link rewriting, GitHub alerts → asides, `mermaid` fences |
| `src/lib/rehype-table-scroll.mjs` | Wide tables scroll inside the column |
| `src/scripts/client.ts` | Mermaid rendering (theme-aware), full-screen diagram viewer, checklists |
| `src/styles/theme.css`, `components.css` | Tokyo Night palette and components |
| `src/overrides/` | Starlight overrides (source chips, banners) |
| `scripts/gen/` | The HTTP API generator and its tests |
| `scripts/check-render.mjs` | Headless Chromium pass over every page for diagram and page errors |
| `scripts/check-dev.mjs` | Cold dev server, every diagram type, stale-module recovery |
| `scripts/check-repo.mjs` | Privacy scan of published pages, generic patterns plus the `PRIVATE_PATTERNS` file; fails on any hit (`DOCS_PRIVACY_STRICT=0` only warns) |
| `src/lib/oss-blocks.mjs` | Drops `oss:private` blocks, so the site shows the open-source view of every page |
| `scripts/shot.mjs` | Screenshots for visual review |
