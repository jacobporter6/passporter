// The site publishes the open-source view of every page. Pages mark text
// that only belongs in the private repository with an `oss:private` block:
// a begin and an end marker, each an HTML comment on a line of its own
// (`<!-- oss:private-` + `begin -->`, and the same with `end`).
//
// The site drops those blocks, so it shows the same text whether or not a
// checkout carries private material. `oss:public` blocks are not supported
// on published pages.
//
// The markers are spelled in pieces here so that tools scanning files for
// markers do not take this source for a block boundary.

const marker = (kind, edge) => new RegExp(`^\\s*<!--\\s*oss:${kind}-${edge}\\s*-->\\s*$`);
export const PRIVATE_BEGIN = marker('private', 'begin');
export const PRIVATE_END = marker('private', 'end');
export const PUBLIC_BLOCK = new RegExp(`oss:public-(?:${'begin'}|${'end'})`);

/** `text` with its private blocks blanked out, line numbers kept (for the privacy scan). */
export function blankPrivate(text) {
  let inside = false;
  return text
    .split('\n')
    .map((line) => {
      if (!inside && PRIVATE_BEGIN.test(line)) { inside = true; return ''; }
      if (inside) { if (PRIVATE_END.test(line)) inside = false; return ''; }
      return line;
    })
    .join('\n');
}

/**
 * Removes private blocks from an mdast `parent`'s children in place: every
 * node from an `html` begin marker to its end marker. Returns an error
 * message for an unclosed block or a public block, else null.
 */
export function dropPrivateNodes(parent) {
  const kept = [];
  let inside = false;
  for (const node of parent.children) {
    const html = node.type === 'html' ? node.value : null;
    if (html && PUBLIC_BLOCK.test(html)) return 'oss:public blocks are not supported on published pages';
    if (!inside && html && PRIVATE_BEGIN.test(html)) { inside = true; continue; }
    if (inside) { if (html && PRIVATE_END.test(html)) inside = false; continue; }
    kept.push(node);
  }
  if (inside) return 'an oss:private block begins but never ends';
  parent.children = kept;
  return null;
}
