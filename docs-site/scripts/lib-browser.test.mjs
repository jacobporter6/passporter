import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, rm, writeFile } from 'node:fs/promises';
import { request } from 'node:http';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { serveDist } from './lib-browser.mjs';

/** GET `rawPath` exactly as written (no client-side URL normalisation). */
function get(url, rawPath) {
  const { hostname, port } = new URL(url);
  return new Promise((resolve, reject) => {
    const req = request({ hostname, port, path: rawPath, method: 'GET' }, (res) => {
      let body = '';
      res.setEncoding('utf8');
      res.on('data', (chunk) => { body += chunk; });
      res.on('end', () => resolve({ status: res.statusCode, body }));
    });
    req.on('error', reject);
    req.end();
  });
}

test('serveDist serves dist/ and nothing outside it', async (t) => {
  const root = await mkdtemp(path.join(tmpdir(), 'serve-dist-'));
  const dist = path.join(root, 'dist');
  await mkdir(path.join(dist, 'guide'), { recursive: true });
  await writeFile(path.join(dist, 'index.html'), 'home');
  await writeFile(path.join(dist, 'guide', 'index.html'), 'guide');
  await writeFile(path.join(dist, '404.html'), 'missing');
  await writeFile(path.join(root, 'secret.txt'), 'SECRET');
  await writeFile(path.join(root, 'dist-evil.txt'), 'SIBLING');
  const { server, url } = await serveDist(dist);
  t.after(async () => {
    server.close();
    await rm(root, { recursive: true, force: true });
  });

  assert.deepEqual(await get(url, '/'), { status: 200, body: 'home' });
  assert.deepEqual(await get(url, '/guide/'), { status: 200, body: 'guide' });
  for (const escape of [
    '/..%2fsecret.txt',
    '/guide/..%2f..%2fsecret.txt',
    '/%2e%2e%2fsecret.txt',
    '/..%5csecret.txt',
    '/..%2fdist-evil.txt',
    '/%2fetc%2fpasswd',
  ]) {
    const res = await get(url, escape);
    assert.equal(res.status, 404, escape);
    assert.equal(res.body, 'missing', escape);
  }
  // A malformed escape is a 404 too, not a crash.
  assert.equal((await get(url, '/%E0%A4%A')).status, 404);
  assert.deepEqual(await get(url, '/'), { status: 200, body: 'home' }, 'still serving');
});
