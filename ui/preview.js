// browser preview only: the desktop app makes QR codes itself (no CDN at runtime)
if (!window.__TAURI__) document.write('<script src="https://cdnjs.cloudflare.com/ajax/libs/qrcodejs/1.0.0/qrcode.min.js"><\/script>');

// browser preview only: ?shot=home|room (and &theme=dark|light) sets up a tidy demo screen,
// for the website's screenshots (names and codes are the demo's, nobody's real ones)
if (!window.__TAURI__) {
  const q = new URLSearchParams(location.search);
  const shot = q.get('shot');
  if (q.get('theme')) document.documentElement.dataset.theme = q.get('theme');
  if (shot) {
    addEventListener('load', () => setTimeout(() => {
      const $ = (s) => document.querySelector(s);
      if ($('#home-name')) { $('#home-name').value = '민준'; $('#home-name').dispatchEvent(new Event('input', { bubbles: true })); }
      if (shot === 'room') {
        $('#create-name').value = '금요일 합주';
        $('#btn-create').click();
        setTimeout(() => document.querySelectorAll('.modal-backdrop.open').forEach((m) => m.classList.remove('open')), 1200);
      }
    }, 200));
  }
}
