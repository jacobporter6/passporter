// HTTP API reference: every route the bridge serves, read from its axum router
// (crates/bridge/src/server/mod.rs, plus the pairing routes nested under
// /v1/auth), with whether it needs a session and the first paragraph of the
// handler's doc comment. Writes docs/architecture/http-api.md.
//
// The parse is deliberately narrow: it understands the router's own shape
// (`let x = Router::new().route(...)...;` blocks, `.layer(require_auth…)`,
// `.nest_service`) and fails loudly on anything it cannot resolve, so a
// refactor of the router breaks `npm run gen:check` instead of silently
// dropping routes.
import { existsSync } from 'node:fs';
import { cell, code, page, readText, repoPath, table } from './lib.mjs';

const SERVER = 'crates/bridge/src/server/mod.rs';
const AUTH_ROUTES = 'crates/bridge/src/auth/routes.rs';
const METHODS = ['get', 'post', 'put', 'patch', 'delete'];

// Routes whose handler and module docs do not describe them one by one (the
// pairing routes are thin wrappers over `DeviceAuth` methods). Any other
// undescribed route fails the generator: document it in the Rust source, not
// here.
const UNDOCUMENTED = {
  'POST /v1/auth/pair': 'Registers a device: its P-256 public key, with the one-time code `passporter pair` shows (single use, 5 minutes).',
  'POST /v1/auth/challenge': 'A single-use challenge for a paired device to sign.',
  'POST /v1/auth/session': 'Exchanges a signed challenge for a 15-minute session token.',
};

/** The text inside the parentheses that open at `open` (balanced, string-aware). */
export function balanced(src, open) {
  let depth = 0;
  let inStr = false;
  for (let i = open; i < src.length; i++) {
    const c = src[i];
    if (inStr) {
      if (c === '\\') i++;
      else if (c === '"') inStr = false;
      continue;
    }
    if (c === '"') inStr = true;
    else if (c === '(') depth++;
    else if (c === ')' && --depth === 0) return src.slice(open + 1, i);
  }
  throw new Error(`unbalanced parentheses at offset ${open}`);
}

/** The body of `fn name(...) ... { ... }` in Rust source, braces balanced. */
export function fnBody(src, name) {
  const m = new RegExp(`\\bfn ${name}\\b[^{]*\\{`).exec(src);
  if (!m) throw new Error(`fn ${name} not found`);
  let depth = 0;
  for (let i = m.index + m[0].length - 1; i < src.length; i++) {
    if (src[i] === '{') depth++;
    else if (src[i] === '}' && --depth === 0) return src.slice(m.index + m[0].length, i);
  }
  throw new Error(`fn ${name} has unbalanced braces`);
}

/**
 * Routes declared in a chain of builder calls. A route is authenticated when
 * an `.layer(require_auth…)` follows it in the same chain (layers wrap the
 * routes added before them), unless `requireAuth` is false for the whole chain.
 * Returns [{ method, path, handler, auth }].
 */
export function chainRoutes(chain, { prefix = '', authLayer = /^require_auth/ } = {}) {
  const calls = [];
  const re = /\.(route|layer|nest_service)\(/g;
  for (let m; (m = re.exec(chain)); ) {
    const args = balanced(chain, m.index + m[0].length - 1);
    calls.push({ kind: m[1], args });
    re.lastIndex = m.index + m[0].length + args.length;
  }
  const routes = [];
  const nests = [];
  for (const [i, c] of calls.entries()) {
    if (c.kind === 'nest_service') {
      const n = /^\s*"([^"]+)"/.exec(c.args);
      if (n) nests.push(prefix + n[1]);
      continue;
    }
    if (c.kind !== 'route') continue;
    const p = /^\s*"([^"]+)"\s*,([\s\S]*)$/.exec(c.args);
    if (!p) throw new Error(`cannot read route: ${c.args.slice(0, 80)}`);
    const auth = calls.slice(i + 1).some((later) => later.kind === 'layer' && authLayer.test(later.args.trim()));
    const handlers = [...p[2].matchAll(new RegExp(`(?:^|[.\\s])(${METHODS.join('|')})\\(([\\w:]+)`, 'g'))];
    if (!handlers.length) throw new Error(`route ${p[1]} has no method handler`);
    for (const [, method, handler] of handlers) routes.push({ method: method.toUpperCase(), path: prefix + p[1], handler, auth });
  }
  return { routes, nests };
}

