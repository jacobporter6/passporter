// Tests for the oss block handling: `npm test`.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { blankPrivate, dropPrivateNodes } from './oss-blocks.mjs';

// Markers spelled in pieces, so marker-scanning tools do not take this file
// for a block boundary.
const mark = (kind, edge) => `<!-- oss:${kind}-${edge} -->`;
const BEGIN = mark('private', 'begin');
const END = mark('private', 'end');

test('blankPrivate empties private blocks and keeps line numbers', () => {
  const text = `a\n${BEGIN}\nsecret\n${END}\nb`;
  assert.equal(blankPrivate(text), 'a\n\n\n\nb');
});

test('dropPrivateNodes removes the nodes between the markers', () => {
  const p = (v) => ({ type: 'paragraph', v });
  const parent = { children: [p(1), { type: 'html', value: BEGIN }, p(2), { type: 'html', value: END }, p(3)] };
  assert.equal(dropPrivateNodes(parent), null);
  assert.deepEqual(parent.children.map((n) => n.v), [1, 3]);
});

test('dropPrivateNodes refuses an unclosed block and public blocks', () => {
  assert.match(dropPrivateNodes({ children: [{ type: 'html', value: BEGIN }] }), /never ends/);
  assert.match(dropPrivateNodes({ children: [{ type: 'html', value: `<!-- oss:${'public'}-begin\nx\noss:${'public'}-end -->` }] }), /not supported/);
});
