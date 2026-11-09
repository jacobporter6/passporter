// Tests for the base-path link rules: `npm test`.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { normalizeBase, withBase } from '../../content-sources.mjs';
import { baseFrontmatterLinks, baseLink } from './paths.mjs';

test('normalizeBase gives an empty root or one leading slash', () => {
  assert.equal(normalizeBase(''), '');
  assert.equal(normalizeBase('/'), '');
  assert.equal(normalizeBase('passporter'), '/passporter');
  assert.equal(normalizeBase('/passporter/'), '/passporter');
  assert.equal(withBase('/app/', '/passporter'), '/passporter/app/');
  assert.equal(withBase('/', ''), '/');
});

test('baseLink prefixes site links only', () => {
  assert.equal(baseLink('/start/quick-start/', '/passporter'), '/passporter/start/quick-start/');
  assert.equal(baseLink('/', '/passporter'), '/passporter/');
  assert.equal(baseLink('/passporter/security/', '/passporter'), '/passporter/security/');
  assert.equal(baseLink('/passporter-guide/', '/passporter'), '/passporter/passporter-guide/');
  assert.equal(baseLink('https://example.com/x', '/passporter'), 'https://example.com/x');
  assert.equal(baseLink('//example.com/x', '/passporter'), '//example.com/x');
  assert.equal(baseLink('../security.md', '/passporter'), '../security.md');
  assert.equal(baseLink('/security/', ''), '/security/');
});

test('baseFrontmatterLinks rewrites hero actions and nothing else', () => {
  const data = { title: 'Home', hero: { actions: [{ text: 'Go', link: '/start/' }, { text: 'Out', link: 'https://example.com' }] } };
  assert.deepEqual(baseFrontmatterLinks(data, '/p').hero.actions.map((a) => a.link), ['/p/start/', 'https://example.com']);
  assert.deepEqual(baseFrontmatterLinks({ title: 'x' }, '/p'), { title: 'x' });
  assert.equal(baseFrontmatterLinks(undefined, '/p'), undefined);
});
