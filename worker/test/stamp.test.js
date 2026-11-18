// The landing page's files are kept a year by browsers (public/_headers): a page must refer
// to the current version of each (scripts/stamp.mjs), or visitors keep the old one.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { stale } from '../scripts/stamp.mjs';

test('the pages carry the current stamp of every file they keep', () => {
  const pages = stale().map((s) => s.page);
  assert.deepEqual(pages, [], 'run: node scripts/stamp.mjs');
});
