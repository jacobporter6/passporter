/**
 * rehype-table-scroll: wrap every Markdown table in <div class="table-scroll">
 * so a wide table scrolls sideways inside the content column instead of
 * running under the table of contents. Plain hast; reusable by any unified
 * pipeline.
 */
import { visit, SKIP } from 'unist-util-visit';

export default function rehypeTableScroll() {
  return (tree) => {
    visit(tree, 'element', (node, index, parent) => {
      if (node.tagName !== 'table' || !parent || index == null) return;
      const cls = parent.properties?.className;
      if (parent.type === 'element' && Array.isArray(cls) && cls.includes('table-scroll')) return SKIP;
      parent.children[index] = {
        type: 'element',
        tagName: 'div',
        properties: { className: ['table-scroll'], tabIndex: 0, role: 'region', ariaLabel: 'Table (scrolls sideways)' },
        children: [node],
      };
      return SKIP;
    });
  };
}
