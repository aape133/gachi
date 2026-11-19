// browser preview only: the desktop app makes QR codes itself (no CDN at runtime)
if (!window.__TAURI__) document.write('<script src="https://cdnjs.cloudflare.com/ajax/libs/qrcodejs/1.0.0/qrcode.min.js"><\/script>');

// browser preview only: ?shot=home|room (and &theme=dark|light, &lang=ko|en|ja, &peers=N,
// &show for no waiting) sets up a tidy demo screen, for the website's screenshots (names and
// codes are the demo's, nobody's real ones)
if (!window.__TAURI__) {
  const q = new URLSearchParams(location.search);
  const shot = q.get('shot');
  if (q.get('theme')) document.documentElement.dataset.theme = q.get('theme');
  // my name and the room's, in the preview's language
  const WORDS = { ko: ['민준', '금요일 합주'], en: ['Alex', 'Friday jam'], ja: ['たくみ', '金曜セッション'] }[String(q.get('lang') || 'ko').slice(0, 2)] || ['Alex', 'Friday jam'];
  if (shot) {
    // (a picture: everything as it stands, nothing caught half-way through a fade or a move)
    document.head.insertAdjacentHTML('beforeend', '<style>*, *::before, *::after { animation: none !important; transition: none !important; }</style>');
    addEventListener('load', () => setTimeout(() => {
      const $ = (s) => document.querySelector(s);
      if ($('#home-name')) { $('#home-name').value = WORDS[0]; $('#home-name').dispatchEvent(new Event('input', { bubbles: true })); }
      if (shot === 'room') {
        $('#create-name').value = WORDS[1];
        $('#btn-create').click();
        setTimeout(() => {
          document.querySelectorAll('.modal-backdrop.open').forEach((m) => m.classList.remove('open'));
          // a session under way, not one that just began (the clock stays put)
          const clock = $('#session-time');
          const hold = () => { if (clock && clock.textContent !== '12:48') clock.textContent = '12:48'; };
          hold();
          if (clock) new MutationObserver(hold).observe(clock, { childList: true, characterData: true, subtree: true });
        }, 1200);
      }
    }, 200));
  }
}
