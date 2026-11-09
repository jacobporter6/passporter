// Tests for the HTTP API generator: `npm test`.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { balanced, bridgeRoutes, chainRoutes, describe, handlerDoc, moduleDoc, plain } from './http-api.mjs';

test('balanced skips parentheses inside strings', () => {
  const src = 'x(a, "b)", f(c))';
  assert.equal(balanced(src, 1), 'a, "b)", f(c)');
});

test('routes before an auth layer need a session; routes after it do not', () => {
  const chain = `
        .route("/agents", get(agents::list))
        .route("/push/subscriptions", get(push::list).post(push::subscribe).delete(push::unsubscribe))
        .route("/audio/{id}", patch(audio::append).layer(DefaultBodyLimit::max(limit)))
        .fallback(api_not_found)
        .layer(require_auth.clone())
        .route("/version", get(version::version))
        .nest_service("/auth", public_auth)
        .layer(timeout(settings.request_timeout))`;
  const { routes, nests } = chainRoutes(chain, { prefix: '/v1' });
  assert.deepEqual(routes.map((r) => `${r.method} ${r.path} ${r.handler} ${r.auth}`), [
    'GET /v1/agents agents::list true',
    'GET /v1/push/subscriptions push::list true',
    'POST /v1/push/subscriptions push::subscribe true',
    'DELETE /v1/push/subscriptions push::unsubscribe true',
    'PATCH /v1/audio/{id} audio::append true',
    'GET /v1/version version::version false',
  ]);
  assert.deepEqual(nests, ['/v1/auth']);
});

test('a route without a method handler is an error, not a silent gap', () => {
  assert.throws(() => chainRoutes('.route("/x", any_service)'), /no method handler/);
});

test('handlerDoc reads the doc comment above a free async fn, skipping attributes', () => {
  const src = [
    'impl X {',
    '    /// A method, not a handler.',
    '    async fn list(&self) {}',
    '}',
    '/// Every agent, as a bare',
    '/// JSON array.',
    '///',
    '/// More detail.',
    '#[allow(clippy::unused_async)]',
    'pub(crate) async fn list(State(state): State<AppState>) {}',
  ].join('\n');
  assert.equal(handlerDoc(src, 'list'), 'Every agent, as a bare\nJSON array.\n\nMore detail.');
  assert.equal(handlerDoc(src, 'missing'), null);
  assert.equal(handlerDoc('async fn bare() {}', 'bare'), '');
});

test('plain joins lines and unwraps intra-doc links', () => {
  const doc = 'Runs a [`Runbook`] (see\n[`crate::runbooks::run`]) and [the hub](crate::registry::Hub).';
  assert.equal(plain(doc), 'Runs a `Runbook` (see `crate::runbooks::run`) and the hub.');
});

test('describe: the handler doc after the route, else the module bullet naming it', () => {
  const route = { method: 'GET', path: '/v1/audio/{id}' };
  assert.equal(describe(route, '`GET /v1/audio/:id`: the upload\'s offset. 404 if unknown.', ''), "The upload's offset.");
  const mod = moduleDoc([
    '//! The dictation endpoints:',
    '//!',
    '//! - `POST /v1/audio` declares a clip.',
    '//! - `GET /v1/audio/:id` reports where an',
    '//!   upload is. Then more.',
  ].join('\n'));
  assert.equal(describe(route, '`GET /v1/audio/:id`.', mod), 'Reports where an upload is.');
  assert.equal(describe({ method: 'GET', path: '/v1/x' }, '', '//! `GET /v1/x[?turn=<n>]` (design §6.4).\n//!\n//! Without `turn`, the latest.'.replace(/\/\/! ?/g, '')), 'Without `turn`, the latest.');
  assert.equal(describe({ method: 'GET', path: '/v1/s' }, '`GET /v1/s` is the status, for resuming.', ''), 'The status, for resuming.');
  assert.equal(describe({ method: 'GET', path: '/v1/y' }, '', mod), 'The dictation endpoints:');
  const shared = '//! Runbooks: named operations (`GET /v1/runbooks`, `POST /v1/runbooks/:name/run`). Never shell.';
  assert.equal(describe({ method: 'POST', path: '/v1/runbooks/{name}/run' }, '`POST /v1/runbooks/:name/run`.', moduleDoc(shared)), 'Runbooks: named operations.');
  assert.equal(describe({ method: 'GET', path: '/v1/z' }, '', ''), '');
});

test('the real router: every route resolves, with the expected access rules', () => {
  const routes = bridgeRoutes();
  const find = (method, path) => routes.find((r) => r.method === method && r.path === path);
  assert.equal(find('POST', '/v1/actions')?.auth, true);
  assert.equal(find('GET', '/v1/agents')?.auth, true);
  assert.equal(find('POST', '/v1/agents')?.auth, true);
  assert.equal(find('PATCH', '/v1/audio/{id}')?.auth, true);
  assert.equal(find('POST', '/v1/attachments')?.auth, true);
  assert.equal(find('GET', '/v1/version')?.auth, false);
  assert.equal(find('POST', '/v1/auth/pair')?.auth, false);
  for (const r of routes) assert.ok(r.doc.length > 0, `${r.method} ${r.path} has no doc summary`);
});
