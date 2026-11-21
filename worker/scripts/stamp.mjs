// Version stamps on the landing page's own files: each reference in the pages to the
// stylesheet, the script, the font's stylesheet or an image gets `?v=` and the start of a
// hash of that file. Browsers keep those files a year (see public/_headers), so a changed
// file must change its address: a new stamp. The pages themselves are asked for anew each
// visit, so they bring the new stamps.
//
//   node scripts/stamp.mjs           writes the stamps into the pages
//   node scripts/stamp.mjs --check   only says whether they are current (exit 1 if not)

import { createHash } from 'node:crypto';
import { readFileSync, readdirSync, writeFileSync, statSync } from 'node:fs';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const PUBLIC = join(dirname(fileURLToPath(import.meta.url)), '..', 'public');

// what a page refers to that is kept a year: the path as written in the page
// (and the site's icon: asked for anew each visit like the pages, but browsers keep the
// icon of a page they know for a long time by its address; a new stamp is a new address)
const KEPT = /\/(?:style\.css|main\.js|favicon\.svg|fonts\/pretendard\.css|img\/[\w.-]+\.(?:webp|png|jpg|svg))(?:\?v=[0-9a-f]*)?/g;

/** The pages: every .html under public/. */
function pages(dir = PUBLIC) {
  return readdirSync(dir).flatMap((n) => {
    const p = join(dir, n);
    return statSync(p).isDirectory() ? pages(p) : n.endsWith('.html') ? [p] : [];
  });
}

/** The stamp of a file in public/ (text with its line ends as LF, so a checkout's CRLF
 * does not change it). */
export function stampOf(path) {
  let bytes = readFileSync(join(PUBLIC, path));
  if (/\.(css|js)$/.test(path)) bytes = Buffer.from(bytes.toString('utf8').replace(/\r\n/g, '\n'));
  return createHash('sha256').update(bytes).digest('hex').slice(0, 10);
}

/** Each page whose stamps are not current, with the text it should have. */
export function stale() {
  const out = [];
  for (const page of pages()) {
    const text = readFileSync(page, 'utf8');
    const fresh = text.replace(KEPT, (ref) => {
      const path = ref.split('?')[0];
      return `${path}?v=${stampOf(path)}`;
    });
    if (fresh !== text) out.push({ page, fresh });
  }
  return out;
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const check = process.argv.includes('--check');
  const todo = stale();
  for (const { page, fresh } of todo) {
    if (check) console.error(`stale stamps: ${page} (run: node scripts/stamp.mjs)`);
    else writeFileSync(page, fresh);
  }
  if (!check) console.log(todo.length ? `stamped ${todo.length} page(s)` : 'stamps current');
  if (check && todo.length) process.exit(1);
}
