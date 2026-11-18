// The download buttons point at the latest release's files (else the release page, as
// written in the page). Asked of GitHub once per visit; nothing else is loaded.
(async () => {
  try {
    const r = await fetch('https://api.github.com/repos/aape133/gachi/releases/latest', { headers: { Accept: 'application/vnd.github+json' } });
    if (!r.ok) return;
    const rel = await r.json();
    const file = (re) => (rel.assets || []).find((a) => re.test(a.name))?.browser_download_url;
    const app = file(/_x64-setup\.exe$/), vst = file(/_vst3\.zip$/);
    if (app) { document.getElementById('dl').href = app; document.getElementById('dl2').href = app; }
    if (vst) document.getElementById('vst').href = vst;
    const v = String(rel.tag_name || '').replace(/^v/, '');
    if (/^\d+(\.\d+){1,3}$/.test(v)) document.getElementById('ver').textContent = `v${v}`;
  } catch { /* offline, or GitHub busy: the buttons keep the release page */ }
})();

// The language menu: a choice is remembered, so the start page doesn't move you again.
// The menu closes on a click elsewhere or Escape.
const langMenu = document.querySelector('details.lang');
for (const a of document.querySelectorAll('a[data-lang]')) {
  a.addEventListener('click', () => { try { localStorage.setItem('lang', a.dataset.lang); } catch { /* private window */ } });
}
document.addEventListener('click', (e) => { if (langMenu?.open && !langMenu.contains(e.target)) langMenu.open = false; });
document.addEventListener('keydown', (e) => { if (e.key === 'Escape' && langMenu?.open) { langMenu.open = false; langMenu.querySelector('summary').focus(); } });
