// browser preview only: the desktop app makes QR codes itself (no CDN at runtime)
if (!window.__TAURI__) document.write('<script src="https://cdnjs.cloudflare.com/ajax/libs/qrcodejs/1.0.0/qrcode.min.js"><\/script>');
