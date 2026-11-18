// The landing page's own few lines: the download buttons, and the language menu.

// ── the download buttons point at the latest release's files (else the release page, as
//    written in the page). Asked of GitHub once per visit; nothing else is loaded. ──
const release = fetch('https://api.github.com/repos/aape133/gachi/releases/latest', { headers: { Accept: 'application/vnd.github+json' } })
  .then((r) => (r.ok ? r.json() : null))
  .catch(() => null); // offline, or GitHub busy: the buttons keep the release page

async function showRelease() {
  const rel = await release;
  if (!rel) return;
  const file = (re) => (rel.assets || []).find((a) => re.test(a.name))?.browser_download_url;
  const app = file(/_x64-setup\.exe$/), vst = file(/_vst3\.zip$/);
  const set = (id, href) => { const el = document.getElementById(id); if (el && href) el.href = href; };
  set('dl', app); set('dl2', app); set('vst', vst);
  const v = String(rel.tag_name || '').replace(/^v/, '');
  const ver = document.getElementById('ver');
  if (ver && /^\d+(\.\d+){1,3}$/.test(v)) ver.textContent = `v${v}`;
}

// ── languages: each has its own page (/, /en/, /ja/: what search engines show), but
//    switching swaps the page in place: no reload, the reader stays where they were.
//    The other pages are fetched as the menu opens, so the swap is at once. ──
const pages = new Map(); // path → Promise<Document>
function page(path) {
  if (!pages.has(path)) {
    const got = fetch(path)
      .then((r) => { if (!r.ok) throw new Error(r.status); return r.text(); })
      .then((t) => new DOMParser().parseFromString(t, 'text/html'));
    got.catch(() => pages.delete(path));
    pages.set(path, got);
  }
  return pages.get(path);
}
pages.set(location.pathname, Promise.resolve(document.cloneNode(true)));

async function switchTo(path, push) {
  let doc;
  try { doc = await page(path); } catch { location.href = path; return; } // (offline: an ordinary visit)
  const y = scrollY;
  document.documentElement.lang = doc.documentElement.lang;
  document.title = doc.title;
  for (const sel of ['meta[name="description"]', 'link[rel="canonical"]']) {
    const now = document.querySelector(sel), then = doc.querySelector(sel);
    if (now && then) now.replaceWith(then.cloneNode(true));
  }
  // (the new body's own script tag does not run again: this script takes the new body on)
  document.body.replaceWith(document.importNode(doc.body, true));
  if (push) history.pushState(null, '', path + location.hash);
  scrollTo(0, y);
  wire();
  document.querySelector('details.lang summary')?.focus({ preventScroll: true });
}

// back / forward between languages (a hash link within one page changes nothing here)
addEventListener('popstate', () => { if (location.pathname !== document.body.dataset.path) switchTo(location.pathname, false); });

/** What the page's own elements need, on the first load and after each swap. */
function wire() {
  document.body.dataset.path = location.pathname;
  showRelease();
  const menu = document.querySelector('details.lang');
  if (!menu) return;
  const links = [...menu.querySelectorAll('a[data-lang]')];
  menu.addEventListener('toggle', () => { if (menu.open) links.forEach((a) => page(a.getAttribute('href'))); });
  for (const a of links) {
    a.addEventListener('click', (e) => {
      // a choice is remembered, so the start page doesn't move the reader again
      try { localStorage.setItem('lang', a.dataset.lang); } catch { /* private window */ }
      if (e.ctrlKey || e.metaKey || e.shiftKey || e.button) return; // (a new tab or window: as usual)
      e.preventDefault();
      menu.open = false;
      const path = a.getAttribute('href');
      if (path !== location.pathname) switchTo(path, true);
    });
  }
}

// the menu closes on a click elsewhere or Escape (listened on the document: the body changes)
document.addEventListener('click', (e) => {
  const menu = document.querySelector('details.lang');
  if (menu?.open && !menu.contains(e.target)) menu.open = false;
});
document.addEventListener('keydown', (e) => {
  const menu = document.querySelector('details.lang');
  if (e.key === 'Escape' && menu?.open) { menu.open = false; menu.querySelector('summary').focus(); }
});

wire();
