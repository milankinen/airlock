// Copy the manual pages to Starlight content. Adds the title, edit link and
// last change date as frontmatter, and makes relative .md links site links.
// Runs as an Astro integration: once before the build, and again in the dev
// server when a source page changes.
import { existsSync, readFileSync, writeFileSync, mkdirSync, readdirSync, rmSync, statSync } from 'node:fs';
import { dirname, join, posix, relative } from 'node:path';
import { fileURLToPath } from 'node:url';
import { execFileSync } from 'node:child_process';
import { BASE, EDIT_BASE } from './sidebar.mjs';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const SRC = join(root, 'src');
const OUT = join(root, 'site/content/docs');

// Take the last change date from the source page, not from the copy.
// Without git (for example a source tarball) the pages have no date.
function lastCommitDate(file) {
  try {
    return execFileSync('git', ['log', '-1', '--format=%cI', '--', file], { cwd: SRC, encoding: 'utf8' }).trim();
  } catch {
    return '';
  }
}

// Write only changed files. Each write makes the dev server reload the page.
function writeIfChanged(file, data) {
  mkdirSync(dirname(file), { recursive: true });
  let old;
  try {
    old = readFileSync(file);
  } catch {}
  if (!old?.equals(Buffer.from(data))) writeFileSync(file, data);
}

function walk(dir) {
  if (!existsSync(dir)) return [];
  return readdirSync(dir).flatMap((f) => {
    const p = join(dir, f);
    return statSync(p).isDirectory() ? walk(p) : [p];
  });
}

// Only inline links change. Reference links stay as they are.
function siteLink(fromRel, href) {
  const m = href.match(/^([^#:]*\.md)(#.*)?$/);
  if (!m) return href;
  const target = posix.normalize(posix.join(posix.dirname(fromRel), m[1])).replace(/\.md$/, '');
  // Pages are files, see build.format in the Astro config.
  return (target === 'intro' ? `${BASE}/` : `${BASE}/${target}.html`) + (m[2] ?? '');
}

// Copy all pages, and remove the copies of pages that are not in the source
// any more. The content dir stays in place, because a running dev server
// reads it.
function importPages() {
  const written = new Set();
  const summary = readFileSync(join(SRC, 'SUMMARY.md'), 'utf8');
  const summaryTitles = Object.fromEntries(
    [...summary.matchAll(/\[([^\]]+)\]\(\.\/([^)]+)\)/g)].map((m) => [m[2], m[1]]),
  );

  for (const file of walk(SRC)) {
    const rel = relative(SRC, file).split('\\').join('/');
    if (rel === 'SUMMARY.md') continue;
    // The intro page shows the logo with raw HTML, which the site does not
    // process. So the logo must be a static file.
    if (rel === 'logo.png') {
      writeIfChanged(join(root, 'public/logo.png'), readFileSync(file));
      continue;
    }
    const out = join(OUT, rel === 'intro.md' ? 'index.md' : rel);
    written.add(out);
    if (!rel.endsWith('.md')) {
      writeIfChanged(out, readFileSync(file));
      continue;
    }
    let text = readFileSync(file, 'utf8');
    // Starlight shows the title from frontmatter, so remove the H1 from the page.
    let title = summaryTitles[rel] ?? rel;
    const h1 = text.match(/^# (.+)\n+/m);
    if (h1 && !text.slice(0, h1.index).includes('```')) {
      title = h1[1].trim();
      text = text.slice(0, h1.index) + text.slice(h1.index + h1[0].length);
    }
    text = text.replace(/\]\(([^)\s]+)\)/g, (_, href) => `](${siteLink(rel, href)})`);
    text = text.replace(/src="logo\.png"/g, `src="${BASE}/logo.png"`);
    const meta = [`title: ${JSON.stringify(title)}`, `editUrl: ${EDIT_BASE}${rel}`];
    const date = lastCommitDate(file);
    if (date) meta.push(`lastUpdated: ${date}`);
    writeIfChanged(out, `---\n${meta.join('\n')}\n---\n\n${text.replace(/^\n+/, '')}`);
  }
  for (const file of walk(OUT)) {
    if (!written.has(file)) rmSync(file);
  }
}

export default function manualPages() {
  return {
    name: 'airlock-manual-pages',
    hooks: {
      'astro:config:setup': ({ addWatchFile, logger }) => {
        importPages();
        logger.info(`imported manual pages to ${relative(root, OUT)}`);
        // The sidebar comes from SUMMARY.md, so a change restarts the server.
        addWatchFile(join(SRC, 'SUMMARY.md'));
      },
      'astro:server:setup': ({ server, logger }) => {
        server.watcher.add(SRC);
        server.watcher.on('all', (_, file) => {
          if (!file.startsWith(SRC + '/')) return;
          importPages();
          logger.info(`imported manual pages after change in ${relative(SRC, file)}`);
        });
      },
    },
  };
}
