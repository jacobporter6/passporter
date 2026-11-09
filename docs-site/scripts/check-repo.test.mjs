// Tests for the privacy scan: `npm test`.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { GENERIC, parsePatterns, placeholderRepoUrl, privacyHits } from './check-repo.mjs';

// Private addresses built at run time, so secret scanners that refuse
// literal ones in any file pass this test file.
const ip = (...parts) => parts.join('.');
const LAN = ip(172, 16, 1, 20);
const NET = ip(10, 0, 0, 0);

test('flags home paths, private addresses and internal hostnames', () => {
  const text = [
    'Attached image: @/home/alice/.local/state/x.jpg',
    `listen = "${LAN}:8443" and ${NET}/24`,
    'reverse_proxy to nas.lan',
  ].join('\n');
  assert.deepEqual(privacyHits(text).map((h) => `${h.line} ${h.name} ${h.match}`), [
    '1 home directory /home/alice',
    `2 private IPv4 address ${LAN}`,
    `2 private IPv4 address ${NET}`,
    '3 internal hostname nas.lan',
  ]);
});

test('placeholders, documentation addresses, loopback and versions pass', () => {
  assert.deepEqual(privacyHits('path = "/home/you/git/x" and /home/YOU/y at 127.0.0.1:8178 or 192.0.2.10, version 1.10.2'), []);
});

test('private patterns: one regex per line, comments ignored, (?i) for case-insensitive', () => {
  const extra = parsePatterns('# a personal domain\n(?i)example-family\\.dev\n\n\\bAlice\\b\n');
  assert.equal(extra.length, 2);
  const hits = privacyHits('Open https://app.Example-Family.dev, says Alice (not alice).', [...GENERIC, ...extra]);
  assert.deepEqual(hits.map((h) => h.match), ['Example-Family.dev', 'Alice']);
});

test('the project URL under any owner becomes the placeholder before the scan', () => {
  const url = 'https://github.com/OWNER/passporter';
  assert.equal(placeholderRepoUrl('git clone https://github.com/someone/passporter && cd x', url), 'git clone https://github.com/OWNER/passporter && cd x');
  assert.equal(placeholderRepoUrl('see github.com/someone/other', url), 'see github.com/someone/other');
});
