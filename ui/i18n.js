/* gachi — the app's words, from the language files in locales/ (one per language,
 * listed in locales/languages.json; en-US is the base the others fall back to).
 *
 * The app hands every file over before this page runs (window.GACHI_LOCALES, with the
 * saved choice and Windows' language); the browser preview reads them from locales/.
 *
 *   tr('card.name_joined', { name })      a sentence, {holes} filled
 *   data-i18n="home.create_room"          an element's text (its key)
 *   data-i18n-html=…                      an element's markup (<b>, <br>, <a> in the file)
 *   data-i18n-title / -placeholder / -aria   an attribute
 */
(() => {
  'use strict';
  const BASE = 'en-US';

  /** the browser preview: no app to hand the files over, so read them (synchronously:
   *  the page's script needs them before it runs) */
  function readFiles() {
    const get = (path) => { const x = new XMLHttpRequest(); x.open('GET', path, false); x.send(); return JSON.parse(x.responseText); };
    return Object.fromEntries(get('locales/languages.json').map((code) => [code, get(`locales/${code}.json`)]));
  }
  const FILES = window.GACHI_LOCALES || readFiles();
  const CODES = Object.keys(FILES);

  /** The language a choice means: a code we have ("ko-KR"), else one of the same language
   *  ("ko", "en-GB"), else English. 'auto': Windows' language. (As gachi_lang::resolve.) */
  function resolve(choice) {
    const sys = window.GACHI_SYSTEM_LANG || navigator.languages?.[0] || navigator.language || BASE;
    const want = String(!choice || choice === 'auto' ? sys : choice).replace('_', '-').toLowerCase();
    const lang = (c) => c.split('-')[0].toLowerCase();
    return CODES.find((c) => c.toLowerCase() === want) || CODES.find((c) => lang(c) === lang(want)) || BASE;
  }

  let lang = null, words = FILES[BASE];

  /** The settings' language. True when that changes what is spoken. */
  function set(choice) {
    const next = resolve(choice);
    if (next === lang) return false;
    lang = next;
    words = FILES[lang];
    document.documentElement.lang = lang;
    return true;
  }

  function tr(key, vars) {
    let s = words[key] ?? FILES[BASE][key];
    if (s == null) { console.warn('[i18n] no such key:', key); s = key; }
    return vars ? s.replace(/\{(\w+)\}/g, (m, k) => (k in vars ? String(vars[k]) : m)) : s;
  }

  const ATTRS = [['i18nTitle', 'title'], ['i18nPlaceholder', 'placeholder'], ['i18nAria', 'aria-label']];

  /** The marked elements under `root` in the language spoken now. */
  function apply(root = document.body) {
    for (const el of root.querySelectorAll('[data-i18n]')) el.textContent = tr(el.dataset.i18n);
    for (const el of root.querySelectorAll('[data-i18n-html]')) el.innerHTML = tr(el.dataset.i18nHtml);
    for (const el of root.querySelectorAll('[data-i18n-title], [data-i18n-placeholder], [data-i18n-aria]')) {
      for (const [k, attr] of ATTRS) if (el.dataset[k]) el.setAttribute(attr, tr(el.dataset[k]));
    }
    fit(root);
  }

  /* ── labels that must stay on one line: a little tighter if they don't fit, cut short
     (whole text on hover) if even that is not enough. Whatever a language's lengths. ── */
  const ONE_LINE = 'button, .btn, .seg > *, .label, .k, .rs-title, .dbtn';
  const ROWS = '.dock-me';
  // (controls made of several lines on purpose)
  const MANY_LINES = '.nearby-room, .status-strip, .invite-slot, .recent-bar, .code-chip, .plate, .lat, .swatch, .ms, .radio-row, [data-tab], .toast';
  // (text only: an icon's shapes are not lines)
  const lines = (el) => {
    const tops = [], r = document.createRange(), walk = document.createTreeWalker(el, NodeFilter.SHOW_TEXT);
    for (let n = walk.nextNode(); n; n = walk.nextNode()) {
      r.selectNodeContents(n);
      for (const x of r.getClientRects()) if (x.width >= 1 && !tops.some((t) => Math.abs(t - x.top) < x.height / 2)) tops.push(x.top);
    }
    return tops.length;
  };
  const tooLong = (el) => el.scrollWidth > el.clientWidth + 1 || lines(el) > 1;

  // a button with an icon can drop its words (they stay as the tooltip)
  const hasIcon = (el) => el.querySelector(':scope > svg, :scope > [data-icon]') && el.querySelector(':scope > span:not([data-icon])');

  function fit(root = document.body) {
    const els = [...root.querySelectorAll(ONE_LINE)].filter((el) => !el.closest(MANY_LINES));
    // all class changes, then all measuring: one layout per pass, not one per label
    for (const el of els) {
      el.classList.remove('fit-tight', 'fit-icon', 'fit-cut');
      // (a tooltip given here last time: the words may be others now, or fit)
      if (el.dataset.fitTitle != null) { el.removeAttribute('title'); delete el.dataset.fitTitle; }
    }
    const tight = els.filter((el) => el.offsetWidth && el.textContent.trim() && tooLong(el));
    for (const el of tight) el.classList.add('fit-tight');
    for (const el of tight.filter(tooLong)) {
      if (!el.title) { el.title = el.textContent.trim(); el.dataset.fitTitle = ''; }
      el.classList.add(hasIcon(el) ? 'fit-icon' : 'fit-cut');
    }
    // rows of buttons that keep their size (a card's foot): if they don't fit side by side,
    // the ones with an icon show only the icon
    const rows = [...root.querySelectorAll(ROWS)].filter((r) => r.offsetWidth && r.scrollWidth > r.clientWidth + 1);
    for (const row of rows) {
      for (const el of row.children) {
        if (!hasIcon(el)) continue;
        if (!el.title) { el.title = el.textContent.trim(); el.dataset.fitTitle = ''; }
        el.classList.add('fit-icon');
      }
    }
  }
  let refit = 0;
  addEventListener('resize', () => { clearTimeout(refit); refit = setTimeout(() => fit(), 150); });

  /** The languages there are, as [code, its own name], for the settings. */
  const languages = () => CODES.map((c) => [c, FILES[c]['language.name'] || c]);

  set(window.GACHI_LANG);
  window.i18n = { set, apply, fit, tr, languages, get lang() { return lang; } };
  window.tr = tr;
})();
