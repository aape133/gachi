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
      // &act=tracks,metro,…: something opened or switched on, to look at it (one after another)
      const ACT = {
        mixer: () => $('#view-seg [data-v=mixer]').click(),
        tracks: () => $('#view-seg [data-v=tracks]').click(),
        metro: () => $('#metro-more').click(),
        metroon: () => $('#metro-toggle').click(),
        rec: () => $('#rec-more').click(),
        recording: () => $('#rec-btn').click(),
        room: () => $('#btn-room').click(),
        lat: () => document.querySelectorAll('#stage-others .strip')[2]?.querySelector('[data-act=lat]').click(),
        color: () => document.querySelectorAll('#stage-others .strip')[1]?.querySelector('[data-act=color]').click(),
        colorme: () => $('#stage-me [data-act=color]').click(),
        chat: () => $('#btn-chat').click(),
        leave: () => $('#btn-leave').click(),
        feedback: () => { $('#feedback-warn').hidden = false; },
        audio: () => $('[data-open=settings]').click(),
        network: () => { $('[data-open=settings]').click(); $('[data-tab=network]').click(); },
        general: () => { $('[data-open=settings]').click(); $('[data-tab=display]').click(); },
        about: () => { $('[data-open=settings]').click(); $('[data-tab=about]').click(); },
      };
      const acts = (q.get('act') || '').split(',').filter((a) => ACT[a]);
      acts.forEach((a, i) => setTimeout(ACT[a], (shot === 'room' ? 1600 : 400) + i * 250));
    }, 200));
  }
}
