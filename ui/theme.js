// before the page paints: the saved theme, so it does not flash the other one
try { const t = localStorage.getItem('gachi-theme'); if (t === 'light' || t === 'dark') document.documentElement.dataset.theme = t; } catch {}
