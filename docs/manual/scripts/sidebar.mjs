// Parse SUMMARY.md into a Starlight sidebar. SUMMARY.md owns the page order.
import { readFileSync } from 'node:fs';

// Path of the manual on GitHub Pages.
export const BASE = '/airlock';

export const REPO = 'https://github.com/milankinen/airlock';

// Each page gets its edit link from the import, because the pages that the
// site builds are copies.
export const EDIT_BASE = `${REPO}/edit/main/docs/manual/src/`;

const SUMMARY = new URL('../src/SUMMARY.md', import.meta.url);

// The intro page is the landing page of the site.
export function slugOf(file) {
  const s = file.replace(/^\.\//, '').replace(/\.md$/, '');
  return s === 'intro' ? 'index' : s;
}

export function sidebarFromSummary() {
  const root = [];
  const stack = [{ depth: -1, items: root }];
  for (const line of readFileSync(SUMMARY, 'utf8').split('\n')) {
    const m = line.match(/^(\s*)- \[([^\]]+)\]\(([^)]+)\)/);
    if (!m) continue;
    const depth = m[1].length / 4;
    const node = { entry: { label: m[2], slug: slugOf(m[3]) }, children: [] };
    while (stack.at(-1).depth >= depth) stack.pop();
    stack.at(-1).items.push(node);
    stack.push({ depth, items: node.children });
  }
  // A Starlight group cannot link to a page, so the parent page becomes the
  // first entry of its group.
  const conv = (n) =>
    n.children.length
      ? {
          label: n.entry.label,
          collapsed: true,
          items: [{ label: 'Overview', slug: n.entry.slug }, ...n.children.map(conv)],
        }
      : n.entry;
  return root.map(conv);
}