/** The `///` doc comment directly above the free `async fn name` in Rust source ('' if none), or null if there is no such fn. */
export function handlerDoc(src, name) {
  const lines = src.split('\n');
  const at = lines.findIndex((l) => new RegExp(`^(?:pub(?:\\([^)]*\\))? )?async fn ${name}\\b`).test(l));
  if (at < 0) return null;
  const doc = [];
  for (let i = at - 1; i >= 0; i--) {
    const l = lines[i].trim();
    if (l.startsWith('#[')) continue;
    if (!l.startsWith('///')) break;
    doc.unshift(l.replace(/^\/\/\/ ?/, ''));
  }
  return doc.join('\n');
}

/** Rustdoc text as one line of Markdown, intra-doc links unwrapped. */
export function plain(doc) {
  return doc
    .replace(/\s*\n\s*/g, ' ')
    .trim()
    .replace(/\[(`[^`]+`)\]\([^)]*\)/g, '$1')
    .replace(/\[([^\]]+)\]\((?:crate|self|super|protocol)::[^)]*\)/g, '$1')
    .replace(/\[(`[^`]+`)\]/g, '$1');
}

/** The first sentence of a line of text (a `.` followed by a capital, a code span or the end). */
export function firstSentence(text) {
  const m = /^(.+?[.!?])(?=\s+[A-Z0-9`(]|$)/.exec(text);
  return (m ? m[1] : text).trim();
}

const capitalise = (s) => s.replace(/^[a-z]/, (c) => c.toUpperCase());

/** A route's description once the route is cut from its sentence: no leading design reference or bare verb. */
const tidy = (rest) => rest.replace(/\s*\(see `crate::[^`]*`[^()]*\)/g, '').replace(/^\(design §[^)]*\)\s*[:.]?\s*/, '').replace(/^(?:is|are|returns)\s+/, '');

/**
 * What a route does, from Rust docs. Handlers document themselves in one of
 * two ways: a sentence in the handler's own `///` comment (often after the
 * route, as in "`GET /v1/x`: the …"), or a bullet or opening paragraph naming
 * the route in the module's `//!` comment. Failing both, the module's own
 * first sentence (a module serving a few related routes), with parenthesised
 * route lists dropped. Returns the first sentence found, without the route
 * itself, or ''.
 */
export function describe({ method, path }, handlerDocText, moduleDocText) {
  const route = `${method} ${path.replace(/\{(\w+)\}/g, ':$1')}`;
  // `GET /v1/x`, `GET /v1/x[?turn=<n>]`, `GET /v1/x?since=`: the route, then an optional query.
  const mention = new RegExp('^\\s*(?:-\\s+)?`' + route.replace(/[.*+?^${}()|[\]\\]/g, '\\$&') + '(?:\\[?\\?[^`]*)?`\\s*(?:[:.,]\\s*)?');
  const blocks = (doc) => doc.split(/\n\s*\n|\n(?=\s*- )/).map(plain).filter(Boolean);
  for (const block of blocks(handlerDocText)) {
    const rest = tidy(block.replace(mention, ''));
    if (rest) return capitalise(firstSentence(rest));
  }
  const mod = blocks(moduleDocText);
  for (const [i, block] of mod.entries()) {
    if (!mention.test(block)) continue;
    // A heading-like paragraph (just the route and a reference) introduces the next one.
    const rest = tidy(block.replace(mention, '')) || mod[i + 1] || '';
    if (rest) return capitalise(firstSentence(rest));
  }
  const [lead = ''] = blocks(moduleDocText);
  return capitalise(firstSentence(lead.replace(/\s*\([^()]*`(?:GET|POST|PUT|PATCH|DELETE|WS) \/v1[^()]*\)/g, '')));
}

/** The `//!` module comment of Rust source. */
export function moduleDoc(src) {
  return src.split('\n').filter((l) => l.startsWith('//!')).map((l) => l.replace(/^\/\/! ?/, '')).join('\n');
}

/** Source file of a handler path as written in `file` (`m::f`, `crate::m::f`, or `f`). */
export function handlerFile(file, handler) {
  const parts = handler.split('::');
  const name = parts.pop();
  const dir = file.replace(/\/[^/]+$/, '');
  let mod;
  if (!parts.length) return { file, name };
  if (parts[0] === 'crate') mod = `crates/bridge/src/${parts.slice(1).join('/')}`;
  else mod = `${dir}/${parts.join('/')}`;
  for (const candidate of [`${mod}.rs`, `${mod}/mod.rs`]) if (existsSync(repoPath(candidate))) return { file: candidate, name };
  throw new Error(`${handler}: no module file for ${mod}`);
}

/** Every route the bridge serves, sorted by path then method. */
export function bridgeRoutes() {
  const body = fnBody(readText(SERVER), 'router');
  const routes = [];
  const nests = [];
  for (const block of body.matchAll(/let \w+ = Router::new\(\)([\s\S]*?);\n/g)) {
    const r = chainRoutes(block[1], { prefix: '/v1' });
    routes.push(...r.routes);
    nests.push(...r.nests);
  }
  if (!nests.includes('/v1/auth')) throw new Error(`${SERVER}: expected the pairing routes nested at /v1/auth`);
  const auth = chainRoutes(fnBody(readText(AUTH_ROUTES), 'router'), { prefix: '/v1/auth' });
  routes.push(...auth.routes.map((r) => ({ ...r, auth: false, from: AUTH_ROUTES })));
  return routes
    .map((r) => {
      const where = handlerFile(r.from ?? SERVER, r.handler);
      const src = readText(where.file);
      const doc = handlerDoc(src, where.name);
      if (doc == null) throw new Error(`${r.method} ${r.path}: no free \`async fn ${where.name}\` in ${where.file}`);
      const text = UNDOCUMENTED[`${r.method} ${r.path}`] ?? describe(r, doc, moduleDoc(src));
      if (!text) throw new Error(`${r.method} ${r.path}: neither \`${where.name}\` nor its module doc in ${where.file} says what it does`);
      return { ...r, file: where.file, doc: text };
    })
    .sort((a, b) => a.path.localeCompare(b.path, 'en') || METHODS.indexOf(a.method.toLowerCase()) - METHODS.indexOf(b.method.toLowerCase()));
}

const display = (p) => p.replace(/\{(\w+)\}/g, ':$1');

export function outputs() {
  const routes = bridgeRoutes();
  const rows = routes.map((r) => {
    let access = r.auth ? 'session' : '**public**';
    if (r.path === '/v1/stream') access = 'session (first message)';
    return [r.method, code(display(r.path)), access, cell(r.doc)];
  });
  const files = [SERVER, AUTH_ROUTES];
  const body = `
Every route \`passporter-bridge\` serves under \`/v1\`, read from its router.
Anything else is the PWA's static files (with the single-page fallback).
Request and response bodies are \`crates/protocol\` types; the
[bridge README](../../crates/bridge/README.md#api) has status codes and the
details behind each row, and [design §5](../design.md#5-bridge-api-sketch)
the rules every action follows (idempotency, stale guard, fixed verbs, audit
first).

**Access**: *session* routes need \`Authorization: Bearer <token>\`: a
15-minute session token from device pairing, or the static token where the
config opts in. The event stream cannot carry headers, so its first message
is the token. *Public* routes are the pairing handshake and the build
version.

${table(['Method', 'Path', 'Access', 'What it does'], rows)}

${routes.length} routes. Each description is the first sentence that the handler's doc comment, or its
module's, gives for the route.
`;
  return [{
    path: 'docs/architecture/http-api.md',
    content: page({
      title: 'HTTP API',
      description: 'Every route the bridge serves, with its access rule and what it does. Generated from the router.',
      order: 7,
      sources: files,
      body,
    }),
  }];
}
