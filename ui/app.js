/* gachi — UI logic. Talks only to window.backend (see backend.js). */
(() => {
  'use strict';
  const backend = window.backend;
  const ALPHABET = window.CODE_ALPHABET;
  const { i18n, tr } = window;
  i18n.set(backend.getSettings().language);
  // the language changed in the settings: everything shown says it the new way (this
  // runs before the other settings listeners, which then draw in the new language)
  backend.on('settings', () => { if (i18n.set(backend.getSettings().language)) relocalize(); });

  /* ─────────────── helpers ─────────────── */
  const $ = (s, el = document) => el.querySelector(s);
  const $$ = (s, el = document) => [...el.querySelectorAll(s)];
  const h = (html) => { const t = document.createElement('template'); t.innerHTML = html.trim(); return t.content.firstElementChild; };
  const esc = (s) => String(s).replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
  const f1 = (n) => n.toFixed(1);
  /** text / an attribute, written only when it changes (the once-a-second updates of every
   *  card would otherwise lay the page out again for figures that stayed the same) */
  const put = (el, text) => { if (el && el.textContent !== text) el.textContent = text; };
  const putAttr = (el, name, v) => { if (el && el.getAttribute(name) !== v) el.setAttribute(name, v); };
  const fmtCode = (c) => `${c.slice(0, 3)}-${c.slice(3)}`;
  // the name a first run gets, in each language (the home screen asks for a real one)
  const DEFAULT_NAMES = i18n.everywhere('settings.default_name');
  const clamp = (v, a, b) => Math.min(b, Math.max(a, v));
  const dbText = (db) => (db <= -40 ? '-∞' : (db > 0 ? '+' : '') + f1(db));
  const panText = (v) => (v === 0 ? 'C' : (v < 0 ? 'L' : 'R') + Math.abs(v));
  const quality = (ms) => (ms < 20 ? ['good', tr('app.good')] : ms < 30 ? ['warn', tr('app.fair')] : ['bad', tr('app.high')]);
  const mmss = (ms) => {
    const s = Math.floor(ms / 1000), hh = Math.floor(s / 3600), mm = Math.floor((s % 3600) / 60), ss = s % 60;
    const p = (n) => String(n).padStart(2, '0');
    return hh ? `${hh}:${p(mm)}:${p(ss)}` : `${p(mm)}:${p(ss)}`;
  };
  const isTyping = () => ['INPUT', 'TEXTAREA', 'SELECT'].includes(document.activeElement?.tagName) &&
    document.activeElement.type !== 'range' && document.activeElement.type !== 'checkbox';

  /* ─────────────── icons ─────────────── */
  const ICON = {
    sliders: '<line x1="4" y1="21" x2="4" y2="14"/><line x1="4" y1="10" x2="4" y2="3"/><line x1="12" y1="21" x2="12" y2="12"/><line x1="12" y1="8" x2="12" y2="3"/><line x1="20" y1="21" x2="20" y2="16"/><line x1="20" y1="12" x2="20" y2="3"/><line x1="1" y1="14" x2="7" y2="14"/><line x1="9" y1="8" x2="15" y2="8"/><line x1="17" y1="16" x2="23" y2="16"/>',
    mic: '<rect x="9" y="2" width="6" height="12" rx="3"/><path d="M19 10v1a7 7 0 0 1-14 0v-1"/><line x1="12" y1="18" x2="12" y2="22"/>',
    'mic-off': '<line x1="2" y1="2" x2="22" y2="22"/><path d="M15 9.3V5a3 3 0 0 0-5.7-1.3"/><path d="M9 9v2a3 3 0 0 0 5.1 2.1"/><path d="M19 10v1a7 7 0 0 1-.9 3.4M16.9 16.9A7 7 0 0 1 5 11v-1"/><line x1="12" y1="18" x2="12" y2="22"/>',
    chat: '<path d="M21 15a2 2 0 0 1-2 2H7l-4 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2z"/>',
    copy: '<rect x="9" y="9" width="13" height="13" rx="2"/><path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1"/>',
    link: '<path d="M10 13a5 5 0 0 0 7.54.54l3-3a5 5 0 0 0-7.07-7.07l-1.72 1.71"/><path d="M14 11a5 5 0 0 0-7.54-.54l-3 3a5 5 0 0 0 7.07 7.07l1.71-1.71"/>',
    'user-plus': '<path d="M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2"/><circle cx="9" cy="7" r="4"/><line x1="19" y1="8" x2="19" y2="14"/><line x1="22" y1="11" x2="16" y2="11"/>',
    metronome: '<path d="M9 3h6l4 18H5z"/><path d="M12 16l5-8"/><line x1="7" y1="16" x2="17" y2="16"/>',
    x: '<line x1="18" y1="6" x2="6" y2="18"/><line x1="6" y1="6" x2="18" y2="18"/>',
    lock: '<rect x="4" y="11" width="16" height="10" rx="2"/><path d="M8 11V7a4 4 0 0 1 8 0v4"/>',
    unlock: '<rect x="4" y="11" width="16" height="10" rx="2"/><path d="M8 11V7a4 4 0 0 1 7.6-1.7"/>',
    refresh: '<path d="M21 12a9 9 0 1 1-2.6-6.4"/><polyline points="21 3 21 9 15 9"/>',
    'chevron-down': '<polyline points="6 9 12 15 18 9"/>',
    plus: '<line x1="12" y1="5" x2="12" y2="19"/><line x1="5" y1="12" x2="19" y2="12"/>',
    minus: '<line x1="5" y1="12" x2="19" y2="12"/>',
    headphones: '<path d="M3 18v-6a9 9 0 0 1 18 0v6"/><path d="M21 19a2 2 0 0 1-2 2h-1a2 2 0 0 1-2-2v-3a2 2 0 0 1 2-2h3zM3 19a2 2 0 0 0 2 2h1a2 2 0 0 0 2-2v-3a2 2 0 0 0-2-2H3z"/>',
    'log-out': '<path d="M9 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h4"/><polyline points="16 17 21 12 16 7"/><line x1="21" y1="12" x2="9" y2="12"/>',
    send: '<line x1="22" y1="2" x2="11" y2="13"/><polygon points="22 2 15 22 11 13 2 9 22 2"/>',
    check: '<polyline points="20 6 9 17 4 12"/>',
    sun: '<circle cx="12" cy="12" r="4"/><path d="M12 2v2M12 20v2M4.9 4.9l1.4 1.4M17.7 17.7l1.4 1.4M2 12h2M20 12h2M4.9 19.1l1.4-1.4M17.7 6.3l1.4-1.4"/>',
    moon: '<path d="M21 12.8A9 9 0 1 1 11.2 3a7 7 0 0 0 9.8 9.8z"/>',
    pencil: '<path d="M12 20h9"/><path d="M16.5 3.5a2.1 2.1 0 0 1 3 3L7 19l-4 1 1-4z"/>',
    alert: '<path d="M10.3 3.9 1.8 18a2 2 0 0 0 1.7 3h17a2 2 0 0 0 1.7-3L13.7 3.9a2 2 0 0 0-3.4 0z"/><line x1="12" y1="9" x2="12" y2="13"/><line x1="12" y1="17" x2="12.01" y2="17"/>',
    users: '<circle cx="9" cy="8" r="3"/><path d="M3 20a6 6 0 0 1 12 0M16 5.5a3 3 0 0 1 0 5.5M17 14.5a6 6 0 0 1 4 5.5"/>',
    input: '<path d="M3 12h7M14 5v14M10 9l4 3-4 3M18 8v8M21 10v4"/>',
    wave: '<path d="M3 12h3l2-6 4 12 3-9 2 3h4"/>',
    globe: '<circle cx="12" cy="12" r="9"/><path d="M3 12h18M12 3c3 3.5 3 14.5 0 18M12 3c-3 3.5-3 14.5 0 18"/>',
    info: '<circle cx="12" cy="12" r="9"/><path d="M12 11v6M12 7.5v.01"/>',
    clock: '<circle cx="12" cy="12" r="8.5"/><path d="M12 7.5V12l3 2"/>',
  };
  const icon = (n) => `<svg class="ic" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${ICON[n] || ''}</svg>`;
  const hydrateIcons = (root = document) => $$('[data-icon]', root).forEach((el) => {
    // an empty <span data-icon> only marks the place: put the icon itself there. Left inside
    // the span, the icon sat on the span's text baseline, ~2 px above the label next to it
    if (el.tagName === 'SPAN' && !el.textContent.trim() && !el.id && !el.className) {
      el.outerHTML = icon(el.dataset.icon);
      return;
    }
    el.insertAdjacentHTML('afterbegin', icon(el.dataset.icon));
    el.removeAttribute('data-icon');
  });

  function paintRange(el) {
    const min = +el.min, max = +el.max, p = ((+el.value - min) / (max - min)) * 100;
    const pan = el.classList.contains('pan');
    el.style.setProperty('--a', (pan ? Math.min(50, p) : 0) + '%');
    el.style.setProperty('--b', (pan ? Math.max(50, p) : p) + '%');
  }
  /** peak (0..1) → bar height on a dB scale: -60 dB at the bottom, 0 dB at the top */
  const meterPos = (p) => (p > 0.001 ? Math.min(1, Math.max(0, (20 * Math.log10(p) + 60) / 60)) : 0);
  /** rises at once, falls smoothly (about 30 dB a second) instead of flickering */
  function meterBar(el, peak) {
    // (30 times a second for every bar: the page is touched only when the bar moves, so
    // silent channels and people cost nothing to draw)
    const was = el._v || 0;
    const v = Math.max(meterPos(peak), was - 0.03);
    if (v !== was) { el._v = v; el.style.setProperty('--v', v.toFixed(3)); }
    // (at full scale: shown for a second, a single block of it is over before it is seen)
    if (peak >= 0.99) el._clipAt = performance.now();
    const clip = !!el._clipAt && performance.now() - el._clipAt < 1000;
    if (clip !== !!el._clip) { el._clip = clip; el.classList.toggle('clip', clip); }
  }
  const setMeter = (el, lv) => { if (!el) return; meterBar(el.children[0], lv[0]); if (el.children[1]) meterBar(el.children[1], lv[1]); };

  /* a slider clicked once follows the mouse wheel while the pointer is on it (until
     something else is clicked; the wheel scrolls as usual everywhere else): a notch moves
     it about 1/40 of its range (1 dB, 5 on the pan, 3 % of the click), with Shift one step */
  // (by how far the wheel turned, not by events: one notch of a mouse wheel is 100 px, a
  // wheel turned fast sends several in one event, a touchpad many small ones)
  const WHEEL_NOTCH = 100;
  let wheelLeft = 0, wheelOn = null;
  function sliderWheel(e) {
    const t = e.currentTarget;
    if (t !== document.activeElement || t.disabled) return;
    e.preventDefault();
    const min = +t.min, max = +t.max, step = +t.step || 1;
    const notch = e.shiftKey ? step : Math.max(step, Math.round((max - min) / 40 / step) * step);
    // (Shift turns the wheel sideways in some systems; lines rather than pixels in others)
    let d = Math.abs(e.deltaY) >= Math.abs(e.deltaX) ? e.deltaY : e.deltaX;
    if (e.deltaMode === 1) d *= WHEEL_NOTCH / 3;
    if (wheelOn !== t) { wheelOn = t; wheelLeft = 0; }
    wheelLeft -= d;
    const notches = Math.trunc(wheelLeft / WHEEL_NOTCH);
    if (!notches) return;
    wheelLeft -= notches * WHEEL_NOTCH;
    const v = clamp(+t.value + notches * notch, min, max);
    if (v === +t.value) return;
    t.value = v;
    t.dispatchEvent(new Event('input', { bubbles: true }));
    t.dispatchEvent(new Event('change', { bubbles: true }));
  }
  // (on a slider once it is clicked, not on the page: a wheel handler that may stop the
  // scrolling makes every scroll of the page wait for the script)
  const hookWheel = (e) => {
    const t = e.target;
    if (t instanceof HTMLInputElement && t.type === 'range' && !t.wheelHooked) {
      t.wheelHooked = true;
      t.addEventListener('wheel', sliderWheel, { passive: false });
    }
  };
  // (pressed, or focused from the keyboard; a window that is not in front gets no focus
  // events until it is)
  document.addEventListener('pointerdown', hookWheel, true);
  document.addEventListener('focusin', hookWheel);

  /** A note in the corner: 5 s (8 s with a button), held while the pointer is on it, three
   *  at most (the oldest goes). `kind`: 'info' (blue), 'ok' (green), 'warn' (yellow). */
  const TOAST_MAX = 3;
  function toast(text, { kind = 'info', action, onAction } = {}) {
    const el = h(`<div class="toast"><span class="dot ${kind === 'warn' ? 'warn' : kind === 'ok' ? '' : 'idle'}"></span><span class="t">${esc(text)}</span>`
      + (action ? `<button class="btn sm act" type="button">${esc(action)}</button>` : '')
      + `<button class="ibtn sm bare" type="button" title="${esc(tr('app.click_close'))}" aria-label="${esc(tr('app.click_close'))}">${icon('x')}</button></div>`);
    if (kind === 'info') el.querySelector('.dot').style.background = 'var(--accent)';
    const box = $('#toasts');
    box.append(el);
    while (box.childElementCount > TOAST_MAX) box.firstElementChild.remove();
    let timer = 0, left = action ? 8000 : 5000, since = 0;
    const close = () => {
      if (el.classList.contains('out')) return;
      clearTimeout(timer);
      el.classList.add('out');
      setTimeout(() => el.remove(), 260);
    };
    const run = () => { since = Date.now(); timer = setTimeout(close, left); };
    el.addEventListener('pointerenter', () => { clearTimeout(timer); left -= Date.now() - since; });
    el.addEventListener('pointerleave', run);
    el.querySelector('.ibtn').addEventListener('click', close);
    el.querySelector('.act')?.addEventListener('click', () => { close(); onAction?.(); });
    run();
  }
  async function copy(text, msg) {
    try { await navigator.clipboard.writeText(text); }
    catch {
      const ta = h('<textarea style="position:fixed;opacity:0"></textarea>');
      ta.value = text; document.body.append(ta); ta.select(); document.execCommand('copy'); ta.remove();
    }
    toast(msg);
  }

  function show(name) {
    $$('.screen').forEach((s) => s.classList.toggle('active', s.id === 'screen-' + name));
    // (the toasts sit above the room's bottom bar, in the corner elsewhere)
    document.body.classList.toggle('screen-home-on', name !== 'room');
  }

  /* ─────────────── latency segments ─────────────── */
  const SEG = [
    ['inBuf', 'latency.input_device', 'var(--seg-in)'],
    ['net', 'latency.network_one_way', 'var(--seg-net)'],
    ['jitter', 'settings.jitter_buffer', 'var(--seg-jit)'],
    ['outBuf', 'latency.my_output_device', 'var(--seg-out)'],
  ];
  const BAR_SCALE = 40; // ms that fills the bar
  function paintBar(fill, b, total) {
    fill.style.width = clamp((total / BAR_SCALE) * 100, 4, 100) + '%';
    const segs = SEG.filter(([k]) => b[k] > 0);
    if (fill.childElementCount !== segs.length) fill.innerHTML = segs.map(([, , c]) => `<i style="background:${c}"></i>`).join('');
    segs.forEach(([k], i) => { fill.children[i].style.flex = b[k]; });
  }

  /* ═══════════════════════ HOME ═══════════════════════ */
  let createMax = 8;
  let netInfo = null, netChecking = false;

  $('#create-max').addEventListener('click', (e) => {
    const b = e.target.closest('button'); if (!b) return;
    createMax = +b.dataset.v;
    $$('#create-max button').forEach((x) => x.classList.toggle('on', x === b));
  });
  $('#create-pw-on').addEventListener('change', (e) => {
    $('#create-pw').hidden = !e.target.checked;
    if (e.target.checked) $('#create-pw').focus();
  });

  $('#btn-create').addEventListener('click', async () => {
    const btn = $('#btn-create'), label = $('span:last-child', btn);
    btn.disabled = true; label.textContent = tr('app.creating');
    try {
      const password = $('#create-pw-on').checked ? $('#create-pw').value : '';
      // (left empty: the room is called what the field said it would be)
      const named = $('#create-name').value.trim();
      const me = backend.getSettings().name;
      const name = named || (me && !DEFAULT_NAMES.includes(me) ? tr('home.names_room', { name: me }) : '');
      const r = await backend.createRoom({ name, maxPeers: createMax, password });
      enterRoom(r, password);
      openInvite();
    } catch (err) {
      toast(err.message);
    } finally {
      btn.disabled = false; label.textContent = tr('home.create_room');
    }
  });

  /* code input: one real <input> drawn as 6 boxes */
  const codeReal = $('#code-real');
  const boxes = $$('#code-wrap .box');
  const hint = $('#join-hint');
  const HINT_DEFAULT = 'home.paste_too_ctrl';

  function normCode(raw) {
    const cleaned = raw.toUpperCase().replace(/[^A-Z0-9]/g, '');
    const ok = [...cleaned].filter((c) => ALPHABET.includes(c)).join('');
    return { code: ok.slice(0, 6), rejected: ok.length !== cleaned.length };
  }
  function renderCode() {
    const v = codeReal.value, focused = document.activeElement === codeReal;
    boxes.forEach((b, i) => {
      b.textContent = v[i] || '';
      b.classList.toggle('filled', !!v[i]);
      b.classList.toggle('cursor', focused && i === v.length);
    });
    $('#btn-join').disabled = v.length !== 6;
  }
  function setHint(text, err = false) { hint.textContent = text; hint.classList.toggle('err', err); }
  function shake() { const w = $('#code-wrap'); w.classList.remove('shake'); void w.offsetWidth; w.classList.add('shake'); }

  codeReal.addEventListener('input', () => {
    const { code, rejected } = normCode(codeReal.value);
    codeReal.value = code;
    if (rejected) { setHint(tr('app.codes_never_use'), true); shake(); }
    else setHint(tr(HINT_DEFAULT));
    renderCode();
  });
  codeReal.addEventListener('focus', renderCode);
  codeReal.addEventListener('blur', renderCode);
  codeReal.addEventListener('keydown', (e) => { if (e.key === 'Enter' && codeReal.value.length === 6) join(codeReal.value); });
  $('#btn-join').addEventListener('click', () => join(codeReal.value));

  // paste a code anywhere on the home screen
  document.addEventListener('paste', (e) => {
    if (!$('#screen-home').classList.contains('active') || isTyping()) return;
    const { code } = normCode(e.clipboardData.getData('text'));
    if (code.length === 6) { codeReal.value = code; codeReal.focus(); renderCode(); setHint(tr('app.code_pasted_enter')); }
  });

  /* ═══════════════════════ CONNECTING ═══════════════════════ */
  const STEP_LABELS = { signal: 'connect.finding_room', exchange: 'connect.exchanging_addresses', punch: 'connect.direct_p2p_connection', measure: 'connect.measuring_latency' };

  /** Joins a room. Returns 'ok', or why not: 'stop' (asked for a password, refused,
   *  cancelled: nothing to retry) or 'retry' (no connection). */
  async function join(code, password) {
    $('#connect-code').textContent = fmtCode(code);
    $('#connect-steps').innerHTML = Object.entries(STEP_LABELS)
      .map(([id, l]) => `<li data-step="${id}"><span class="st"></span><span>${tr(l)}</span><span class="detail"></span></li>`).join('');
    show('connecting');
    try {
      const r = await backend.joinRoom(code, (id, st, detail) => {
        const li = $(`[data-step="${id}"]`);
        li.className = st;
        if (st === 'done') { li.querySelector('.st').innerHTML = icon('check'); li.querySelector('.detail').textContent = detail || ''; }
      }, password);
      codeReal.value = ''; renderCode();
      enterRoom(r, password);
      return 'ok';
    } catch (err) {
      show('home');
      // the room left last has closed meanwhile (everyone left): nothing to go back to
      if (err.kind === 'not_found' && recent?.code === code) { recent = null; renderRecent(); }
      if (err.kind === 'password_required' || err.kind === 'wrong_password') { askPassword(code, err.kind === 'wrong_password'); return 'stop'; }
      if (err.message !== 'cancelled') { setHint(err.message, true); shake(); }
      return err.kind || err.message === 'cancelled' ? 'stop' : 'retry';
    }
  }

  // our own internet dropped and the room went on without us: back in through its code,
  // trying a few times while the connection comes back
  let rejoining = false;
  backend.on('room-lost', async ({ code, password }) => {
    if (!room || rejoining) return;
    rejoining = true;
    leaveRoom({ keepRecent: true });
    toast(tr('app.connection_dropped_left'));
    try {
      for (let i = 0; i < 8; i++) {
        const r = await join(code, password || undefined);
        if (r !== 'retry') break;
        if (i === 7) { setHint(tr('app.couldnt_rejoin_room'), true); break; }
        await new Promise((res) => setTimeout(res, 4000));
        if (room) break;
      }
    } finally { rejoining = false; }
  });

  // password rooms: ask, then join again with it
  let pwFor = null;
  function askPassword(code, wrong) {
    pwFor = code;
    $('#pw-note').textContent = wrong ? tr('app.wrong_password_please') : tr('dialog.room_has_password');
    $('#pw-note').classList.toggle('q-warn', wrong);
    $('#pw-input').value = '';
    openModal('modal-pw');
    setTimeout(() => $('#pw-input').focus(), 50);
  }
  $('#pw-form').addEventListener('submit', (e) => {
    e.preventDefault();
    const pw = $('#pw-input').value;
    if (!pw || !pwFor) return;
    closeModal($('#modal-pw'));
    join(pwFor, pw);
  });
  $('#btn-cancel-connect').addEventListener('click', () => backend.cancelJoin());

  /* ═══════════════════════ ROOM ═══════════════════════ */
  let room = null;
  const peers = new Map();   // id → PeerView
  const strips = new Map();  // id → { el, meter, avatar }
  // the room left last (see `renderRecent`), the nearby list as last looked up
  let recent = null, lastNearby = [], recentTimer = null;
  let clockTimer = null;

  /** `password`: what the room was entered with (kept in memory, for going back in: see
   *  `recent`) */
  function enterRoom(r, password = '') {
    room = r;
    clearInterval(recentTimer);
    room.pw = password || '';
    peers.clear(); strips.clear(); slots.clear(); nextSlot = 1;
    $('#room-name').textContent = r.name; $('#room-name').title = r.name; // (a long name is cut short in the bar)
    $('#room-code').textContent = fmtCode(r.code);
    $('#stage-me').innerHTML = ''; $('#stage-others').innerHTML = '';
    $('#stage-me').append(buildSelfStrip());
    applyView();
    $('#chat-list').innerHTML = '';
    sysMsg(r.isHost ? tr('app.room_created_share') : tr('app.joined_name', { name: r.name }));
    r.peers.forEach((p) => addPeer(p));
    qualityAdvised = false;
    adviseQuality();
    updateRoomChrome();
    syncSelfControls();
    clearInterval(clockTimer);
    const tick = () => { $('#session-time').textContent = mmss(Date.now() - r.startedAt); };
    tick(); clockTimer = setInterval(tick, 1000);
    show('room');
    // (the strips' size and labels, now that the room has a size: before it was hidden)
    fitStage();
  }

  /** `keepRecent`: offer the room on the home screen, to go back in with one click: when
   *  the room goes on without us (our connection dropped, or we left with others still in
   *  it), not when we were the last one out or were put out */
  function leaveRoom({ keepRecent = false } = {}) {
    backend.leaveRoom();
    // (a room with its code turned off cannot be joined)
    recent = keepRecent && room && room.open !== false ? { code: room.code, name: room.name, pw: room.pw || '', at: Date.now() } : null;
    renderRecent();
    room = null;
    clearInterval(clockTimer);
    closePop(); closeColorPop();
    // (the room's own window: its buttons speak of a room that is no more)
    closeModal($('#modal-invite'));
    setMetro(false, { send: false });
    if (rec.on) { setRecUi(false); toast(tr('app.recording_saved_left')); }
    closeMenus();
    $('#screen-room').classList.remove('chat-open');
    unread = 0; updateBadge();
    show('home');
    syncIdleEngine();
    // the room's device closes in the background: read it once it has
    setTimeout(probeDevice, 1500);
    refreshNearby();
    toast(tr('app.left_room'));
  }

  /* ── the people's colours: given out in the order they come in, on this screen (each one
     sees it from their own side, themselves first), stepping through the palette so that
     people next to each other get colours far apart. A colour picked by hand is kept by the
     person's name on this PC ── */
  const PALETTE = ['#E06A6E', '#E8845A', '#E8A04A', '#D9B84A', '#B5C45A', '#86C46A', '#5DBE7E', '#46B89A', '#4AC4C0', '#4AB0D8', '#5C9CF0', '#7A7CF0', '#9C7BE6', '#C77BD9', '#D46AB4', '#E07A98', '#C9A27A', '#A9ADB8'];
  const PALETTE_NAMES = ['color.red', 'color.vermilion', 'color.orange', 'color.yellow', 'color.olive', 'color.lime', 'color.green', 'color.teal', 'color.mint', 'color.sky', 'color.blue', 'color.indigo', 'color.purple', 'color.magenta', 'color.pink', 'color.rose', 'color.brown', 'color.gray'];
  const GIVE_ORDER = [10, 0, 6, 2, 12, 3, 8, 15, 5, 11, 1, 7, 14, 4, 9, 13, 16, 17];
  const slots = new Map();
  let nextSlot = 1;
  const stored = (key, fallback) => { try { return JSON.parse(localStorage.getItem(key)) ?? fallback; } catch { return fallback; } };
  const store = (key, v) => { try { localStorage.setItem(key, JSON.stringify(v)); } catch { /* fine without */ } };
  /** (the last 200 people only: the list is never let grow without end) */
  const KEEP_PEOPLE = 200;
  let pickedColors = stored('gachi-track-colors', {});
  const nameOf = (id) => (id === 'self' ? backend.getSettings().name : peers.get(id)?.name) || '';
  const slotOf = (id) => { if (id === 'self') return 0; if (!slots.has(id)) slots.set(id, nextSlot++); return slots.get(id); };
  const colorOf = (id) => pickedColors[nameOf(id)] || PALETTE[GIVE_ORDER[slotOf(id) % GIVE_ORDER.length]];
  function setColor(id, c) {
    const name = nameOf(id); if (!name) return;
    const next = { ...pickedColors };
    delete next[name];
    next[name] = c;
    const names = Object.keys(next);
    names.slice(0, Math.max(0, names.length - KEEP_PEOPLE)).forEach((n) => delete next[n]);
    pickedColors = next;
    store('gachi-track-colors', pickedColors);
    paintColor(id);
  }
  function paintColor(id) {
    strips.get(id)?.el.style.setProperty('--tc', colorOf(id));
    $$(`.msg[data-from="${CSS.escape(id)}"]`, $('#chat-list')).forEach((m) => m.style.setProperty('--tc', colorOf(id)));
    if (popFor === id) pop.style.setProperty('--tc', colorOf(id));
  }

  /* ── the others' order: as this viewer arranged them (by name, on this PC), the ones not
     arranged yet in the order they came in ── */
  let savedOrder = stored('gachi-track-order', []);
  function placeStrip(el, name) {
    const box = $('#stage-others');
    const at = savedOrder.indexOf(name);
    if (at >= 0) {
      const before = $$('.strip', box).find((s) => { const i = savedOrder.indexOf(peers.get(s.dataset.id)?.name); return i < 0 || i > at; });
      if (before) { box.insertBefore(el, before); return; }
    }
    const slot = $('.invite-slot', box);
    slot ? slot.before(el) : box.append(el);
  }
  function saveOrder() {
    const now = $$('#stage-others .strip').map((s) => peers.get(s.dataset.id)?.name).filter(Boolean);
    savedOrder = [...now, ...savedOrder.filter((n) => !now.includes(n))].slice(0, KEEP_PEOPLE);
    store('gachi-track-order', savedOrder);
  }

  /* ── the view: a mixer (strips side by side) or tracks (a row each). The first time by
     the window's shape (tall: tracks), after that as last picked ── */
  let view = stored('gachi-view', null);
  function applyView() {
    const v = view || (innerHeight > innerWidth ? 'tracks' : 'mixer');
    const st = $('#stage');
    st.classList.toggle('mixer', v === 'mixer');
    st.classList.toggle('tracks', v === 'tracks');
    $$('#view-seg button').forEach((b) => b.classList.toggle('on', b.dataset.v === v));
    fitStage();
  }
  $('#view-seg').addEventListener('click', (e) => {
    const b = e.target.closest('button'); if (!b) return;
    view = b.dataset.v;
    store('gachi-view', view);
    closePop(); closeColorPop();
    applyView();
  });

  /* ── a person: the same parts in both views (the styles lay them out) ── */
  const KNOB_DEG = 135; // the pan knob turns this far either way
  /** the fader's scale: where each mark sits, from the bottom (`min`) to the top (`max`) */
  const scaleHtml = (marks, min, max) => marks.map((d) => `<span style="bottom:${((d - min) / (max - min)) * 100}%">${d === min && min <= -40 ? '-∞' : d > 0 ? '+' + d : d}</span>`).join('');
  const PEER_MARKS = [6, 0, -6, -12, -24, -40];
  const GAIN_MARKS = [24, 12, 0, -12, -24];

  function buildSelfStrip() {
    const el = h(`
      <article class="strip self" data-id="self">
        <button class="lat q-accent" data-act="local"><i class="dot"></i><span class="t" data-f="local"></span></button>
        <div class="knob-box" aria-hidden="true"><div class="knob"><div class="knob-face"></div></div><span class="knob-val"></span></div>
        <div class="msr">
          <button class="ms mic" data-act="mute">M</button>
          <button class="ms mon" data-act="monitor">MON</button>
          <button class="ms r" data-act="rec" title="${esc(tr('card.include_recording_only'))}" aria-label="${esc(tr('card.include_recording_only'))}"><i></i></button>
        </div>
        <div class="lane">
          <div class="meter" data-f="meter"><i></i><i></i></div>
          <div class="scale" data-f="scale"></div>
          <div class="fader"><input type="range" step="0.5" data-act="fader"></div>
          <span class="db" data-f="db"></span>
        </div>
        <button class="plate" data-act="color" title="${esc(tr('card.my_track'))}"><b class="nm" data-f="name"></b><span class="inst" data-f="inst"></span></button>
      </article>`);
    strips.set('self', { el, meter: $('[data-f=meter]', el) });
    return el;
  }

  /** what changes on my own strip and the bottom bar (the settings, my buttons) */
  function syncSelfControls() {
    const s = backend.getSettings(), self = backend.self;
    const daw = s.connection === 'daw';
    // the bottom bar: the mic, the monitor (VST mode: my own sound through the plugin), the input
    const mic = $('#dock-mic');
    mic.classList.toggle('off', !!self.muted);
    mic.setAttribute('aria-pressed', String(!self.muted));
    mic.innerHTML = icon(self.muted ? 'mic-off' : 'mic') + `<span class="wide-label">${esc(tr('card.mic'))}</span>`;
    mic.title = self.muted ? tr('card.muted_click_unmute') : tr('card.mute_mic');
    const mon = $('#dock-mon'), monOn = daw ? self.own !== false : !!self.monitor;
    mon.classList.toggle('on', monOn);
    mon.setAttribute('aria-pressed', String(monOn));
    mon.title = daw ? tr('card.hear_yourself_daw') : tr('card.hear_yourself_directly');
    $('#room-input-pick').hidden = daw;
    syncDawChip();
    fillInputSelect($('#room-input'), s);
    const st = strips.get('self'); if (!st) return;
    const el = st.el;
    el.style.setProperty('--tc', colorOf('self'));
    put($('[data-f=local]', el), f1(backend.localMs()) + 'ms');
    $('[data-act=local]', el).title = (daw
      ? tr('card.input_output_daws') + ` · ${tr('settings.n_samples', { n: s.bufferSize })} · ${s.sampleRate / 1000}kHz`
      : tr('card.my_device_input') + ` · ${tr('settings.n_samples', { n: s.bufferSize })} · ${s.sampleRate / 1000}kHz`) +` · ${tr('card.sending')}: ${streamText(myStream())} · ${tr('card.click_audio_settings')}`;
    const m = $('[data-act=mute]', el);
    m.classList.toggle('on', !!self.muted);
    m.title = self.muted ? tr('card.muted_click_unmute') : tr('card.mute_mic');
    m.setAttribute('aria-label', m.title);
    const mo = $('[data-act=monitor]', el);
    mo.classList.toggle('on', monOn);
    mo.title = mon.title; mo.setAttribute('aria-label', mon.title);
    $('[data-act=rec]', el).classList.toggle('on', self.rec !== false);
    el.classList.toggle('rec-on', self.rec !== false);
    // the fader: what I send (device mode), or how loud I hear myself in the DAW (VST mode)
    const f = $('[data-act=fader]', el);
    const [min, max, v] = daw ? [-40, 6, self.monitorDb ?? 0] : [-24, 24, self.gain];
    if (+f.min !== min) { f.min = min; f.max = max; $('[data-f=scale]', el).innerHTML = scaleHtml(daw ? PEER_MARKS : GAIN_MARKS, min, max); }
    f.value = v; paintRange(f);
    f.title = daw ? tr('card.how_loud_hear') : tr('card.input');
    f.setAttribute('aria-label', f.title);
    put($('[data-f=db]', el), dbText(v));
    put($('[data-f=name]', el), s.name);
    put($('[data-f=inst]', el), (room?.isHost ? tr('card.host') + ' · ' : '') + (s.instrument || tr('card.add_instrument')));
    st.meter.classList.toggle('mono', myStream().channels === 1);
    i18n.fit(el);
  }
  /** VST mode, the bottom bar: the plugin's state where the input would be */
  function syncDawChip() {
    const daw = backend.getSettings().connection === 'daw';
    $('#room-daw').hidden = !daw;
    if (!daw) return;
    put($('#room-daw-text'), tr('settings.vst_link') + ' · ' + (backend._plugin ? tr('netcheck.plugin_connected') : tr('netcheck.waiting_plugin')));
    $('#room-daw-dot').className = 'dot' + (backend._plugin && !backend._pluginWaiting ? '' : ' warn');
  }

  function toggleMute() {
    backend.setSelf({ muted: !backend.self.muted });
    syncSelfControls();
    toast(backend.self.muted ? tr('card.mute_mic') : tr('card.mic_2'), { kind: backend.self.muted ? 'warn' : 'ok' });
  }
  function toggleMonitor() {
    // VST mode: your own sound through the plugin; else the app's software monitor
    if (backend.getSettings().connection === 'daw') backend.setSelf({ own: backend.self.own === false });
    else backend.setSelf({ monitor: !backend.self.monitor });
    syncSelfControls();
  }
  $('#dock-mic').addEventListener('click', toggleMute);
  $('#dock-mon').addEventListener('click', toggleMonitor);

  function addPeer(p, announce = false) {
    peers.set(p.id, p);
    slotOf(p.id);
    const el = h(`
      <article class="strip" data-id="${esc(p.id)}">
        <button class="lat" data-act="lat" title="${esc(tr('card.connection_latency_details'))}"><i class="dot"></i><span class="t" data-f="total"></span><span data-f="warn" hidden>${icon('alert')}</span></button>
        <div class="knob-box"><div class="knob" title="${esc(tr('card.pan'))}"><div class="knob-face" data-f="knob"></div><input type="range" min="-100" max="100" step="1" value="${p.pan}" data-act="pan" aria-label="${esc(tr('card.pan'))}"></div><span class="knob-val" data-f="pan"></span></div>
        <div class="msr">
          <button class="ms m" data-act="mute" title="${esc(tr('card.mute_silence_person'))}" aria-label="${esc(tr('card.mute_silence_person'))}">M</button>
          <button class="ms s" data-act="solo" title="${esc(tr('card.solo_hear_only'))}" aria-label="${esc(tr('card.solo_hear_only'))}">S</button>
          <button class="ms r" data-act="rec" title="${esc(tr('card.include_recording_only'))}" aria-label="${esc(tr('card.include_recording_only'))}"><i></i></button>
        </div>
        <div class="lane">
          <div class="meter" data-f="meter"><i></i><i></i></div>
          <div class="scale">${scaleHtml(PEER_MARKS, -40, 6)}</div>
          <div class="fader"><input type="range" min="-40" max="6" step="0.5" value="${p.volume}" data-act="vol" title="${esc(tr('card.vol'))}" aria-label="${esc(tr('card.vol'))}"></div>
          <span class="db" data-f="vol"></span>
        </div>
        <button class="plate" data-act="color" draggable="true" title="${esc(tr('card.drag_or_color'))}"><b class="nm" data-f="name"></b><span class="inst" data-f="inst"></span></button>
      </article>`);
    strips.set(p.id, { el, meter: $('[data-f=meter]', el) });
    placeStrip(el, p.name);
    $$('input[type=range]', el).forEach(paintRange);
    updatePeer(p);
    if (announce) { const m = tr('card.name_joined', { name: p.name }); sysMsg(m); toast(m, { kind: 'ok' }); }
    updateRoomChrome();
  }

  function removePeer(id) {
    const p = peers.get(id), st = strips.get(id);
    peers.delete(id); strips.delete(id);
    if (popFor === id) closePop();
    if (colorPopFor === id) closeColorPop();
    if (st) { st.el.classList.add('leaving'); setTimeout(() => st.el.remove(), 250); }
    if (p) sysMsg(tr('card.name_left', { name: p.name }));
    setTimeout(updateRoomChrome, 260);
  }

  function updatePeer(p) {
    peers.set(p.id, p);
    const st = strips.get(p.id); if (!st) return;
    const el = st.el;
    el.style.setProperty('--tc', colorOf(p.id));
    put($('[data-f=name]', el), p.name);
    put($('[data-f=inst]', el), (p.isHost ? tr('card.host') + ' · ' : '') + (p.instrument || ''));
    putAttr($('.plate', el), 'aria-label', `${p.name} · ${tr('card.drag_or_color')}`);
    const connecting = p.connected === false;
    // until the round trip and the stream's timing are measured the figure is an estimate
    const [qc, ql] = connecting ? ['idle', tr('card.connecting')] : p.measured === false ? ['idle', tr('card.measuring')] : quality(p.total);
    const chip = $('[data-act=lat]', el);
    putAttr(chip, 'class', 'lat q-' + qc + (chip.classList.contains('active') ? ' active' : ''));
    put($('[data-f=total]', el), connecting || p.measured === false ? ql : `${f1(p.total)}ms`);
    const issues = [p.conn.link === 'wifi' && 'Wi-Fi', p.loss >= 1 && tr('card.loss_n', { n: f1(p.loss) }), p.outageMs > 0 && tr('card.brief_dropout')].filter(Boolean);
    $('[data-f=warn]', el).hidden = !issues.length;
    putAttr(chip, 'title', `P2P${p.conn.ipv6 ? ' · IPv6' : ''} · ${tr('card.latency')} ${f1(p.total)} ms (${ql})`
      + (issues.length ? ' · ' + issues.join(' · ') : '') + ' · ' + tr('card.click_details'));
    put($('[data-f=vol]', el), dbText(p.volume));
    put($('[data-f=pan]', el), panText(p.pan));
    $('[data-f=knob]', el).style.transform = `rotate(${(p.pan / 100) * KNOB_DEG}deg)`;
    const vol = $('[data-act=vol]', el), pan = $('[data-act=pan]', el);
    if (document.activeElement !== vol && +vol.value !== p.volume) { vol.value = p.volume; paintRange(vol); }
    if (document.activeElement !== pan && +pan.value !== p.pan) pan.value = p.pan;
    $('[data-act=mute]', el).classList.toggle('on', !!p.mute);
    $('[data-act=solo]', el).classList.toggle('on', !!p.solo);
    $('[data-act=rec]', el).classList.toggle('on', p.rec !== false);
    el.classList.toggle('rec-on', p.rec !== false);
    el.classList.toggle('away', connecting);
    st.meter.classList.toggle('mono', p.stream?.channels === 1);
  }

  function updateDim() {
    const anySolo = [...peers.values()].some((p) => p.solo);
    peers.forEach((p) => strips.get(p.id)?.el.classList.toggle('dim', !!p.mute || (anySolo && !p.solo)));
  }

  /** The strips as wide as fit the window (64-132), the rows as tall (40-72; 58 at least
   *  when narrow, where each row holds its meter over its volume). Looked at again whenever
   *  the people, the window or the view change. */
  function fitStage() {
    if (!room) return;
    const stage = $('#stage'), n = peers.size + 1;
    if (stage.classList.contains('mixer')) {
      // (a strip is its width + 14 of padding and border, 4 between two; the stage around
      // them, mine set apart, takes 69)
      const w = stage.clientWidth;
      const sw = clamp(Math.floor((w - 69 - 4 * Math.max(0, n - 2)) / n) - 14, 64, 132);
      stage.style.setProperty('--strip-w', sw + 'px');
      stage.style.setProperty('--meter-w', (sw >= 100 ? 8 : 6) + 'px');
      stage.style.setProperty('--name-size', (sw >= 100 ? 14 : 12) + 'px');
    } else {
      // (a row is its height + 2 of border + 4 below it)
      const rh = clamp(Math.floor((stage.clientHeight - 24) / n) - 6, innerWidth < 900 ? 58 : 40, 72);
      stage.style.setProperty('--row-h', rh + 'px');
      stage.style.setProperty('--meter-h', (rh >= 60 ? 4 : 3) + 'px');
      stage.style.setProperty('--name-size', (rh >= 60 ? 14 : 12) + 'px');
    }
    // the labels for their size now (also the first time the room shows: this runs whenever
    // the stage changes size, and before that the room is hidden and measures 0)
    i18n.fit(stage);
  }
  // (the stage's own size: the window, the chat panel opening; and the window itself, which
  // is also heard of while nothing is being drawn)
  new ResizeObserver(() => { if (room) fitStage(); }).observe($('#stage'));
  // (a moment later: at the event itself the page is not always laid out at the new size yet)
  let fitTimer = 0;
  window.addEventListener('resize', () => { clearTimeout(fitTimer); fitTimer = setTimeout(() => { if (room) fitStage(); }, 60); });
  function updateRoomChrome() {
    if (!room) return;
    const count = peers.size + 1;
    const open = room.open !== false;
    $('#room-count').textContent = `${count}/${room.maxPeers}`;
    const code = $('#room-code');
    code.textContent = open ? fmtCode(room.code) : tr('card.code_off');
    $('#btn-room').classList.toggle('closed', !open);
    if (roomPanel.classList.contains('open')) fillRoomPanel();
    const box = $('#stage-others');
    let slot = $('.invite-slot', box);
    if (slot && !open) { slot.remove(); slot = null; }
    if (open && count < room.maxPeers) {
      if (!slot) {
        slot = h(`<button class="invite-slot">${icon('user-plus')}<span>${esc(tr('card.invite_friends'))}</span><span class="mono">${fmtCode(room.code)}</span></button>`);
        slot.addEventListener('click', openInvite);
        box.append(slot);
      }
    } else slot?.remove();
    syncSelfControls();
    fitStage();
    updateDim();
  }

  /* ── on the stage (delegated) ── */
  const stage = $('#stage');
  stage.addEventListener('input', (e) => {
    const t = e.target, act = t.dataset.act; if (!act) return;
    const id = t.closest('.strip').dataset.id;
    if (t.type === 'range') paintRange(t);
    if (id === 'self') {
      if (act === 'fader') {
        if (backend.getSettings().connection === 'daw') backend.setSelf({ monitorDb: +t.value });
        else backend.setSelf({ gain: +t.value });
        syncSelfControls();
      }
      return;
    }
    const p = peers.get(id); if (!p) return;
    // (shown at once: the figures that confirm it come back a moment later)
    if (act === 'vol') { p.volume = +t.value; backend.setPeer(id, { volume: p.volume }); }
    if (act === 'pan') { p.pan = +t.value; backend.setPeer(id, { pan: p.pan }); }
    updatePeer(p);
  });
  $('#room-input').addEventListener('change', (e) => backend.updateSettings({ inputChannel: e.target.value }));
  stage.addEventListener('dblclick', (e) => {
    const t = e.target; if (t.type !== 'range') return;
    t.value = 0;
    t.dispatchEvent(new Event('input', { bubbles: true }));
  });
  stage.addEventListener('click', (e) => {
    const b = e.target.closest('button[data-act]'); if (!b) return;
    const id = b.closest('.strip')?.dataset.id, act = b.dataset.act; if (!id) return;
    if (act === 'color') { colorPopFor === id ? closeColorPop() : openColorPop(id, b); return; }
    if (id === 'self') {
      if (act === 'mute') toggleMute();
      if (act === 'monitor') toggleMonitor();
      if (act === 'rec') { backend.setSelf({ rec: backend.self.rec === false }); syncSelfControls(); }
      if (act === 'local') openSettings('audio');
      return;
    }
    const p = peers.get(id); if (!p) return;
    if (act === 'mute') { p.mute = !p.mute; backend.setPeer(id, { mute: p.mute }); }
    if (act === 'solo') { p.solo = !p.solo; backend.setPeer(id, { solo: p.solo }); }
    if (act === 'rec') { p.rec = p.rec === false; backend.setPeer(id, { rec: p.rec }); }
    if (act === 'lat') { popFor === id ? closePop() : openPop(id, b); return; }
    updatePeer(p);
    updateDim();
  });

  /* ── the others' order: a name plate dragged to another place ── */
  let dragId = null;
  const clearDrop = () => $$('#stage .drop-before, #stage .drop-after').forEach((s) => s.classList.remove('drop-before', 'drop-after'));
  stage.addEventListener('dragstart', (e) => {
    const plate = e.target.closest?.('.plate[draggable=true]'); if (!plate) return;
    dragId = plate.closest('.strip').dataset.id;
    e.dataTransfer.effectAllowed = 'move';
    e.dataTransfer.setData('text/plain', dragId);
    strips.get(dragId)?.el.classList.add('dragging');
    closePop(); closeColorPop();
  });
  stage.addEventListener('dragover', (e) => {
    if (!dragId) return;
    const s = e.target.closest('#stage-others .strip'); if (!s || s.dataset.id === dragId) return;
    e.preventDefault();
    const all = $$('#stage-others .strip');
    const after = all.indexOf(strips.get(dragId)?.el) < all.indexOf(s);
    if (!s.classList.contains(after ? 'drop-after' : 'drop-before')) { clearDrop(); s.classList.add(after ? 'drop-after' : 'drop-before'); }
  });
  stage.addEventListener('drop', (e) => {
    if (!dragId) return;
    e.preventDefault();
    const s = e.target.closest('#stage-others .strip'), el = strips.get(dragId)?.el;
    if (s && el && s !== el) { s.classList.contains('drop-after') ? s.after(el) : s.before(el); saveOrder(); }
    clearDrop();
  });
  stage.addEventListener('dragend', () => { strips.get(dragId)?.el.classList.remove('dragging'); dragId = null; clearDrop(); });

  /* ── backend events ── */
  backend.on('peer-joined', ({ peer, silent }) => { if (room) { addPeer(peer, !silent); adviseQuality(); } });
  backend.on('peer-left', ({ id }) => removePeer(id));
  // the host put us out
  backend.on('kicked', () => {
    if (!room) return;
    leaveRoom(); // (put out: it would not let us back in, so not offered again)
    toast(tr('card.host_removed_room'), { kind: 'warn' });
  });
  backend.on('stats', ({ peers: list }) => {
    // settings screen: the real buffer as the device runs it
    noteWaitingPlugin();
    if ($('#modal-settings').classList.contains('open')) { syncBuffer(backend.getSettings()); syncPacket(backend.getSettings()); syncPluginStatus(); }
    // the home screen's status line follows the plugin as it connects / goes away
    if (!room) { renderHomeStatus(); return; }
    noteFeedback(!!backend._feedback);
    syncDawChip();
    // our own buffers as the device really runs (changes with the ASIO panel)
    const self = strips.get('self');
    if (self) put($('[data-f=local]', self.el), f1(backend.localMs()) + 'ms');
    list.forEach(updatePeer);
    updateDim();
    if (popFor) updatePop();
  });
  const setMeterEl = $('#set-meter'), settingsModal = $('#modal-settings');
  // the meters are asked for only while some are on screen: a room, or the settings
  backend.levelsWanted = () => !!room || settingsModal.classList.contains('open');
  backend.on('levels', (lv) => {
    // nobody is looking (minimized): nothing to draw
    if (document.hidden) return;
    if (room) {
      setMeter(strips.get('self')?.meter, lv.self);
      for (const id in lv.peers) {
        const st = strips.get(id); if (!st) continue;
        setMeter(st.meter, lv.peers[id]);
      }
      if (lv.master) setMeter($('#master-meter'), lv.master);
    }
    if (settingsModal.classList.contains('open')) {
      meterBar(setMeterEl.children[0], Math.max(lv.self[0], lv.self[1]));
      // by channel number
      (lv.inputs || []).forEach((v, i) => { const bar = chBars.get(i + 1); if (bar) meterBar(bar, v); });
    }
  });
  backend.on('chat', (m) => addMsg(m));
  // the app's own messages (host changed, someone dropped, code turned off…): always in the
  // chat log, and as a toast when the chat panel is closed so they are not missed
  backend.on('notice', ({ text }) => {
    if (room) sysMsg(text);
    if (!room || !chatOpen()) toast(text);
  });
  // room-level changes (e.g. the host left and someone else became host)
  backend.on('room', (r) => {
    if (!room) return;
    room.isHost = r.isHost;
    room.safetyCode = r.safetyCode;
    // code, on/off and size can change (host settings, a new host, a new code)
    if (r.code && r.code !== room.code) {
      room.code = r.code;
      $('#stage-others .invite-slot')?.remove();
    }
    if (r.open !== undefined) room.open = r.open;
    // the room's own time, as the host tells it (the clock reads it every second)
    if (r.startedAt) room.startedAt = r.startedAt;
    if (r.maxPeers) room.maxPeers = r.maxPeers;
    if (r.inviteUrl) room.inviteUrl = r.inviteUrl;
    if ($('#modal-invite').classList.contains('open')) fillRoomPanel();
    renderSafety();
    r.peers.forEach((p) => {
      const prev = peers.get(p.id); if (!prev) return;
      prev.isHost = p.isHost; prev.name = p.name; prev.instrument = p.instrument;
      updatePeer(prev);
    });
    updateRoomChrome();
  });
  backend.on('settings', () => { syncSelfControls(); syncSettings(); renderHomeStatus(); syncHomeProfile(); applyTheme(); syncIdleEngine(false); });

  /* ── latency popover ── */
  const pop = $('#lat-pop');
  let popFor = null, popAnchor = null, kickTimer = 0;

  function openPop(id, anchor) {
    closePop(); closeColorPop();
    popFor = id; popAnchor = anchor;
    anchor.classList.add('active');
    const p = peers.get(id);
    pop.style.setProperty('--tc', colorOf(id));
    pop.innerHTML = `
      <div class="pop-head"><span class="bar"></span><div class="who"><b>${esc(p.name)}</b><span data-f="sub"></span></div><span class="spacer"></span><button class="ibtn sm bare" data-pop="close" title="${esc(tr('room.close'))}" aria-label="${esc(tr('room.close'))}">${icon('x')}</button></div>
      <div class="pop-total"><span class="n" data-f="total"></span><span class="u">ms</span><span class="lat-q" data-f="q"></span></div>
      <div class="lat-bar"><div class="lat-fill" data-f="bar"></div></div>
      <div class="legend" data-f="legend"></div>
      <div class="pop-sep"></div>
      <dl class="kv">
        <dt>${tr('latency.path')}</dt><dd data-f="path"></dd>
        <dt>${tr('latency.link')}</dt><dd data-f="link"></dd>
        <dt>${tr('card.sending')}</dt><dd data-f="stream"></dd>
        <dt>${tr('latency.security')}</dt><dd data-f="sec"></dd>
        <dt>${tr('latency.round_trip_rtt')}</dt><dd data-f="rtt" class="num"></dd>
        <dt>${tr('latency.packet_loss')}</dt><dd data-f="loss" class="num"></dd>
      </dl>
      <div class="tip" data-f="tip" hidden></div>
      <div class="pop-sep"></div>
      <div class="pop-row">
        <div><div class="jb-title">${tr('settings.jitter_buffer')}</div><div class="note" data-f="jb-note"></div></div>
        <div class="stepper">
          <button class="ibtn sm" data-pop="dec" title="${esc(tr('dialog.fewer'))}" aria-label="${esc(tr('dialog.fewer'))}">${icon('minus')}</button>
          <span class="num" data-f="jb"></span>
          <button class="ibtn sm" data-pop="inc" title="${esc(tr('dialog.more'))}" aria-label="${esc(tr('dialog.more'))}">${icon('plus')}</button>
          <button class="btn sm chip" data-pop="auto">${tr('latency.auto')}</button>
        </div>
      </div>
      <div data-f="kick" hidden>
        <div class="pop-sep" style="margin-bottom:12px"></div>
        <div class="pop-row"><span class="note">${esc(tr('card.removed_cant_return'))}</span><button class="btn sm danger" data-pop="kick">${esc(tr('card.remove'))}</button></div>
      </div>`;
    pop.classList.add('open');
    updatePop();
    placePop();
  }
  function placePop() {
    if (!popAnchor) return;
    const r = popAnchor.getBoundingClientRect(), pw = pop.offsetWidth, ph = pop.offsetHeight;
    let left = r.left + r.width / 2 - pw / 2, top = r.bottom + 8;
    if (top + ph > innerHeight - 12) {
      // not enough room below → open beside the strip so it never covers its own one
      const s = popAnchor.closest('.strip').getBoundingClientRect();
      top = s.top;
      left = s.right + 10 + pw <= innerWidth - 12 ? s.right + 10 : s.left - pw - 10;
    }
    pop.style.left = clamp(left, 12, innerWidth - pw - 12) + 'px';
    pop.style.top = clamp(top, 12, innerHeight - ph - 12) + 'px';
  }
  /** "24bit · 48 kHz · 스테레오 · 패킷 64": what someone sends (as their packets say) */
  function streamText(st) {
    if (!st || !st.bits) return '';
    const khz = st.sampleRate ? `${st.sampleRate % 1000 ? (st.sampleRate / 1000).toFixed(1) : st.sampleRate / 1000} kHz` : '';
    return [`${st.bits}bit`, khz, st.channels === 1 ? tr('latency.mono') : tr('latency.stereo'), st.packetFrames ? tr('latency.packet_n', { n: st.packetFrames }) : ''].filter(Boolean).join(' · ');
  }
  /** the same for what we send ourselves, from our settings */
  function myStream() {
    const s = backend.getSettings();
    const daw = s.connection === 'daw';
    return { bits: s.quality === 'pcm16' ? 16 : 24, channels: daw || s.inputChannel.includes('-') ? 2 : 1, sampleRate: s.sampleRate, packetFrames: backend.packetFrames() };
  }
  function updatePop() {
    const p = peers.get(popFor); if (!p) return closePop();
    const b = p.breakdown, [qc, ql] = p.measured === false ? ['accent', tr('card.measuring')] : quality(p.total);
    const n = $('[data-f=total]', pop); n.textContent = f1(p.total); n.className = 'n q-' + qc;
    const q = $('[data-f=q]', pop); q.textContent = ql; q.className = 'lat-q q-' + qc;
    put($('[data-f=sub]', pop), [p.instrument, tr('latency.p2p_direct') + (p.conn.link === 'wifi' ? ' · Wi-Fi' : '')].filter(Boolean).join(' · '));
    paintBar($('[data-f=bar]', pop), b, p.total);
    $('[data-f=legend]', pop).innerHTML = SEG.filter(([k]) => b[k] > 0)
      // (a peer in DAW mode: its input is the DAW's block, without the DAW's own device)
      .map(([k, l, c]) => `<div class="legend-row"><i style="background:${c}"></i><span>${tr(k === 'inBuf' && p.stream?.daw ? 'latency.input_daw' : l)}</span><span class="v">${f1(b[k])} ms</span></div>`).join('');
    $('[data-f=path]', pop).textContent = tr('latency.p2p_direct') + (p.conn.ipv6 ? ' · IPv6' : ' · ' + tr('latency.hole_punching'));
    $('[data-f=link]', pop).innerHTML = { wifi: '<span class="q-warn">Wi-Fi</span>', wired: tr('latency.wired') }[p.conn.link] || tr('latency.unknown');
    $('[data-f=stream]', pop).textContent = streamText(p.stream) || tr('latency.checking');
    $('[data-f=sec]', pop).innerHTML = p.conn.encrypted
      ? (p.conn.cipher?.startsWith('AES') ? 'AES-256' : 'ChaCha20')
      : '<span class="q-warn">' + tr('latency.exchanging_keys') + '</span>';
    $('[data-f=rtt]', pop).textContent = p.rtt > 0 ? f1(p.rtt) + ' ms' : tr('card.measuring');
    $('[data-f=loss]', pop).textContent = f1(p.loss) + ' %';
    const tip = $('[data-f=tip]', pop);
    const tipText = p.outageMs > 0
        // the stream stopped for a moment (not loss: more buffer would not have helped)
        ? tr('latency.connection_dropped_moment', { dur: p.outageMs >= 1000 ? tr('latency.n_s', { n: f1(p.outageMs / 1000) }) : Math.round(p.outageMs) + 'ms', name: p.name })
      : p.conn.link === 'wifi' && b.jitter - (p.jitterIfWired ?? b.jitter) >= 0.5
        // compared with what the same stream would need on a wired link
        ? tr('latency.name_wi_fi', { name: p.name, ms: f1(b.jitter - p.jitterIfWired) })
        : p.conn.link === 'wifi'
        ? tr('latency.name_wi_fi_steady', { name: p.name })
        : p.loss > 1 ? tr('latency.lots_packet_loss') : '';
    tip.hidden = !tipText;
    if (tipText) tip.innerHTML = icon('alert') + `<span>${esc(tipText)}</span>`;
    $('[data-f=jb]', pop).textContent = (+p.packets).toFixed(1);
    $('[data-pop=auto]', pop).classList.toggle('on', p.jitterAuto);
    $('[data-f=jb-note]', pop).textContent = p.jitterAuto
      ? tr('latency.auto_n_packets', { n: (+p.packets).toFixed(1), ms: f1(b.jitter) })
      : tr('latency.manual_auto_would', { n: (+p.autoPackets).toFixed(1) });
    // only the host can put people out (asked once more: the button says so for a moment)
    $('[data-f=kick]', pop).hidden = !room?.isHost;
  }
  function closePop() {
    pop.classList.remove('open');
    popAnchor?.classList.remove('active');
    popFor = null; popAnchor = null;
    clearTimeout(kickTimer);
  }
  pop.addEventListener('click', (e) => {
    const b = e.target.closest('[data-pop]'); if (!b || !popFor) return;
    const p = peers.get(popFor);
    // manual sizes are whole packets
    const whole = Math.max(1, Math.round(p.packets));
    if (b.dataset.pop === 'close') return closePop();
    if (b.dataset.pop === 'auto') backend.setPeer(popFor, { jitterAuto: !p.jitterAuto, jitterPackets: whole });
    if (b.dataset.pop === 'dec') backend.setPeer(popFor, { jitterAuto: false, jitterPackets: Math.max(1, whole - 1) });
    if (b.dataset.pop === 'inc') backend.setPeer(popFor, { jitterAuto: false, jitterPackets: whole + 1 });
    if (b.dataset.pop === 'kick') {
      if (!b.classList.contains('arm')) {
        b.classList.add('arm');
        b.textContent = tr('card.press_again_remove');
        kickTimer = setTimeout(() => { b.classList.remove('arm'); b.textContent = tr('card.remove'); }, 2500);
      } else {
        backend.kickPeer(popFor);
        closePop();
      }
    }
  });
  document.addEventListener('pointerdown', (e) => {
    if (popFor && !pop.contains(e.target) && !popAnchor?.contains(e.target)) closePop();
  });
  window.addEventListener('resize', placePop);
  stage.addEventListener('scroll', closePop);
  $('#stage-others').addEventListener('scroll', closePop);

  /* ── a track's colour (and, on my own, my name and instrument) ── */
  const colorPop = h('<div class="pop color-pop" role="dialog"></div>');
  document.body.append(colorPop);
  let colorPopFor = null;
  function closeColorPop() { colorPop.classList.remove('open'); colorPopFor = null; }
  function openColorPop(id, anchor) {
    closePop();
    colorPopFor = id;
    const me = id === 'self', s = backend.getSettings(), cur = colorOf(id);
    const title = me ? tr('card.my_track') : tr('color.track_of', { name: nameOf(id) });
    colorPop.setAttribute('aria-label', title);
    colorPop.innerHTML = `<div class="color-title">${esc(title)}</div>`
      + (me ? `<div class="me-edit">
          <label><span>${esc(tr('input.name'))}</span><input class="input" data-me="name" maxlength="12" autocomplete="off" spellcheck="false" value="${esc(DEFAULT_NAMES.includes(s.name) ? '' : s.name)}" placeholder="${esc(tr('home.pick_name'))}"></label>
          <label><span>${esc(tr('home.instrument'))}</span><input class="input" data-me="instrument" maxlength="12" autocomplete="off" spellcheck="false" value="${esc(s.instrument || '')}" placeholder="${esc(tr('home.e_g_guitar'))}"></label>
        </div><div class="hr"></div>` : '')
      + `<div class="swatches">${PALETTE.map((c, i) => `<button class="swatch${c === cur ? ' on' : ''}" type="button" data-color="${c}" style="--c:${c}" title="${esc(tr(PALETTE_NAMES[i]))}" aria-label="${esc(tr(PALETTE_NAMES[i]))}"></button>`).join('')}</div>`
      + `<p class="note faint">${esc(tr('color.only_here'))}</p>`;
    colorPop.classList.add('open');
    colorPopSize = innerWidth + 'x' + innerHeight;
    const a = anchor.getBoundingClientRect(), w = colorPop.offsetWidth, hgt = colorPop.offsetHeight;
    const thin = a.width < 40;
    const left = thin ? a.right + 8 : a.left + a.width / 2 - w / 2;
    const top = a.top - hgt - 8 > 8 ? a.top - hgt - 8 : a.bottom + 8;
    colorPop.style.left = `${clamp(left, 8, innerWidth - w - 8)}px`;
    colorPop.style.top = `${clamp(top, 8, innerHeight - hgt - 8)}px`;
  }
  colorPop.addEventListener('click', (e) => {
    const sw = e.target.closest('.swatch');
    if (sw && colorPopFor) { setColor(colorPopFor, sw.dataset.color); closeColorPop(); }
  });
  // my name and instrument, as typed (everyone in the room sees them at once)
  colorPop.addEventListener('change', (e) => {
    const k = e.target.dataset.me; if (!k) return;
    const v = e.target.value.trim();
    if (k === 'name' && !v) return;
    if (v === (backend.getSettings()[k] || '')) return;
    // (my colour goes with my name)
    const c = pickedColors[nameOf('self')];
    backend.updateSettings({ [k]: v });
    if (k === 'name' && c) setColor('self', c);
    toast(k === 'name' ? tr('input.name_changed') : tr('input.instrument_changed'), { kind: 'ok' });
  });
  colorPop.addEventListener('keydown', (e) => { if (e.key === 'Enter') e.target.blur?.(); });
  document.addEventListener('pointerdown', (e) => {
    if (colorPopFor && !colorPop.contains(e.target) && !e.target.closest?.('[data-act=color]')) closeColorPop();
  });
  // (the window made another size: the place it was opened at is gone)
  let colorPopSize = '';
  window.addEventListener('resize', () => { const s = innerWidth + 'x' + innerHeight; if (s !== colorPopSize) { colorPopSize = s; closeColorPop(); } });

  /* ── mic / master / leave ── */
  const master = $('#master-vol');
  paintRange(master);
  master.addEventListener('input', () => {
    paintRange(master);
    backend.setMaster(+master.value);
    $('#master-val').textContent = dbText(+master.value) + ' dB';
  });
  master.addEventListener('dblclick', () => { master.value = 0; master.dispatchEvent(new Event('input')); });

  let leaveArm = null;
  $('#btn-leave').addEventListener('click', () => {
    const btn = $('#btn-leave');
    const reset = () => { clearTimeout(leaveArm); leaveArm = null; btn.classList.remove('arm'); $('.leave-label', btn).textContent = tr('room.leave'); };
    if (!leaveArm) {
      btn.classList.add('arm');
      $('.leave-label', btn).textContent = tr('latency.press_again_leave');
      leaveArm = setTimeout(reset, 2500);
      return;
    }
    reset();
    // (offered again on the home screen while others are still in it; the last one out
    // leaves nothing to go back to)
    leaveRoom({ keepRecent: peers.size > 0 });
  });

  /* ── the two top-bar menus (metronome, recording) ── */
  // The main part of each button switches the thing on and off; the arrow opens its menu.
  function toggleMenu(id, force) {
    for (const other of ['metro', 'rec']) {
      const open = other === id ? (force ?? $(`#${other}-menu`).hidden) : false;
      $(`#${other}-menu`).hidden = !open;
      $(`#${other}`).classList.toggle('open', open);
      $(`#${other}-more`).setAttribute('aria-expanded', open);
    }
  }
  const closeMenus = () => toggleMenu(null);
  const menuOpen = () => !$('#metro-menu').hidden || !$('#rec-menu').hidden;
  $('#metro-more').addEventListener('click', () => { syncMetroMenu(); syncMetroVol(); toggleMenu('metro'); });
  $('#rec-more').addEventListener('click', () => { syncRecMenu(); toggleMenu('rec'); });
  document.addEventListener('pointerdown', (e) => { if (menuOpen() && !e.target.closest('.split')) closeMenus(); });
  /** marks the button of a `.seg` whose value is `v` */
  const segSet = (seg, v) => $$('button', seg).forEach((b) => b.classList.toggle('on', b.dataset.v === String(v)));

  /* ── metronome ── */
  // The click itself is played by the audio engine, lined up across the room via the
  // server clock. This only drives the little beat light, aligned to the same start time.
  let metroOn = false, bpm = 120, beats = 4, beatTimer = null, sendTimer = null;
  // bars counted in when the metronome is turned on (kept between rooms)
  let countIn = 0;
  try { countIn = clamp(+localStorage.getItem('gachi-count-in') || 0, 0, 2); } catch { /* fine without */ }
  const sigLabel = (b) => (b === 6 ? '6/8' : `${b}/4`);
  function syncMetroMenu() {
    $('#metro-on').checked = metroOn;
    $('#bpm').value = bpm;
    segSet($('#metro-sig'), beats);
    segSet($('#metro-count'), countIn);
    $('#metro-toggle').title = metroOn ? tr('metro.turn_off_metronome', { bpm, sig: sigLabel(beats) }) : tr('metro.turn_metronome_whole');
  }
  /** `counted`: bars the room is being counted in with, from `startMs` */
  function setMetro(on, { send = true, startMs = null, counted = null } = {}) {
    const turnedOn = on && !metroOn;
    metroOn = on;
    $('#metro').classList.toggle('on', on);
    syncMetroMenu();
    clearTimeout(beatTimer);
    clearTimeout(sendTimer);
    if (send) backend.setMetronome({ on, bpm, beats, countIn });
    if (!on) return;
    const origin = startMs ?? Date.now();
    const period = 60000 / bpm;
    // (the room's word when it comes; until then what we asked for, when turning it on)
    const lead = (counted ?? (turnedOn && send ? countIn : 0)) * beats;
    let last = null;
    const d = $('#metro-beat');
    // (woken at each beat, not looked at a hundred times a second; each beat's time is
    // counted from the start, so the light does not drift from the click)
    const beat = () => {
      const now = Date.now();
      const n = Math.floor((now - origin) / period);
      if (n >= 0 && n !== last) {
        last = n;
        d.classList.add('flash');
        d.classList.toggle('count', n < lead);
        d.classList.toggle('down', n >= lead && (n - lead) % beats === 0);
        setTimeout(() => d.classList.remove('flash'), 90);
      }
      beatTimer = setTimeout(beat, Math.max(1, origin + (Math.max(n, -1) + 1) * period - now));
    };
    beat();
  }
  function setBpm(v) {
    bpm = clamp(Math.round(v) || 120, 30, 300);
    $('#bpm').value = bpm;
    if (!metroOn) return;
    // wait until the user stops nudging the tempo before telling the room
    setMetro(true, { send: false });
    clearTimeout(sendTimer);
    sendTimer = setTimeout(() => backend.setMetronome({ on: true, bpm, beats, countIn }), 300);
  }
  function toggleMetro() {
    setMetro(!metroOn);
    toast(metroOn ? tr('metro.metronome_bpm_bpm', { bpm, sig: sigLabel(beats) }) + (countIn ? ' · ' + tr('metro.n_bar_count', { n: countIn }) : '') + ' · ' + tr('metro.sync_everyone') : tr('metro.metronome_off'));
  }
  $('#metro-sig').addEventListener('click', (e) => {
    const b = e.target.closest('button'); if (!b) return;
    beats = +b.dataset.v;
    syncMetroMenu();
    if (metroOn) setMetro(true); // tell the room
  });
  $('#metro-count').addEventListener('click', (e) => {
    const b = e.target.closest('button'); if (!b) return;
    countIn = +b.dataset.v;
    try { localStorage.setItem('gachi-count-in', countIn); } catch { /* fine without */ }
    syncMetroMenu();
  });
  backend.on('metronome', ({ on, bpm: b, beats: bb, startMs, byMe, byName, countIn: counted }) => {
    if (!room) return;
    bpm = Math.round(b);
    if (bb) beats = bb;
    setMetro(on, { send: false, startMs, counted: counted || 0 });
    if (!byMe && byName) sysMsg(on ? tr('metro.name_set_metronome', { name: byName, bpm, sig: sigLabel(beats) }) : tr('metro.name_turned_metronome', { name: byName }));
  });
  $('#metro-toggle').addEventListener('click', toggleMetro);
  $('#metro-on').addEventListener('change', toggleMetro);
  $('#bpm-dec').addEventListener('click', () => setBpm(bpm - 1));
  $('#bpm-inc').addEventListener('click', () => setBpm(bpm + 1));
  $('#bpm').addEventListener('change', (e) => setBpm(+e.target.value));
  $('#bpm').addEventListener('wheel', (e) => { e.preventDefault(); setBpm(bpm + (e.deltaY < 0 ? 1 : -1)); }, { passive: false });
  // the click's loudness: ours alone (the same setting as in the settings window)
  const metroVolMenu = $('#metro-vol');
  metroVolMenu.addEventListener('input', () => {
    paintRange(metroVolMenu);
    $('#metro-vol-menu-val').textContent = metroVolMenu.value + '%';
    backend.updateSettings({ metronomeVolume: +metroVolMenu.value });
  });
  // a double click puts the click back at its usual loudness
  const METRO_VOL_DEFAULT = 60;
  for (const el of [metroVolMenu, $('#set-metro-vol')]) {
    el.addEventListener('dblclick', () => { el.value = METRO_VOL_DEFAULT; el.dispatchEvent(new Event('input')); });
  }
  function syncMetroVol() {
    metroVolMenu.value = backend.getSettings().metronomeVolume ?? METRO_VOL_DEFAULT; paintRange(metroVolMenu);
    $('#metro-vol-menu-val').textContent = metroVolMenu.value + '%';
  }

  /* ── recording ── */
  const rec = { on: false, start: 0, timer: null, busy: false };
  /** what the recording's files are, in words: "WAV 16bit", "FLAC 24bit" */
  function recFormatText(s = backend.getSettings()) {
    const bits = s.recFormat === 'flac' ? Math.min(24, s.recBits || 16) : (s.recBits || 16);
    return `${s.recFormat === 'flac' ? 'FLAC' : 'WAV'} ${bits === 32 ? '32bit float' : bits + 'bit'}`;
  }
  const REC_FORMATS = [['wav', 16], ['wav', 24], ['wav', 32], ['flac', 16], ['flac', 24]];
  function syncRecMenu() {
    const s = backend.getSettings();
    const format = s.recFormat === 'flac' ? 'flac' : 'wav';
    const bits = format === 'flac' ? Math.min(24, s.recBits || 16) : (s.recBits || 16);
    $('#rec-formats').innerHTML = REC_FORMATS.map(([f, b], i) => {
      const on = f === format && b === bits;
      return (i === 3 ? '<div class="menu-sep"></div>' : '')
        + `<button class="radio-row${on ? ' on' : ''}" type="button" role="menuitemradio" aria-checked="${on}" data-f="${f}" data-b="${b}" ${rec.on ? 'disabled' : ''}><i></i><span class="grow">${f.toUpperCase()} ${b === 32 ? '32bit float' : b + 'bit'}</span>${f === 'flac' ? `<span class="note">${esc(tr('rec.lossless'))}</span>` : ''}</button>`;
    }).join('');
    $('#rec-mix').checked = s.recMix !== false;
    $('#rec-mix').disabled = rec.on;
    $('#rec-menu').classList.toggle('locked', rec.on);
    $('#rec-locked').hidden = !rec.on;
    // what an hour of one stereo track comes to (a mono one: half)
    const rate = s.sampleRate || 48000;
    const wav = rate * 2 * (bits / 8) * 3600;
    const size = (b) => (b >= 1e9 ? f1(b / 1e9) + ' GB' : Math.round(b / 1e6) + ' MB');
    $('#rec-note').textContent = format === 'flac'
      ? tr('rec.one_stereo_track', { size: size(wav * 0.55) })
      : tr('rec.one_stereo_track_about', { size: size(wav) });
    $('#rec-btn').title = rec.on ? tr('rec.stop_recording') : tr('rec.recording') + ` · ${recFormatText(s)}`;
  }
  $('#rec-formats').addEventListener('click', async (e) => {
    const b = e.target.closest('.radio-row'); if (!b || rec.on) return;
    await backend.updateSettings({ recFormat: b.dataset.f, recBits: +b.dataset.b });
    syncRecMenu();
  });
  $('#rec-mix').addEventListener('change', async (e) => { await backend.updateSettings({ recMix: e.target.checked }); syncRecMenu(); });
  $('#rec-open').addEventListener('click', () => backend.openRecordings?.().catch((e) => toast(e.message)));
  function setRecUi(on) {
    rec.on = on;
    $('#rec').classList.toggle('on', on);
    // the cards' record buttons show which tracks are being taken
    $('#stage').classList.toggle('recording', on);
    clearInterval(rec.timer);
    if (on) {
      rec.start = Date.now();
      const tick = () => { $('#rec-label').textContent = mmss(Date.now() - rec.start); };
      tick(); rec.timer = setInterval(tick, 500);
    } else {
      $('#rec-label').textContent = tr('room.record');
    }
    syncRecMenu();
  }
  function recSaved(r) {
    if (r?.error) { toast(r.error); if (room) sysMsg(tr('rec.recording_stopped_e', { e: r.error })); return; }
    if (!r || r.files == null) return;
    toast(tr('rec.recording_saved_n', { n: r.files }) + ` · ${recFormatText()} · ${mmss(r.seconds * 1000)}`);
    if (!room) return;
    const el = h(`<div class="msg sys">${esc(tr('rec.recording_saved_n_files', { n: r.files, dir: r.dir || '' }))} <button class="link-btn" type="button">${tr('rec.open_folder')}</button></div>`);
    $('button', el).addEventListener('click', () => backend.openRecordings?.().catch((e) => toast(e.message)));
    pushMsg(el);
    if (r.dropped) sysMsg(tr('rec.disk_too_slow', { n: r.dropped }));
  }
  async function toggleRec() {
    if (rec.busy) return;
    const want = !rec.on;
    // the tracks picked on the cards (all of them unless some were switched off)
    const picked = (backend.self.rec !== false ? 1 : 0) + [...peers.values()].filter((p) => p.rec !== false).length;
    if (want && !picked) { toast(tr('rec.no_one_record')); return; }
    rec.busy = true;
    try {
      const r = await backend.setRecording(want);
      setRecUi(want);
      if (want) toast(tr('rec.recording') + ` · ${recFormatText()} · ` + (picked === peers.size + 1 ? tr('rec.separate_track_each') : tr('rec.only_n_picked', { n: picked })));
      else recSaved(r);
    } catch (e) {
      toast(e.message);
      if (!want) setRecUi(false);
    } finally {
      rec.busy = false;
    }
  }
  // the recording ended on its own: the engine restarted (settings changed), or the files
  // could not be written (a full disk)
  backend.on('recording', (r) => { setRecUi(false); recSaved(r); });
  $('#rec-btn').addEventListener('click', toggleRec);

  /* ── chat ── */
  let unread = 0;
  const chatList = $('#chat-list');
  const chatOpen = () => $('#screen-room').classList.contains('chat-open');
  function updateBadge() { const b = $('#chat-badge'); b.hidden = !unread; b.textContent = unread > 9 ? '9+' : unread; }
  function toggleChat(force) {
    const open = $('#screen-room').classList.toggle('chat-open', force);
    $('#btn-chat').classList.toggle('on', open);
    if (open) { unread = 0; updateBadge(); $('#chat-input').focus(); chatList.scrollTop = chatList.scrollHeight; }
  }
  const CHAT_KEEP = 500;
  function pushMsg(el) {
    const atBottom = chatList.scrollHeight - chatList.scrollTop - chatList.clientHeight < 40;
    chatList.append(el);
    // the latest only: a room open all day (or someone's app sending chat as fast as it is
    // taken) would otherwise grow the page without end
    while (chatList.childElementCount > CHAT_KEEP) chatList.firstElementChild.remove();
    if (atBottom) chatList.scrollTop = chatList.scrollHeight;
  }
  /** What we send to one person, bits per second: payload + audio header (12) + encryption
   *  (28) + IP/UDP (28) per packet. `bits`: 16 or 24 (default: the setting). */
  function streamBps(bits) {
    const s = backend.getSettings();
    const me = myStream();
    const frames = me.packetFrames || 64;
    return (frames * me.channels * ((bits || me.bits) / 8) + 68) * 8 * (s.sampleRate / frames);
  }
  /** A big room (9 people and up) while we send 24-bit: say once what it takes and that
   *  16-bit takes less. Nothing is changed for the user. */
  const BIG_ROOM = 9;
  let qualityAdvised = false;
  function adviseQuality() {
    if (!room || qualityAdvised) return;
    const others = peers.size;
    if (others + 1 < BIG_ROOM || myStream().bits !== 24) return;
    qualityAdvised = true;
    const mbps = (bits) => f1((streamBps(bits) * others) / 1e6);
    const text = tr('quality.n_people_room', { n: others + 1, a: mbps(24), b: mbps(16) });
    sysMsg(text);
    if (!chatOpen()) toast(text);
  }
  function sysMsg(text) { pushMsg(h(`<div class="msg sys">${esc(text)}</div>`)); }
  function addMsg({ from, text }) {
    if (from === 'me') { pushMsg(h(`<div class="msg me" data-from="self" style="--tc:${colorOf('self')}"><span class="from">${esc(backend.getSettings().name)}</span><div class="bubble">${esc(text)}</div></div>`)); return; }
    const p = peers.get(from);
    pushMsg(h(`<div class="msg" data-from="${esc(from)}" style="--tc:${p ? colorOf(from) : 'var(--text-2)'}"><span class="from">${esc(p?.name ?? '?')}</span><div class="bubble">${esc(text)}</div></div>`));
    if (!chatOpen()) { unread++; updateBadge(); }
  }
  $('#btn-chat').addEventListener('click', () => toggleChat());
  $('#btn-chat-close').addEventListener('click', () => toggleChat(false));
  // (a narrow window: the chat lies over the room; a click beside it closes it)
  $('#chat-veil').addEventListener('click', () => toggleChat(false));
  $('#chat-form').addEventListener('submit', (e) => {
    e.preventDefault();
    const inp = $('#chat-input'), text = inp.value.trim();
    if (!text) return;
    addMsg({ from: 'me', text }); backend.sendChat(text); inp.value = '';
  });

  /* ═══════════════════════ INVITE ═══════════════════════ */
  // https link (works in any messenger) when the app knows its server, else the app link
  const inviteLink = (c) => (room?.inviteUrl && room.code === c ? room.inviteUrl : `gachi://join/${fmtCode(c)}`);
  const roomPanel = $('#modal-invite');
  let panelCode = null;
  function openInvite() {
    if (!room) return;
    panelCode = null;
    fillRoomPanel();
    openModal('modal-invite');
  }
  /** the room panel: code, link and QR (when the code is on), code on/off, size, new code */
  function fillRoomPanel() {
    if (!room) return;
    const open = room.open !== false, c = room.code, count = peers.size + 1;
    $('#invite-open').hidden = $('#copy-row').hidden = !open;
    $('#invite-closed').hidden = open;
    $('#invite-closed').textContent = room.isHost
      ? tr('invite.room_code_off')
      : tr('invite.host_turned_room');
    if (open && panelCode !== c) {
      panelCode = c;
      $('#invite-code').textContent = fmtCode(c);
      $('#invite-link').textContent = inviteLink(c);
      const qr = $('#qr'); qr.innerHTML = '';
      // the app draws the QR itself; the browser preview falls back to qrcode.js
      Promise.resolve(backend.qrSvg?.(inviteLink(c))).catch(() => null).then((svg) => {
        if (svg) qr.innerHTML = svg;
        else if (window.QRCode) new window.QRCode(qr, { text: inviteLink(c), width: 128, height: 128, colorDark: '#111113', colorLight: '#ffffff' });
      });
    }
    roomPanel.classList.toggle('guest', !room.isHost);
    $('#rm-guest').hidden = !!room.isHost;
    $('#rm-open').checked = open;
    $('#rm-code-note').textContent = open ? tr('invite.anyone_code_join') : tr('invite.off_no_one');
    $('#rm-size').textContent = tr('invite.n', { n: room.maxPeers });
    $('#rm-size-note').textContent = tr('invite.n_here_now', { n: count }) + ' · ' + (count >= room.maxPeers ? tr('invite.full') : tr('invite.n_seats_left', { n: room.maxPeers - count }));
    $('#rm-dec').disabled = !room.isHost || room.maxPeers <= Math.max(2, count);
    $('#rm-inc').disabled = !room.isHost || room.maxPeers >= (room.maxCap || 16);
    $('#rm-new').disabled = !room.isHost || !open;
    $('#rm-open').disabled = !room.isHost;
    renderSafety();
  }
  async function setRoom(patch) {
    try {
      await backend.setRoom(patch);
      if (patch.open === false) toast(tr('invite.room_code_off_no'));
      if (patch.open === true) toast(tr('invite.room_code'));
      if (patch.newCode) toast(tr('invite.new_code_made'));
    } catch (e) {
      toast(e.message);
    }
    fillRoomPanel();
  }
  $('#rm-open').addEventListener('change', (e) => setRoom({ open: e.target.checked }));
  $('#rm-dec').addEventListener('click', () => setRoom({ maxPeers: room.maxPeers - 1 }));
  $('#rm-inc').addEventListener('click', () => setRoom({ maxPeers: room.maxPeers + 1 }));
  $('#rm-new').addEventListener('click', () => setRoom({ newCode: true }));
  // one code for the whole room, from everyone's public key
  function renderSafety() {
    const el = $('#safety-code');
    const code = room?.safetyCode;
    el.textContent = code || tr('invite.shows_once_connected');
    el.classList.toggle('pending', !code);
  }
  $('#btn-room').addEventListener('click', openInvite);
  $('#copy-code').addEventListener('click', () => copy(fmtCode(room.code), tr('invite.code_copied')));
  $('#copy-link').addEventListener('click', () => copy(inviteLink(room.code), tr('invite.link_copied')));
  $('#copy-msg').addEventListener('click', () => copy(
    tr('invite.gachi_join_my', { name: room.name, code: fmtCode(room.code), link: inviteLink(room.code) }),
    tr('invite.invite_copied_paste')));

  /* ═══════════════════════ MODALS ═══════════════════════ */
  function openModal(id) { closePop(); $('#' + id).classList.add('open'); }
  function closeModal(el) {
    el.classList.remove('open');
    if (el.id === 'modal-settings') syncIdleEngine();
  }

  /** Outside a room the audio engine runs only while settings are open (it holds the ASIO
   *  device), except in DAW mode: there it holds no device, and it is what the gachi plugin
   *  connects to, so it stays up on the home screen too (and the plugin's state shows). */
  let idleAsked = null;
  /** `again`: ask even if the same was asked last time (a room just left: its engine is
   *  gone). Every setting changed comes through here, a slider many times a second. */
  function syncIdleEngine(again = true) {
    if (room) { idleAsked = null; return; }
    const want = backend.getSettings().connection === 'daw' || $('#modal-settings').classList.contains('open');
    if (!again && want === idleAsked) return;
    idleAsked = want;
    Promise.resolve(backend.setPreview?.(want)).catch((e) => { idleAsked = null; if (want) toast(e.message); });
  }
  /** Home screen: the device is closed there, so what its control panel changed (buffer,
   *  safe mode) is read anew when the window comes back (the panel is another window) and
   *  at start: the app opens it for a moment. At most every 4 s. */
  let lastProbe = 0;
  function probeDevice() {
    if (room || $('#modal-settings').classList.contains('open') || backend.getSettings().connection === 'daw') return;
    if (Date.now() - lastProbe < 4000) return;
    lastProbe = Date.now();
    Promise.resolve(backend.probeDevice?.()).catch(() => {});
  }
  window.addEventListener('focus', probeDevice);
  document.addEventListener('visibilitychange', () => { if (!document.hidden) probeDevice(); });
  // no animations while nothing is drawn (see `.page-hidden` in the styles; no meters are
  // asked for then either: see `backend.levelsWanted`)
  const syncHidden = () => {
    document.documentElement.classList.toggle('page-hidden', document.hidden);
  };
  document.addEventListener('visibilitychange', syncHidden);
  syncHidden();
  $$('.modal-backdrop').forEach((bd) => {
    bd.addEventListener('pointerdown', (e) => { if (e.target === bd) closeModal(bd); });
    $$('[data-close]', bd).forEach((b) => b.addEventListener('click', () => closeModal(bd)));
  });

  /* ── input routing: every channel, pair, and mic + pair (e.g. loopback) ── */
  function inputCount(s) {
    const dev = backend.getDevices().find((d) => d.id === s.device);
    return Math.max(2, Math.min(32, dev?.inputs || 2));
  }
  /** the driver's channel names (ASIO), e.g. ['Analogue 1', …, 'Loopback 6'] */
  function inputNames(s) {
    return backend.getDevices().find((d) => d.id === s.device)?.inputNames || [];
  }
  const chName = (names, c) => names[c - 1] || tr('input.input_n', { n: c });
  /** "Loopback 1" + "Loopback 2" → "Loopback 1/2" */
  function pairName(names, c) {
    const a = names[c - 1], b = names[c];
    if (!a || !b) return tr('input.input_n', { n: `${c}/${c + 1}` });
    const ma = a.match(/^(.*?)(\d+)\s*([LR]?)$/i), mb = b.match(/^(.*?)(\d+)\s*([LR]?)$/i);
    if (ma && mb && ma[1] === mb[1]) return ma[3] && mb[3] ? `${ma[1]}${ma[2]}` : `${ma[1]}${ma[2]}/${mb[2]}`;
    return `${a} / ${b}`;
  }
  function inputOptions(n, names = []) {
    const mono = [], pairs = [], mixes = [];
    for (let c = 1; c <= n; c++) mono.push([`${c}`, `${chName(names, c)} (${tr('latency.mono')})`]);
    for (let c = 1; c + 1 <= n; c += 2) pairs.push([`${c}-${c + 1}`, `${pairName(names, c)} (${tr('latency.stereo')})`]);
    // a mic on 1 or 2, plus any later stereo pair (an interface's loopback carries PC audio)
    for (const m of [1, 2]) for (let c = 3; c + 1 <= n; c += 2) mixes.push([`${m}+${c}-${c + 1}`, `${chName(names, m)} + ${pairName(names, c)}`]);
    return [[tr('latency.mono'), mono], [tr('latency.stereo'), pairs], [tr('input.mic_stereo_send'), mixes]];
  }
  function fillInputSelect(sel, s) {
    const n = inputCount(s);
    const names = inputNames(s);
    const key = `${s.device}|${n}|${names.join(',')}`;
    if (sel.dataset.key !== key) {
      sel.dataset.key = key;
      sel.innerHTML = inputOptions(n, names).filter(([, list]) => list.length)
        .map(([label, list]) => `<optgroup label="${esc(label)}">${list.map(([v, l]) => `<option value="${v}">${esc(l)}</option>`).join('')}</optgroup>`)
        .join('');
    }
    sel.value = s.inputChannel;
    if (sel.value !== s.inputChannel) {
      // a saved routing the device doesn't have (e.g. switched to a 2-channel device)
      const extra = new Option(tr('input.input_n_not', { n: s.inputChannel }), s.inputChannel);
      sel.add(extra); sel.value = s.inputChannel;
    }
  }
  /**
   * Meters grouped the way the driver names its channels: "Analogue 1", "Analogue 2",
   * "Loopback 1"… → [Analogue: 1 2] [Loopback: 1 …]. Works for any interface; a driver
   * without names gives one group numbered 1…n.
   */
  function channelGroups(names, n) {
    const groups = [];
    for (let c = 1; c <= n; c++) {
      const name = names[c - 1] || '';
      const m = name.match(/^(.*?)[\s_-]*(\d+\s*[LR]?|[LR])$/i);
      const base = (m ? m[1] : name).trim();
      const label = m ? m[2].replace(/\s+/g, '') : String(c);
      const last = groups[groups.length - 1];
      if (last && last.name === base) last.chans.push([c, label]);
      else groups.push({ name: base, chans: [[c, label]] });
    }
    return groups;
  }
  function usedChannels(v) {
    return new Set(v.split(/[+-]/).map((x) => +x).filter((x) => x > 0));
  }
  /** channel number → its bar (rebuilt with the meters) */
  const chBars = new Map();
  function buildChannelMeters(s) {
    const n = inputCount(s);
    const box = $('#ch-meters');
    box.parentElement.hidden = $('#ch-note').hidden = $('#ch-k').hidden = n <= 2;
    const names = inputNames(s);
    const key = `${n}|${names.join(',')}`;
    if (box.dataset.n !== key) {
      box.dataset.n = key;
      box.innerHTML = Array.from({ length: n }, (_, i) => `<div class="ch-row" data-c="${i + 1}"><span class="n" title="${esc(chName(names, i + 1))}">${esc(chName(names, i + 1))}</span><div class="bar"><i></i></div></div>`).join('');
    }
    const used = usedChannels(s.inputChannel);
    $$('.ch-row', box).forEach((el) => el.classList.toggle('used', used.has(+el.dataset.c)));
    chBars.clear();
    $$('.ch-row', box).forEach((el) => chBars.set(+el.dataset.c, $('i', el)));
  }

  /* ── theme: one button flips light/dark ("system" until first flipped) ── */
  const prefersDark = matchMedia('(prefers-color-scheme: dark)');
  const effectiveTheme = () => {
    const t = backend.getSettings().theme;
    return t === 'light' || t === 'dark' ? t : prefersDark.matches ? 'dark' : 'light';
  };
  let themeShown = '';
  function applyTheme() {
    const t = backend.getSettings().theme;
    // (every setting changed comes through here: nothing to do while the look stays)
    const look = t + '|' + effectiveTheme();
    if (look === themeShown) return;
    themeShown = look;
    if (t === 'light' || t === 'dark') document.documentElement.dataset.theme = t;
    else delete document.documentElement.dataset.theme;
    try { localStorage.setItem('gachi-theme', t); } catch { /* fine without */ }
    // the button shows what you switch to
    $$('[data-act=theme]').forEach((b) => { b.innerHTML = icon(effectiveTheme() === 'dark' ? 'sun' : 'moon'); });
    $$('#set-theme button').forEach((b) => b.classList.toggle('on', b.dataset.v === effectiveTheme()));
  }
  $('#set-theme').addEventListener('click', (e) => {
    const b = e.target.closest('button');
    if (b && b.dataset.v !== effectiveTheme()) backend.updateSettings({ theme: b.dataset.v });
  });
  $$('[data-act=theme]').forEach((b) => b.addEventListener('click', () => {
    const flip = () => backend.updateSettings({ theme: effectiveTheme() === 'dark' ? 'light' : 'dark' });
    const calm = matchMedia('(prefers-reduced-motion: reduce)').matches;
    // a soft cross-fade from the old look to the new one (instant where unsupported)
    if (document.startViewTransition && !calm) document.startViewTransition(flip);
    else flip();
  }));
  prefersDark.addEventListener('change', applyTheme);

  /* ── home: my name and instrument ── */
  const homeName = $('#home-name');
  const homeInst = $('#home-inst');
  let homeFocused = false;
  let nameTimer = null;
  function syncHomeProfile() {
    const s = backend.getSettings();
    const first = !s.name || DEFAULT_NAMES.includes(s.name);
    if (document.activeElement !== homeName) homeName.value = first ? '' : s.name;
    $('#me-row').classList.toggle('first', first && !homeName.value.trim());
    $('#me-row').classList.toggle('loading', !backend.ready);
    put($('#me-hint'), first && !homeName.value.trim() ? tr('home.name_needed') : '');
    // first run: put the cursor in the name box (once the saved name is known)
    if (backend.ready && !homeFocused) {
      homeFocused = true;
      if (first && $('#screen-home').classList.contains('active')) homeName.focus();
    }
    if (document.activeElement !== homeInst) homeInst.value = s.instrument || '';
    if (!$('#create-name').value) $('#create-name').placeholder = first ? tr('home.e_g_friday') : tr('home.names_room', { name: s.name });
  }
  homeName.addEventListener('input', () => {
    clearTimeout(nameTimer);
    const v = homeName.value.trim();
    $('#me-row').classList.toggle('first', !v);
    put($('#me-hint'), v ? '' : tr('home.name_needed'));
    nameTimer = setTimeout(() => { if (v) backend.updateSettings({ name: v }); }, 400);
  });
  homeName.addEventListener('blur', () => { if (!homeName.value.trim()) syncHomeProfile(); });
  // the instrument: whatever the player writes (a list could never hold them all)
  let instTimer = null;
  homeInst.addEventListener('input', () => {
    clearTimeout(instTimer);
    instTimer = setTimeout(() => backend.updateSettings({ instrument: homeInst.value.trim() }), 400);
  });

  /* ── home: rooms open on the same network ── */
  /** Looked up when the home screen comes up (the app starts, a room is left) and when the
   *  refresh button is pressed, not on a timer: each lookup is a call on the connection
   *  server's free daily allowance, and an app left open on the home screen used thousands
   *  a day. */
  let nearbyBusy = false;
  async function refreshNearby() {
    if (nearbyBusy) return;
    nearbyBusy = true;
    const btn = $('#nearby-refresh');
    btn.disabled = true;
    btn.classList.add('busy');
    let rooms = [];
    try { rooms = await backend.nearbyRooms(); } catch { /* offline: just hide the list */ }
    renderNearby(rooms || []);
    btn.classList.remove('busy');
    // (the server takes 10 lookups a minute from one address)
    setTimeout(() => { nearbyBusy = false; btn.disabled = false; }, 2000);
  }
  $('#nearby-refresh').addEventListener('click', refreshNearby);
  function renderNearby(rooms) {
    // (the room left last is shown above it already)
    lastNearby = rooms;
    rooms = rooms.filter((r) => r.code !== recent?.code);
    $('#nearby-empty').hidden = rooms.length > 0;
    $('#nearby-list').innerHTML = rooms.map((r) => {
      const full = r.members >= r.max_peers;
      return `<button class="nearby-room" data-code="${esc(r.code)}" ${full ? 'disabled' : ''} title="${esc(r.host)}">
        <span class="nr-name">${esc(r.name || tr('home.unnamed_room'))}</span>
        ${r.locked ? '<span class="nr-lock" title="' + esc(tr('home.password')) + '">' + icon('lock') + '</span>' : ''}
        <span class="nr-count">${r.members}/${r.max_peers}${full ? ' · ' + esc(tr('invite.full')) : ''}</span>
        ${full ? '' : `<span class="nr-go">${tr('home.join_2')}</span>`}
      </button>`;
    }).join('');
    hydrateIcons($('#nearby-list'));
  }
  $('#nearby-list').addEventListener('click', (e) => {
    const b = e.target.closest('.nearby-room');
    if (b && !b.disabled) join(b.dataset.code);
  });

  /* ── the room left last: one click back in, while the app runs. Kept in memory only
     (code, name, the password it was entered with): closing the app forgets it ── */
  const ago = (ms) => {
    const m = Math.floor(ms / 60000);
    return m < 1 ? tr('home.just_now') : m < 60 ? tr('home.n_min_ago', { n: m }) : tr('home.n_h_ago', { n: Math.floor(m / 60) });
  };
  function renderRecent() {
    const box = $('#recent');
    clearInterval(recentTimer);
    box.hidden = !recent;
    if (!recent) { renderNearby(lastNearby); return; }
    box.title = tr('home.last_room') + (recent.pw ? ' · ' + tr('home.password_room_rejoins') : '');
    box.innerHTML = `${icon('clock')}
      <span class="rb-text">
        <span class="rb-name">${esc(recent.name || tr('home.unnamed_room'))}</span>
        <span class="rb-code mono">${fmtCode(recent.code)}</span>
        <span class="rb-ago">${tr('home.last_room')} · ${ago(Date.now() - recent.at)}</span>
      </span>
      ${recent.pw ? '<span class="nr-lock">' + icon('lock') + '</span>' : ''}
      <span class="btn primary sm rb-go">${tr('home.rejoin')}</span>`;
    // ("방금" → "3분 전")
    recentTimer = setInterval(() => { if (recent && !room) renderRecent(); }, 30_000);
    renderNearby(lastNearby);
  }
  $('#recent').addEventListener('click', () => { if (recent) join(recent.code, recent.pw || undefined); });

  /* ═══════════════════════ SETTINGS ═══════════════════════ */

  backend.appVersion?.().then((v) => { $('#about-version').textContent = v; $('#update-from').textContent = v; }).catch(() => {});

  /* ── a newer version: asked once at startup (if the setting allows) and from the about
     page. The app only tells; the download page opens in the browser. ── */
  let updateAsked = false;
  function showUpdate(u) {
    $('#update-pill').hidden = !u;
    if (!u) return;
    $('#update-pill-v').textContent = u.version;
    $('#update-to').textContent = u.version;
  }
  async function checkUpdate(byHand) {
    const out = $('#update-result'), btn = $('#update-check');
    if (byHand) { btn.disabled = true; out.hidden = false; out.textContent = tr('settings.checking'); }
    try {
      const u = await backend.checkUpdate();
      showUpdate(u);
      if (byHand) {
        out.textContent = u ? tr('settings.version_v_out', { v: u.version }) : tr('settings.have_latest_version');
        if (u) openModal('modal-update');
      }
    } catch (e) {
      // at startup: say nothing (offline is no news)
      if (byHand) out.textContent = String(e?.message || e || tr('settings.couldnt_check'));
    } finally {
      if (byHand) btn.disabled = false;
    }
  }
  function checkUpdateAtStart() {
    if (updateAsked || !backend.ready) return;
    updateAsked = true;
    if (backend.getSettings().checkUpdates !== false) checkUpdate(false);
  }
  backend.on('settings', checkUpdateAtStart);
  checkUpdateAtStart();
  $('#update-pill').addEventListener('click', () => openModal('modal-update'));
  $('#update-check').addEventListener('click', () => checkUpdate(true));
  $('#update-open').addEventListener('click', () => { backend.openRelease(); closeModal($('#modal-update')); });
  $('#set-updates').addEventListener('change', (e) => backend.updateSettings({ checkUpdates: e.target.checked }));
  $('#set-language').addEventListener('change', (e) => backend.updateSettings({ language: e.target.value }));
  function openSettings(tab = 'audio') {
    selectTab(tab);
    syncSettings();
    openModal('modal-settings');
    // outside a room, open the audio device so the input meter and test tone work
    if (!room) { idleAsked = true; Promise.resolve(backend.setPreview?.(true)).catch((e) => { idleAsked = null; toast(e.message); }); }
  }
  const metroVol = $('#set-metro-vol');
  metroVol.addEventListener('input', () => {
    paintRange(metroVol);
    $('#metro-vol-val').textContent = metroVol.value + '%';
    backend.updateSettings({ metronomeVolume: +metroVol.value });
  });
  $$('[data-open=settings]').forEach((b) => b.addEventListener('click', () => openSettings()));

  function selectTab(tab) {
    $$('#settings-tabs button').forEach((b) => b.classList.toggle('on', b.dataset.tab === tab));
    $$('.tab-panel').forEach((p) => p.classList.toggle('on', p.dataset.panel === tab));
    if (tab === 'network' && !netInfo && !netChecking) runNetCheck();
  }
  $('#settings-tabs').addEventListener('click', (e) => { const b = e.target.closest('button'); if (b) selectTab(b.dataset.tab); });

  $$('#modal-settings .seg[data-key]').forEach((seg) => seg.addEventListener('click', (e) => {
    const b = e.target.closest('button'); if (!b) return;
    const v = 'num' in seg.dataset ? +b.dataset.v : b.dataset.v;
    backend.updateSettings({ [seg.dataset.key]: v });
  }));
  $('#set-device').addEventListener('change', (e) => backend.updateSettings({ device: e.target.value }));
  $('#open-panel').addEventListener('click', () => {
    Promise.resolve(backend.openControlPanel?.()).catch((e) => toast(e.message));
  });
  $('#set-input').addEventListener('change', (e) => backend.updateSettings({ inputChannel: e.target.value }));
  const jbRange = $('[data-key-range=jitterPackets]');
  jbRange.addEventListener('input', () => { paintRange(jbRange); backend.updateSettings({ jitterPackets: +jbRange.value }); });
  $('#set-nearby').addEventListener('change', (e) => backend.updateSettings({ showNearby: e.target.checked }));
  // the fixed port: a number from 1024 to 65535, else back to what it was
  $('#set-fixed-port').addEventListener('change', (e) => {
    backend.updateSettings({ fixedPort: e.target.checked });
    $('#set-port').hidden = !e.target.checked;
  });
  // digits only, never over 65535 (the last port there is) while typing; below 1024 (the
  // system's) is refused when the box is left
  const PORT_MIN = 1024, PORT_MAX = 65535;
  $('#set-port').addEventListener('input', (e) => {
    let d = e.target.value.replace(/\D/g, '').slice(0, 5);
    if (d && +d > PORT_MAX) d = String(PORT_MAX);
    if (d !== e.target.value) e.target.value = d;
  });
  $('#set-port').addEventListener('change', (e) => {
    const v = +e.target.value;
    if (e.target.value && v >= PORT_MIN && v <= PORT_MAX) backend.updateSettings({ port: v });
    else { e.target.value = backend.getSettings().port ?? 24620; toast(tr('settings.ports_go_1024')); }
  });
  $('#set-port').addEventListener('keydown', (e) => { if (e.key === 'Enter') e.target.blur(); });
  /* GPU acceleration: read by the window as it starts, so a change waits for a restart */
  const gpuAtStart = backend.getSettings().gpu !== false;
  function syncGpuRestart() {
    $('#gpu-restart').hidden = (backend.getSettings().gpu !== false) === gpuAtStart;
  }
  $('#set-gpu').addEventListener('change', (e) => { backend.updateSettings({ gpu: e.target.checked }); syncGpuRestart(); });
  $('#set-verbose').addEventListener('change', (e) => backend.updateSettings({ verboseLog: e.target.checked }));
  $('#open-logs').addEventListener('click', () => backend.openLogs());
  $('#open-repo').addEventListener('click', () => backend.openRepo());
  $('#gpu-restart-btn').addEventListener('click', () => {
    Promise.resolve(backend.restartApp?.()).catch((e) => toast(e.message));
  });
  $('#test-tone').addEventListener('click', () => {
    toast(tr('settings.playing_test_tone'));
    Promise.resolve(backend.playTestTone()).catch((e) => toast(e.message));
  });
  $('#net-recheck').addEventListener('click', runNetCheck);

  /** DAW mode: whether the gachi plugin is connected (from the engine, every ~0.5 s). */
  function syncPluginStatus() {
    const daw = backend.getSettings().connection === 'daw';
    const st = $('#daw-status');
    if (!daw) return;
    st.textContent = (backend._plugin ? tr('settings.plugin_connected') : tr('settings.waiting_plugin')) + (backend._pluginWaiting ? ' · ' + tr('settings.1_more_track') : '');
    st.className = backend._plugin && !backend._pluginWaiting ? 'q-good' : 'q-warn';
  }

  /** The others' sound comes back in what we send: they hear themselves again. Said each
   *  time it starts (the engine checks once a second). */
  let feedbackShown = false;
  /** the bar was closed (or timed out) for this time it came on: stays away until the
   *  feedback stops and starts again */
  let feedbackDismissed = false;
  let feedbackTimer = 0;
  const FEEDBACK_BAR_MS = 20000;
  function hideFeedbackBar() {
    feedbackDismissed = true;
    clearTimeout(feedbackTimer);
    $('#feedback-warn')?.toggleAttribute('hidden', true);
  }
  $('#feedback-close')?.addEventListener('click', hideFeedbackBar);
  function noteFeedback(on) {
    if (on && !feedbackShown) {
      toast(backend.getSettings().connection === 'daw'
        ? tr('settings.others_sound_getting')
        : tr('settings.others_sound_getting_back'));
      feedbackDismissed = false;
      clearTimeout(feedbackTimer);
      feedbackTimer = setTimeout(hideFeedbackBar, FEEDBACK_BAR_MS);
    }
    if (!on) {
      feedbackDismissed = false;
      clearTimeout(feedbackTimer);
    }
    feedbackShown = on;
    $('#feedback-warn')?.toggleAttribute('hidden', !on || feedbackDismissed);
  }

  /** DAW mode: a second gachi plugin waits (only one sends). Said once each time it appears. */
  let waitingShown = false;
  function noteWaitingPlugin() {
    const w = !!backend._pluginWaiting && backend.getSettings().connection === 'daw';
    if (w && !waitingShown) {
      toast(tr('settings.gachi_plugin_several'));
    }
    waitingShown = w;
  }

  /** The device's format, as it runs right now (set in the ASIO panel / Windows). */
  /** What follows from the packet size in use (automatic: it follows the device's block,
   *  so this is drawn again as the device's figures come in). */
  function syncPacket(s) {
    const frames = backend.packetFrames();
    const pkt = backend.frameMs(frames);
    $('#pkt-ms').textContent = s.packetSize ? tr('settings.ms_ms_packet', { ms: f1(pkt) }) : tr('latency.auto') + ` · ${frames} · ` + tr('settings.ms_ms_packet', { ms: f1(pkt) });
    // what one stream really costs on the wire: payload + audio header (12) + encryption (28) + IP/UDP (28)
    const ch = s.inputChannel.includes('-') ? 2 : 1; // a pair (or mic + pair) is stereo
    const pps = s.sampleRate / frames;
    const payload = frames * ch * (s.quality === 'pcm16' ? 2 : 3);
    const bw = (payload + 68) * 8 * pps;
    $('#q-bw').textContent = tr('settings.bw_per_person', { bw: bw >= 1e6 ? f1(bw / 1e6) + ' Mbps' : Math.round(bw / 1000) + ' kbps' });
    // upload grows with the room: everyone sends to everyone
    const mb = (n) => f1((bw * (n - 1)) / 1e6);
    const here = room ? tr('settings.about_mb_mbps', { n: peers.size + 1, mb: mb(peers.size + 1) }) : '';
    $('#q-note').textContent = tr('settings.sent_uncompressed_0', { here, a: mb(8), b: mb(16) }) + ' ' + (s.quality === 'pcm16' ? tr('settings.higher_quality_pick') : tr('settings.short_upload_try'));
    $('#jb-val').textContent = `${s.jitterPackets} · ${f1(s.jitterPackets * pkt)}ms`;
  }

  function syncBuffer(s) {
    const rate = backend._sampleRate, frames = backend._bufferFrames;
    $('#fmt-rate').textContent = rate ? `${rate % 1000 ? (rate / 1000).toFixed(1) : rate / 1000} kHz` : '-';
    $('#fmt-buf').textContent = frames && rate ? tr('settings.n_samples', { n: frames }) + ` · ${f1((frames / rate) * 1000)} ms` : '-';
    // the real latency: more than the buffer (USB transfer, the driver's safety margin)
    const lat = backend._latency;
    const daw = s.connection === 'daw';
    $('#fmt-lat-k').textContent = !lat ? tr('settings.device_latency') : daw ? tr('settings.device_latency_estimated') : lat?.reported ? tr('settings.device_latency_driver') : tr('settings.device_latency_buffer');
    $('#fmt-lat-n').textContent = lat ? f1(lat.inMs + lat.outMs) : '-';
    const bar = $('#fmt-lat-bar');
    bar.innerHTML = lat ? `<i style="flex:${lat.inMs};background:var(--seg-in)"></i><i style="flex:${lat.outMs};background:var(--accent)"></i>` : '';
    $('#fmt-lat-legend').textContent = lat ? tr('settings.ms_out_b', { a: f1(lat.inMs), b: f1(lat.outMs) }) : '';
    $('#fmt-note').textContent = s.connection === 'daw'
      ? tr('settings.change_daws_audio')
      : tr('settings.change_sample_rate');
  }
  const segValue = (s, key) => String(s[key]);
  function syncSettings() {
    const s = backend.getSettings();
    $$('#modal-settings .seg[data-key]').forEach((seg) => {
      $$('button', seg).forEach((b) => b.classList.toggle('on', segValue(s, seg.dataset.key) === b.dataset.v));
    });
    const daw = s.connection === 'daw';
    $$('#modal-settings [data-device-only]').forEach((el) => { el.hidden = daw; });
    $$('#modal-settings [data-daw-only]').forEach((el) => { el.hidden = !daw; });
    $('#conn-note').innerHTML = daw
      ? tr('settings.daw_uses_audio')
      : tr('settings.gachi_opens_asio');
    $('#plugin-get')?.addEventListener('click', (e) => { e.preventDefault(); backend.openRelease(); });
    syncPluginStatus();
    const devs = backend.getDevices();
    const sel = $('#set-device');
    sel.innerHTML = devs.map((d) => `<option value="${esc(d.id)}">${esc(d.label)}</option>`).join('');
    sel.value = s.device;
    const dev = devs.find((d) => d.id === s.device);
    $('#device-warn').hidden = !dev?.warn;
    $('#device-warn').textContent = dev?.warn || '';
    fillInputSelect($('#set-input'), s);
    buildChannelMeters(s);
    metroVol.value = s.metronomeVolume ?? 60; paintRange(metroVol);
    syncMetroVol();
    $('#metro-vol-val').textContent = metroVol.value + '%';
    syncBuffer(s);
    syncPacket(s);
    $('#jb-manual').hidden = s.jitterMode !== 'manual';
    $('#jb-style').hidden = s.jitterMode === 'manual';
    $('#jb-note').textContent = s.jitterMode === 'manual' ? tr('settings.auto_keeps_each') : tr('settings.what_favor_when');
    jbRange.value = s.jitterPackets; paintRange(jbRange);
    $('#set-nearby').checked = s.showNearby !== false;
    $('#set-language').value = !s.language || s.language === 'auto' ? 'auto' : i18n.lang;
    $('#set-fixed-port').checked = !!s.fixedPort;
    $('#set-port').hidden = !s.fixedPort;
    if (document.activeElement !== $('#set-port')) $('#set-port').value = s.port ?? 24620;
    $('#set-gpu').checked = s.gpu !== false;
    $('#set-verbose').checked = s.verboseLog === true;
    $('#set-updates').checked = s.checkUpdates !== false;
    syncGpuRestart();
  }

  /** Looks at the network again (a few requests to the connection server) and shows it. */
  async function runNetCheck() {
    netChecking = true;
    drawNetCheck();
    try { netInfo = await backend.checkNetwork(); } finally { netChecking = false; }
    drawNetCheck();
  }

  /** The last check as it came out (or that one is under way), in the language in use. */
  function drawNetCheck() {
    const box = $('#net-checks');
    if (netChecking || !netInfo) {
      box.innerHTML = `<div class="check"><span class="dot pending"></span><span>${tr('home.checking')}</span></div>`;
      return;
    }
    const n = netInfo;
    const LINK = { wired: [tr('netcheck.wired_ethernet'), ''], wifi: ['Wi-Fi', 'warn'], unknown: [tr('netcheck.cant_tell'), 'warn'] };
    const NAT = {
      none: [tr('netcheck.no_router_public'), ''],
      cone: [tr('netcheck.direct_connections_work'), ''],
      symmetric: [tr('netcheck.hard_direct_connections'), 'warn'],
      unknown: [tr('netcheck.cant_tell_whether'), 'warn'],
      offline: [tr('netcheck.cant_reach_server'), 'bad'],
    };
    // straight on the modem / wall jack: no port to open
    const port = n.nat === 'none' ? [tr('netcheck.not_needed_no'), '']
      : n.upnp ? [tr('netcheck.opens_automatically_upnp'), '']
      : backend.getSettings().fixedPort ? [tr('netcheck.fixed_port_port', { port: backend.getSettings().port }), '']
      : [tr('netcheck.not_automatically_forward'), 'warn'];
    const items = [
      [tr('netcheck.link'), ...(LINK[n.link] || LINK.unknown)],
      ['IPv6', n.ipv6 ? tr('netcheck.available') : tr('room.none'), n.ipv6 ? '' : 'warn'],
      ['NAT', ...(NAT[n.nat] || NAT.unknown)],
      [tr('netcheck.port_opening'), ...port],
    ];
    box.innerHTML = items.map(([k, v, c]) => `<div class="check"><span class="dot ${c}"></span><span>${k}</span><span class="v ${c}">${v}</span></div>`).join('');
    renderHomeStatus();
  }

  function renderHomeStatus() {
    const s = backend.getSettings();
    const dev = backend.getDevices().find((d) => d.id === s.device);
    const daw = s.connection === 'daw';
    // (the plugin's state is known while an engine runs: in a room or with settings open)
    $('#st-audio').textContent = daw ? (backend._plugin == null ? tr('settings.vst_link') : tr('settings.vst_link') + ' · ' + (backend._plugin ? tr('netcheck.plugin_connected') : tr('netcheck.waiting_plugin')) + (backend._pluginWaiting ? ' · ' + tr('netcheck.several_tracks') : ''))
      : backend.hasAsio() ? `${dev?.label ?? '-'} · ${tr('settings.n_samples', { n: s.bufferSize })}` : tr('netcheck.no_asio_device');
    // no ASIO driver: the home screen explains what to install instead of the room cards
    // (DAW mode needs none: the DAW owns the interface)
    const noAsio = !daw && !backend.hasAsio();
    $('#no-asio').hidden = !noAsio;
    $('#home-cards').hidden = noAsio;
    $('#st-audio-dot').className = 'dot' + (noAsio ? ' bad' : (daw ? backend._plugin === false : dev?.warn) ? ' warn' : '');
    $('#st-local').textContent = `${f1(backend.localMs())} ms`;
    if (netInfo) {
      const n = netInfo;
      if (n.nat === 'offline') {
        $('#st-net').textContent = tr('netcheck.cant_reach_server');
        $('#st-net-dot').className = 'dot bad';
        return;
      }
      const link = { wired: tr('latency.wired'), wifi: 'Wi-Fi' }[n.link];
      const direct = n.nat === 'cone' || n.nat === 'none';
      $('#st-net').textContent = [link, n.ipv6 && 'IPv6', direct ? tr('netcheck.p2p_ok') : tr('netcheck.server_connected')].filter(Boolean).join(' · ');
      $('#st-net-dot').className = 'dot' + (n.link === 'wired' && direct ? '' : ' warn');
    }
  }

  /* ═══════════════════════ KEYBOARD ═══════════════════════ */
  document.addEventListener('keydown', (e) => {
    if (e.key === 'Escape') {
      if (menuOpen()) return closeMenus();
      if (colorPopFor) return closeColorPop();
      if (popFor) return closePop();
      const open = $('.modal-backdrop.open');
      if (open) return closeModal(open);
      if ($('#screen-connecting').classList.contains('active')) backend.cancelJoin();
      return;
    }
    if (isTyping() || e.ctrlKey || e.metaKey || e.altKey) return;
    if (room && e.key.toLowerCase() === 'm') toggleMute();
  });

  /* native deep link: gachi://join/CODE → window.gachiJoin('CODE') */
  // (asks first: a link can come from any web page, and joining opens the microphone to the room)
  let linkCode = null;
  window.gachiJoin = (raw) => {
    const { code } = normCode(raw);
    if (code.length !== 6 || room) return;
    linkCode = code;
    $('#link-code').textContent = fmtCode(code);
    openModal('modal-link');
  };
  $('#link-join').addEventListener('click', () => {
    closeModal($('#modal-link'));
    if (linkCode && !room) join(linkCode);
    linkCode = null;
  });

  /* ── it's an app, not a web page: no browser menu or browser shortcuts ── */
  // no right-click menu anywhere: the browser's (back, reload, save as, print, emoji,
  // passwords...) has nothing to do with this app. Ctrl+C / Ctrl+V / Ctrl+X still work on
  // selected text and in the text fields.
  document.addEventListener('contextmenu', (e) => e.preventDefault());
  document.addEventListener('keydown', (e) => {
    const k = e.key.toLowerCase();
    const ctrl = e.ctrlKey || e.metaKey;
    if (k === 'f5' || (ctrl && ['r', 'p', 's', 'f', 'g', 'u', 'j', 'h'].includes(k)) || (ctrl && e.shiftKey && k === 'r')) e.preventDefault();
  }, true);

  /* ── no ASIO driver: look again once one is installed ── */
  $('#no-asio-retry').addEventListener('click', async () => {
    const ok = await backend.refreshDevices();
    toast(ok ? tr('netcheck.found_asio_device') : tr('netcheck.still_no_asio'));
  });

  /** Everything on screen again, in the language just picked. */
  function relocalize() {
    i18n.apply();
    closePop();
    renderCode();
    renderRecent();
    renderNearby(lastNearby);
    syncMetroMenu();
    syncRecMenu();
    if (room) {
      // the cards are rebuilt as they are (levels, controls and all come back with the next update)
      const list = $$('#stage-others .strip').map((s) => peers.get(s.dataset.id)).filter(Boolean);
      $('#stage-me').innerHTML = ''; $('#stage-others').innerHTML = ''; strips.clear(); peers.clear();
      $('#stage-me').append(buildSelfStrip());
      list.forEach((p) => addPeer(p));
      syncRecMenu();
      updateRoomChrome();
    }
    // (the network as last looked at, said anew: no need to ask the server again)
    drawNetCheck();
    renderHomeStatus();
  }

  /* ═══════════════════════ DROP-DOWN LISTS ═══════════════════════ */
  // drawn by the app, not the browser's own: WebView2 shuts its own list at a turn of the
  // mouse wheel anywhere (or one just before it opens). The <select> stays, hidden, as the
  // options and the value: code fills, sets and reads it as before, and a pick fires its
  // 'change'
  const SEL_VALUE = Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype, 'value');
  const SEL_INDEX = Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype, 'selectedIndex');
  let openList = null; // { sel, btn, list, active, timer }

  function enhanceSelect(sel) {
    const btn = document.createElement('button');
    btn.type = 'button';
    btn.className = sel.className + ' cselect';
    btn.setAttribute('aria-haspopup', 'listbox');
    btn.setAttribute('aria-expanded', 'false');
    const text = document.createElement('span');
    text.className = 'cselect-text';
    btn.append(text);
    // (before the select: a <label> around them both then clicks the button)
    sel.before(btn);
    sel.hidden = true;
    sel.tabIndex = -1;
    const refresh = () => {
      const o = sel.options[sel.selectedIndex];
      put(text, o ? o.text : '');
      putAttr(btn, 'title', o ? o.text : '');
      const aria = sel.getAttribute('aria-label');
      if (aria) putAttr(btn, 'aria-label', aria);
      btn.disabled = sel.disabled;
      if (openList?.sel === sel) { openList.active = sel.selectedIndex; markList(); }
    };
    sel.refreshList = refresh;
    for (const [prop, base] of [['value', SEL_VALUE], ['selectedIndex', SEL_INDEX]]) {
      Object.defineProperty(sel, prop, {
        configurable: true,
        get() { return base.get.call(sel); },
        set(v) { base.set.call(sel, v); refresh(); },
      });
    }
    // options filled anew (another device, the language): the open list too
    new MutationObserver(() => { refresh(); if (openList?.sel === sel) drawList(); })
      .observe(sel, { childList: true, subtree: true, characterData: true, attributes: true });
    sel.addEventListener('change', refresh);
    btn.addEventListener('click', () => (openList?.sel === sel ? closeList() : openSelectList(sel, btn)));
    btn.addEventListener('keydown', (e) => {
      const open = openList?.sel === sel;
      if (!open) {
        if (['ArrowDown', 'ArrowUp', 'Enter', ' '].includes(e.key)) { e.preventDefault(); openSelectList(sel, btn); }
        return;
      }
      if (e.key === 'ArrowDown' || e.key === 'ArrowUp') { e.preventDefault(); moveActive(e.key === 'ArrowDown' ? 1 : -1); }
      else if (e.key === 'Home' || e.key === 'End') { e.preventDefault(); openList.active = e.key === 'Home' ? -1 : sel.options.length; moveActive(e.key === 'Home' ? 1 : -1); }
      else if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); pickOption(openList.active); }
      else if (e.key === 'Tab') closeList();
    });
    refresh();
  }

  function openSelectList(sel, btn) {
    closeList();
    if (sel.disabled || !sel.options.length) return;
    const list = document.createElement('div');
    list.className = 'cselect-list';
    list.setAttribute('role', 'listbox');
    // (the button keeps the focus: arrows and Enter still reach it)
    list.addEventListener('pointerdown', (e) => e.preventDefault());
    list.addEventListener('click', (e) => {
      const o = e.target.closest('.cselect-opt');
      if (o && !o.classList.contains('off')) pickOption(+o.dataset.i);
    });
    list.addEventListener('pointermove', (e) => {
      const o = e.target.closest('.cselect-opt');
      if (o && !o.classList.contains('off') && openList && openList.active !== +o.dataset.i) { openList.active = +o.dataset.i; markList(false); }
    });
    document.body.append(list);
    openList = { sel, btn, list, active: sel.selectedIndex };
    btn.setAttribute('aria-expanded', 'true');
    btn.classList.add('open');
    drawList();
    // the button gone (the room left, the window shut) or moved: the list goes with it
    let at = btn.getBoundingClientRect();
    openList.timer = setInterval(() => {
      if (!btn.isConnected || !btn.offsetParent) return closeList();
      const r = btn.getBoundingClientRect();
      if (r.left !== at.left || r.top !== at.top || r.width !== at.width) { at = r; placeList(); }
    }, 200);
  }

  function drawList() {
    const { sel, list } = openList;
    const opt = (o) => `<div class="cselect-opt${o.disabled ? ' off' : ''}" role="option" data-i="${o.index}">${esc(o.text)}</div>`;
    list.innerHTML = [...sel.children].map((c) => (c.tagName === 'OPTGROUP'
      ? `<div class="cselect-group">${esc(c.label)}</div>${[...c.children].map(opt).join('')}`
      : opt(c))).join('');
    placeList();
    markList();
  }

  /** the picked one and the one the arrows are on; `reveal`: scrolled into sight */
  function markList(reveal = true) {
    const { sel, list, active } = openList;
    for (const el of list.children) {
      if (!el.dataset.i) continue;
      const i = +el.dataset.i;
      el.classList.toggle('on', i === sel.selectedIndex);
      el.classList.toggle('active', i === active);
      el.setAttribute('aria-selected', String(i === sel.selectedIndex));
      if (reveal && i === active) el.scrollIntoView({ block: 'nearest' });
    }
  }

  function moveActive(step) {
    const { sel } = openList;
    let i = openList.active;
    for (let n = 0; n < sel.options.length; n++) {
      i += step;
      if (i < 0 || i >= sel.options.length) return;
      if (!sel.options[i].disabled) break;
    }
    openList.active = i;
    markList();
  }

  /** below the button, or above it where there is more room (the bottom bar's list) */
  function placeList() {
    const { btn, list } = openList;
    const r = btn.getBoundingClientRect();
    const gap = 6, edge = 8;
    list.style.minWidth = r.width + 'px';
    list.style.maxWidth = Math.max(r.width, innerWidth - edge * 2) + 'px';
    list.style.maxHeight = 'none';
    const want = list.scrollHeight;
    const below = innerHeight - r.bottom - gap - edge, above = r.top - gap - edge;
    const down = below >= Math.min(want, 240) || below >= above;
    list.style.maxHeight = Math.min(360, down ? below : above) + 'px';
    list.style.top = down ? r.bottom + gap + 'px' : '';
    list.style.bottom = down ? '' : innerHeight - r.top + gap + 'px';
    list.style.left = Math.max(edge, Math.min(r.left, innerWidth - edge - list.offsetWidth)) + 'px';
    list.classList.toggle('up', !down);
  }

  function pickOption(i) {
    if (!openList) return;
    const { sel, btn } = openList;
    closeList();
    btn.focus();
    if (i < 0 || i >= sel.options.length || i === sel.selectedIndex) return;
    SEL_INDEX.set.call(sel, i);
    sel.dispatchEvent(new Event('change', { bubbles: true }));
  }

  function closeList() {
    if (!openList) return;
    clearInterval(openList.timer);
    openList.list.remove();
    openList.btn.setAttribute('aria-expanded', 'false');
    openList.btn.classList.remove('open');
    openList = null;
  }
  // a click anywhere else shuts it; the wheel does not (it scrolls the list when on it),
  // only the page it sits on scrolling away under it
  document.addEventListener('pointerdown', (e) => {
    if (openList && !openList.list.contains(e.target) && !openList.btn.contains(e.target)) closeList();
  }, true);
  document.addEventListener('scroll', (e) => {
    if (openList && e.target !== openList.list && e.target.contains?.(openList.btn)) closeList();
  }, true);
  addEventListener('resize', () => closeList());
  addEventListener('blur', () => closeList());
  // (Escape shuts the list alone, before the window or menu it sits in)
  document.addEventListener('keydown', (e) => {
    if (openList && e.key === 'Escape') { e.preventDefault(); e.stopImmediatePropagation(); const { btn } = openList; closeList(); btn.focus(); }
  }, true);

  /* ═══════════════════════ INIT ═══════════════════════ */
  $$('select.select').forEach(enhanceSelect);
  // the language list: every language file there is, by its own name
  for (const [code, name] of i18n.languages()) $('#set-language').append(new Option(name, code));
  i18n.apply();
  hydrateIcons();
  renderCode();
  applyTheme();
  syncSettings();
  renderHomeStatus();
  syncHomeProfile();
  syncIdleEngine();
  probeDevice();
  refreshNearby();
  runNetCheck();
})();
