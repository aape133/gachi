/* gachi — UI logic. Talks only to window.backend (see backend.js). */
(() => {
  'use strict';
  const backend = window.backend;
  const ALPHABET = window.CODE_ALPHABET;

  /* ─────────────── helpers ─────────────── */
  const $ = (s, el = document) => el.querySelector(s);
  const $$ = (s, el = document) => [...el.querySelectorAll(s)];
  const h = (html) => { const t = document.createElement('template'); t.innerHTML = html.trim(); return t.content.firstElementChild; };
  const esc = (s) => String(s).replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
  const f1 = (n) => n.toFixed(1);
  const fmtCode = (c) => `${c.slice(0, 3)}-${c.slice(3)}`;
  const initial = (name) => [...(name || '?')][0];
  /** a stable avatar color for a name */
  const hue = (name) => [...String(name)].reduce((h, c) => (h * 31 + c.codePointAt(0)) % 360, 7);
  // the colours people pick from (as in gachi_proto::signal::COLOR_HUES): the room shows
  // everyone in the one they picked
  const HUES = [255, 215, 190, 165, 135, 90, 48, 25, 355, 320];
  const COLOR_NAMES = ['보라', '파랑', '하늘', '청록', '초록', '연두', '노랑', '주황', '빨강', '분홍'];
  const myHue = () => HUES[backend.getSettings().color] ?? HUES[0];
  const clamp = (v, a, b) => Math.min(b, Math.max(a, v));
  const dbText = (db) => (db <= -40 ? '-∞' : (db > 0 ? '+' : '') + f1(db));
  const panText = (v) => (v === 0 ? 'C' : (v < 0 ? 'L' : 'R') + Math.abs(v));
  const quality = (ms) => (ms < 20 ? ['good', '좋음'] : ms < 30 ? ['warn', '보통'] : ['bad', '높음']);
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
    const clip = peak >= 0.99;
    if (clip !== !!el._clip) { el._clip = clip; el.classList.toggle('clip', clip); }
  }
  /** the avatar's glow with the sound (same rule: only when it changes) */
  function avatarLevel(el, peak) {
    if (!el) return;
    const v = clamp(meterPos(peak) * 1.3 - 0.3, 0, 1);
    if (v !== el._lv) { el._lv = v; el.style.setProperty('--lv', v.toFixed(3)); }
  }
  const setMeter = (el, lv) => { if (!el) return; meterBar(el.children[0], lv[0]); meterBar(el.children[1], lv[1]); };

  function toast(text) {
    const el = h(`<div class="toast" title="눌러서 닫기">${esc(text)}</div>`);
    $('#toasts').append(el);
    const close = () => {
      if (el.classList.contains('out')) return;
      el.classList.add('out');
      setTimeout(() => el.remove(), 300);
    };
    el.addEventListener('click', close);
    // long messages stay long enough to read (about 15 characters a second)
    const ms = Math.min(8000, Math.max(2200, text.length * 65));
    setTimeout(close, ms);
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
  }

  /* ─────────────── latency segments ─────────────── */
  const SEG = [
    ['inBuf', '상대 입력 (장치)', 'var(--seg-in)'],
    ['net', '네트워크 (편도)', 'var(--seg-net)'],
    ['jitter', '지터 버퍼', 'var(--seg-jit)'],
    ['outBuf', '내 출력 (장치)', 'var(--seg-out)'],
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
  let netInfo = null;

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
    const btn = $('#btn-create');
    btn.disabled = true; btn.textContent = '만드는 중…';
    try {
      const r = await backend.createRoom({
        name: $('#create-name').value.trim(),
        maxPeers: createMax,
        password: $('#create-pw-on').checked ? $('#create-pw').value : '',
      });
      enterRoom(r);
      openInvite();
    } catch (err) {
      toast(err.message);
    } finally {
      btn.disabled = false; btn.textContent = '방 만들기';
    }
  });

  /* code input: one real <input> drawn as 6 boxes */
  const codeReal = $('#code-real');
  const boxes = $$('#code-wrap .box');
  const hint = $('#join-hint');
  const HINT_DEFAULT = '붙여넣기(Ctrl+V)도 돼요';

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
    if (rejected) { setHint('0, O, 1, I, L은 코드에 쓰이지 않아요', true); shake(); }
    else setHint(HINT_DEFAULT);
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
    if (code.length === 6) { codeReal.value = code; codeReal.focus(); renderCode(); setHint('코드를 붙여넣었어요 · Enter로 참여'); }
  });

  /* ═══════════════════════ CONNECTING ═══════════════════════ */
  const STEP_LABELS = { signal: '서버에서 방 찾기', exchange: '연결 정보 교환', punch: 'P2P 직접 연결', measure: '지연 측정' };

  /** Joins a room. Returns 'ok', or why not: 'stop' (asked for a password, refused,
   *  cancelled: nothing to retry) or 'retry' (no connection). */
  async function join(code, password) {
    $('#connect-code').textContent = fmtCode(code);
    $('#connect-steps').innerHTML = Object.entries(STEP_LABELS)
      .map(([id, l]) => `<li data-step="${id}"><span class="st"></span><span>${l}</span><span class="detail"></span></li>`).join('');
    show('connecting');
    try {
      const r = await backend.joinRoom(code, (id, st, detail) => {
        const li = $(`[data-step="${id}"]`);
        li.className = st;
        if (st === 'done') { li.querySelector('.st').innerHTML = icon('check'); li.querySelector('.detail').textContent = detail || ''; }
      }, password);
      codeReal.value = ''; renderCode();
      enterRoom(r);
      return 'ok';
    } catch (err) {
      show('home');
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
    leaveRoom();
    toast('인터넷 연결이 끊겨서 방에서 나왔어요. 다시 들어가는 중이에요…');
    try {
      for (let i = 0; i < 8; i++) {
        const r = await join(code, password || undefined);
        if (r !== 'retry') break;
        if (i === 7) { setHint('방에 다시 들어가지 못했어요. 인터넷 연결을 확인하고 코드로 다시 참여해 주세요.', true); break; }
        await new Promise((res) => setTimeout(res, 4000));
        if (room) break;
      }
    } finally { rejoining = false; }
  });

  // password rooms: ask, then join again with it
  let pwFor = null;
  function askPassword(code, wrong) {
    pwFor = code;
    $('#pw-note').textContent = wrong ? '비밀번호가 맞지 않아요. 다시 입력해 주세요.' : '이 방은 비밀번호가 있어요. 방장에게 받은 비밀번호를 입력하세요.';
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
  let clockTimer = null;

  function enterRoom(r) {
    room = r;
    peers.clear(); strips.clear();
    $('#room-name').textContent = r.name; $('#room-name').title = r.name; // (a long name is cut short in the bar)
    $('#room-code').textContent = fmtCode(r.code);
    const stage = $('#stage');
    stage.innerHTML = '';
    stage.append(buildSelfStrip());
    $('#chat-list').innerHTML = '';
    sysMsg(r.isHost ? '방을 만들었어요. 코드를 공유해 친구를 초대하세요.' : `'${r.name}'에 들어왔어요.`);
    r.peers.forEach((p) => addPeer(p));
    qualityAdvised = false;
    adviseQuality();
    updateRoomChrome();
    syncSelfControls();
    clearInterval(clockTimer);
    const tick = () => { $('#session-time').textContent = mmss(Date.now() - r.startedAt); };
    tick(); clockTimer = setInterval(tick, 1000);
    show('room');
  }

  function leaveRoom() {
    backend.leaveRoom();
    room = null;
    clearInterval(clockTimer);
    closePop();
    setMetro(false, { send: false });
    if (rec.on) { setRecUi(false); toast('녹음은 나오면서 저장했어요'); }
    closeMenus();
    $('#screen-room').classList.remove('chat-open');
    unread = 0; updateBadge();
    show('home');
    syncIdleEngine();
    // the room's device closes in the background: read it once it has
    setTimeout(probeDevice, 1500);
    refreshNearby();
    toast('방에서 나왔어요');
  }

  /* ── self strip ── */
  function buildSelfStrip() {
    const s = backend.getSettings();
    const el = h(`
      <article class="strip self" data-id="self">
        <div class="strip-top">
          <span class="top-tags"><span class="tag me-tag">나</span><span class="tag host-tag" hidden>호스트</span></span>
          <button class="net-chip q-accent" data-act="local" title="오디오 설정 열기"><i class="dot"></i><span><span data-f="local"></span> ms</span></button>
        </div>
        <div class="who-big">
          <button class="avatar lg" type="button" data-act="color" title="내 색 바꾸기" style="--hue:${myHue()}">${esc(initial(s.name))}</button>
          <div class="who">
            <div class="name"><button class="name-edit" data-act="rename" title="이름 바꾸기"><span class="nm" data-f="name">${esc(s.name)}</span>${icon('pencil')}</button></div>
            <button class="name-edit inst-edit" data-act="reinst" title="악기 바꾸기"><span class="inst" data-f="inst">${esc(s.instrument)}</span>${icon('pencil')}</button>
          </div>
        </div>
        <div class="meter" data-f="meter"><i></i><i></i></div>
        <label class="ctrl"><span>입력</span><input type="range" min="-24" max="24" step="0.5" value="${backend.self.gain}" data-act="gain"><span class="val" data-f="gain"></span></label>
        <select class="select" data-act="input-ch" aria-label="입력 채널"></select>
        <label class="ctrl" data-f="own" hidden title="DAW에서 들리는 내 소리 크기 (상대에게 보내는 크기는 그대로)"><span>내 소리</span><input type="range" min="-40" max="6" step="0.5" value="${backend.self.monitorDb ?? 0}" data-act="monitor-db"><span class="val" data-f="monitor-db"></span></label>
        <div class="strip-foot">
          <button class="pill" data-act="mute"></button>
          <button class="pill" data-act="monitor" title="내 소리를 바로 듣기 (네트워크 거치지 않음)">${icon('headphones')}<span>모니터</span></button>
          <span class="spacer"></span>
          <button class="ms r" data-act="rec" title="녹음에 넣기 (켜진 사람만 녹음해요)"><i></i></button>
        </div>
      </article>`);
    strips.set('self', { el, meter: $('[data-f=meter]', el), avatar: $('.avatar', el) });
    return el;
  }

  function syncSelfControls() {
    const st = strips.get('self'); if (!st) return;
    const el = st.el, s = backend.getSettings(), self = backend.self;
    $('[data-f=name]', el).textContent = s.name;
    st.avatar.style.setProperty('--hue', myHue());
    $('.host-tag', el).hidden = !room?.isHost;
    $('[data-f=inst]', el).textContent = s.instrument || '악기 적기';
    $('[data-f=inst]', el).title = s.instrument;
    $('.avatar', el).textContent = initial(s.name);
    $('[data-f=local]', el).textContent = f1(backend.localMs());
    const g = $('[data-act=gain]', el); g.value = self.gain; paintRange(g);
    $('[data-f=gain]', el).textContent = dbText(self.gain) + ' dB';
    fillInputSelect($('[data-act=input-ch]', el), s);
    // DAW mode: the DAW track decides what is sent, and the plugin plays your own sound
    // (no delay) at the level set here. The monitor button switches that on and off there
    // (in device mode it is the app's own software monitor)
    const daw = s.connection === 'daw';
    $('[data-act=input-ch]', el).hidden = daw;
    $('[data-act=monitor]', el).title = daw ? 'DAW에서 내 소리 듣기 (gachi 플러그인이 지연 없이 들려줘요)' : '내 소리를 바로 듣기 (네트워크 거치지 않음)';
    $('[data-f=own]', el).hidden = !daw;
    const own = $('[data-act=monitor-db]', el); own.value = self.monitorDb ?? 0; paintRange(own);
    $('[data-f=monitor-db]', el).textContent = dbText(self.monitorDb ?? 0) + ' dB';
    $('[data-act=local]', el).title = (daw
      ? `DAW 블록 기준 입력+출력 (DAW 자체 지연 제외) · ${s.bufferSize} samples · ${s.sampleRate / 1000}kHz`
      : `내 장치 입력+출력 (드라이버 보고) · ${s.bufferSize} samples · ${s.sampleRate / 1000}kHz`) + ` · 보내는 소리: ${streamText(myStream())} · 누르면 오디오 설정`;
    const mute = $('[data-act=mute]', el);
    mute.className = 'pill' + (self.muted ? ' muted' : '');
    mute.innerHTML = icon(self.muted ? 'mic-off' : 'mic') + `<span>${self.muted ? '음소거됨' : '마이크'}</span>`;
    mute.title = self.muted ? '음소거됨 · 누르면 마이크 켜기' : '마이크 음소거'; // (small cards show the icon only)
    $('[data-act=monitor]', el).classList.toggle('on', daw ? self.own !== false : self.monitor);
    $('[data-act=rec]', el).classList.toggle('on', self.rec !== false);
    // (off: the level has nothing to act on)
    $('[data-f=own]', el).classList.toggle('off', daw && self.own === false);
    const mic = $('#mic-btn');
    mic.classList.toggle('muted', self.muted);
    mic.innerHTML = icon(self.muted ? 'mic-off' : 'mic');
  }

  function toggleMute() {
    backend.setSelf({ muted: !backend.self.muted });
    syncSelfControls();
    toast(backend.self.muted ? '마이크 음소거' : '마이크 켜짐');
  }

  /* ── peer strips ── */
  function addPeer(p, announce = false) {
    peers.set(p.id, p);
    const el = h(`
      <article class="strip" data-id="${p.id}">
        <div class="strip-top">
          <span class="top-tags">${p.isHost ? '<span class="tag">호스트</span>' : ''}</span>
          <button class="net-chip" data-act="lat" title="연결 · 지연 자세히"><i class="dot"></i><span data-f="total"></span><span class="warn-ic" data-f="warn" hidden>${icon('alert')}</span></button>
        </div>
        <div class="who-big">
          <div class="avatar lg" style="--hue:${p.hue}">${esc(initial(p.name))}</div>
          <div class="who">
            <div class="name"><span class="nm" title="${esc(p.name)}">${esc(p.name)}</span></div>
            <div class="inst" title="${esc(p.instrument)}">${esc(p.instrument)}</div>
          </div>
        </div>
        <div class="meter" data-f="meter"><i></i><i></i></div>
        <label class="ctrl"><span>볼륨</span><input type="range" min="-40" max="6" step="0.5" value="${p.volume}" data-act="vol"><span class="val" data-f="vol"></span></label>
        <label class="ctrl"><span>팬</span><input type="range" class="pan" min="-100" max="100" step="1" value="${p.pan}" data-act="pan"><span class="val" data-f="pan"></span></label>
        <div class="strip-foot">
          <button class="ms m" data-act="mute" title="뮤트 (이 사람 소리 끄기)">M</button>
          <button class="ms s" data-act="solo" title="솔로 (이 사람 소리만 듣기)">S</button>
          <button class="ms r" data-act="rec" title="녹음에 넣기 (켜진 사람만 녹음해요)"><i></i></button>
          <span class="spacer"></span>
          <button class="ms kick" data-act="kick" title="방에서 내보내기 (다시 들어올 수 없어요)" hidden>${icon('log-out')}<span class="kick-label">한 번 더</span></button>
        </div>
      </article>`);
    strips.set(p.id, { el, meter: $('[data-f=meter]', el), avatar: $('.avatar', el) });
    const slot = $('#stage .invite-slot');
    slot ? slot.before(el) : $('#stage').append(el);
    $$('input[type=range]', el).forEach(paintRange);
    updatePeer(p);
    if (announce) { sysMsg(`${p.name} 님이 들어왔어요`); toast(`${p.name} 님이 들어왔어요`); }
    updateRoomChrome();
  }

  function removePeer(id) {
    const p = peers.get(id), st = strips.get(id);
    peers.delete(id); strips.delete(id);
    if (popFor === id) closePop();
    if (st) { st.el.classList.add('leaving'); setTimeout(() => st.el.remove(), 250); }
    if (p) sysMsg(`${p.name} 님이 나갔어요`);
    setTimeout(updateRoomChrome, 260);
  }

  function updatePeer(p) {
    peers.set(p.id, p);
    const st = strips.get(p.id); if (!st) return;
    const el = st.el;
    const connecting = p.connected === false;
    // until the round trip and the stream's timing are measured the figure is an estimate
    const [qc, ql] = connecting ? ['accent', '연결 중…'] : p.measured === false ? ['accent', '측정 중…'] : quality(p.total);
    // the connection is a small chip: playing together is what the card is about. It
    // speaks up (colour, a warning mark) only when something needs attention
    const chip = $('[data-act=lat]', el);
    chip.className = 'net-chip q-' + qc + (chip.classList.contains('active') ? ' active' : '');
    $('[data-f=total]', el).textContent = connecting || p.measured === false ? ql : `${f1(p.total)} ms`;
    const issues = [p.conn.link === 'wifi' && 'Wi-Fi', p.loss >= 1 && `손실 ${f1(p.loss)}%`, p.outageMs > 0 && '잠깐 끊김', p.conn.type === 'relay' && '릴레이'].filter(Boolean);
    $('[data-f=warn]', el).hidden = !issues.length;
    chip.title = `${p.conn.type === 'relay' ? '릴레이' : 'P2P' + (p.conn.ipv6 ? ' · IPv6' : '')} · 지연 ${f1(p.total)} ms (${ql})`
      + (issues.length ? ' · ' + issues.join(' · ') : '') + ' · 누르면 자세히';
    $('[data-f=vol]', el).textContent = dbText(p.volume) + ' dB';
    $('[data-f=pan]', el).textContent = panText(p.pan);
    $('[data-act=mute]', el).classList.toggle('on', p.mute);
    $('[data-act=solo]', el).classList.toggle('on', p.solo);
    $('[data-act=rec]', el).classList.toggle('on', p.rec !== false);
  }

  function updateDim() {
    const anySolo = [...peers.values()].some((p) => p.solo);
    peers.forEach((p) => strips.get(p.id)?.el.classList.toggle('dim', p.mute || (anySolo && !p.solo)));
  }

  /** The biggest cards with which everyone fits the window without scrolling: big, medium
   *  (8 people in the default window), small (a full room of 16 in it); if not even the
   *  small ones fit, the stage scrolls. Looked at again whenever the people or the window
   *  change. */
  function fitStage() {
    const stage = $('#stage');
    const fits = () => stage.scrollHeight <= stage.clientHeight + 1;
    stage.classList.remove('mid', 'many');
    if (fits()) return;
    stage.classList.add('mid');
    if (fits()) return;
    stage.classList.replace('mid', 'many');
  }
  // (the stage's own size: the window, the chat panel opening; and the window itself, which
  // is also heard of while nothing is being drawn)
  new ResizeObserver(() => { if (room) fitStage(); }).observe($('#stage'));
  // (a moment later: at the event itself the page is not always laid out at the new size yet)
  let fitTimer = 0;
  window.addEventListener('resize', () => { clearTimeout(fitTimer); fitTimer = setTimeout(() => { if (room) fitStage(); }, 60); });
  function updateRoomChrome() {
    if (!room) return;
    // only the host can put people out
    $$('#stage [data-act=kick]').forEach((b) => { b.hidden = !room.isHost; });
    const count = peers.size + 1;
    const open = room.open !== false;
    $('#room-count').textContent = `${count}/${room.maxPeers}명`;
    const code = $('#room-code');
    code.textContent = open ? fmtCode(room.code) : '코드 꺼짐';
    code.classList.toggle('mono', open); // the code in mono; Korean text in the normal font
    $('#btn-room').classList.toggle('closed', !open);
    if (roomPanel.classList.contains('open')) fillRoomPanel();
    let slot = $('#stage .invite-slot');
    if (slot && !open) { slot.remove(); slot = null; }
    if (open && count < room.maxPeers) {
      if (!slot) {
        slot = h(`<button class="invite-slot"><span class="plus">${icon('user-plus')}</span>친구 초대하기<span class="mono">${fmtCode(room.code)}</span></button>`);
        slot.addEventListener('click', openInvite);
        $('#stage').append(slot);
      }
    } else slot?.remove();
    fitStage();
    updateSummary();
    updateDim();
  }

  function updateSummary() {
    const all = [...peers.values()].filter((p) => p.connected !== false);
    const list = all.filter((p) => p.measured !== false);
    const big = $('#avg-latency'), sub = $('#avg-sub');
    if (!all.length) { big.textContent = '--'; big.className = 'big num'; sub.textContent = '아직 아무도 없어요'; return; }
    if (!list.length) { big.textContent = '--'; big.className = 'big num'; sub.textContent = '지연 측정 중…'; return; }
    const avg = list.reduce((a, p) => a + p.total, 0) / list.length;
    const max = Math.max(...list.map((p) => p.total));
    big.textContent = f1(avg);
    big.className = 'big num q-' + quality(avg)[0];
    sub.textContent = `최대 ${f1(max)} ms · 편도, 상대 입력부터 내 출력까지`;
  }

  /* ── stage interactions (delegated) ── */
  const stage = $('#stage');
  stage.addEventListener('input', (e) => {
    const t = e.target, act = t.dataset.act; if (!act) return;
    const id = t.closest('.strip').dataset.id;
    if (t.type === 'range') paintRange(t);
    if (id === 'self') {
      if (act === 'gain') { backend.setSelf({ gain: +t.value }); syncSelfControls(); }
      if (act === 'monitor-db') { backend.setSelf({ monitorDb: +t.value }); syncSelfControls(); }
      return;
    }
    if (act === 'vol') backend.setPeer(id, { volume: +t.value });
    if (act === 'pan') backend.setPeer(id, { pan: +t.value });
  });
  stage.addEventListener('change', (e) => {
    if (e.target.dataset.act === 'input-ch') { backend.updateSettings({ inputChannel: e.target.value }); }
  });
  stage.addEventListener('dblclick', (e) => {
    const t = e.target; if (t.type !== 'range') return;
    t.value = ['gain', 'vol', 'pan', 'monitor-db'].includes(t.dataset.act) ? 0 : t.value;
    t.dispatchEvent(new Event('input', { bubbles: true }));
  });
  stage.addEventListener('click', (e) => {
    const b = e.target.closest('button[data-act]'); if (!b) return;
    const id = b.closest('.strip').dataset.id, act = b.dataset.act;
    if (id === 'self') {
      if (act === 'mute') toggleMute();
      if (act === 'monitor') {
        // VST mode: your own sound through the plugin; else the app's software monitor
        if (backend.getSettings().connection === 'daw') backend.setSelf({ own: backend.self.own === false });
        else backend.setSelf({ monitor: !backend.self.monitor });
        syncSelfControls();
      }
      if (act === 'rec') { backend.setSelf({ rec: backend.self.rec === false }); syncSelfControls(); }
      if (act === 'local') openSettings('audio');
      return;
    }
    const p = peers.get(id);
    if (act === 'mute') { backend.setPeer(id, { mute: !p.mute }); }
    if (act === 'solo') { backend.setPeer(id, { solo: !p.solo }); }
    // (shown at once: the figures that confirm it come back half a second later)
    if (act === 'rec') { p.rec = p.rec === false; b.classList.toggle('on', p.rec); backend.setPeer(id, { rec: p.rec }); }
    // put out of the room: asks once more (the button says so for a moment)
    if (act === 'kick') {
      if (!b.classList.contains('arm')) {
        b.classList.add('arm');
        setTimeout(() => b.classList.remove('arm'), 2500);
      } else {
        b.classList.remove('arm');
        backend.kickPeer(id);
      }
    }
    if (act === 'lat') { popFor === id ? closePop() : openPop(id, b); }
  });

  /* ── backend events ── */
  backend.on('peer-joined', ({ peer, silent }) => { if (room) { addPeer(peer, !silent); adviseQuality(); } });
  backend.on('peer-left', ({ id }) => removePeer(id));
  // the host put us out
  backend.on('kicked', () => {
    if (!room) return;
    leaveRoom();
    toast('방장이 방에서 내보냈어요');
  });
  backend.on('stats', ({ peers: list }) => {
    // settings screen: the real buffer as the device runs it
    noteWaitingPlugin();
    if ($('#modal-settings').classList.contains('open')) { syncBuffer(backend.getSettings()); syncPacket(backend.getSettings()); syncPluginStatus(); }
    // the home screen's status line follows the plugin as it connects / goes away
    if (!room) { renderHomeStatus(); return; }
    noteFeedback(!!backend._feedback);
    // our own buffers as the device really runs (changes with the ASIO panel)
    const self = strips.get('self');
    if (self) $('[data-f=local]', self.el).textContent = f1(backend.localMs());
    list.forEach(updatePeer);
    updateSummary(); updateDim();
    if (popFor) updatePop();
  });
  const miniMeter = $('#mini-meter'), setMeterEl = $('#set-meter'), settingsModal = $('#modal-settings');
  backend.on('levels', (lv) => {
    // nobody is looking (minimized): nothing to draw
    if (document.hidden) return;
    if (room) {
      const self = strips.get('self');
      setMeter(self?.meter, lv.self);
      setMeter(miniMeter, lv.self);
      avatarLevel(self?.avatar, lv.self[0]);
      for (const id in lv.peers) {
        const st = strips.get(id); if (!st) continue;
        const l = lv.peers[id];
        setMeter(st.meter, l);
        avatarLevel(st.avatar, l[0]);
      }
    }
    if (settingsModal.classList.contains('open')) {
      setMeter(setMeterEl, lv.self);
      // by channel number (the bars are grouped by name, so position inside a group is not it)
      (lv.inputs || []).forEach((v, i) => { const bar = chBars.get(i + 1); if (bar) meterBar(bar, v); });
    }
  });
  backend.on('chat', (m) => addMsg(m));
  // system messages from the engine/network (relay fallback, settings applied, host changed …)
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
    syncSelfControls(); // my own "호스트" tag
    room.safetyCode = r.safetyCode;
    // code, on/off and size can change (host settings, a new host, a new code)
    if (r.code && r.code !== room.code) {
      room.code = r.code;
      $('#stage .invite-slot')?.remove();
    }
    if (r.open !== undefined) room.open = r.open;
    if (r.maxPeers) room.maxPeers = r.maxPeers;
    if (r.inviteUrl) room.inviteUrl = r.inviteUrl;
    if ($('#modal-invite').classList.contains('open')) fillRoomPanel();
    renderSafety();
    r.peers.forEach((p) => {
      const st = strips.get(p.id); if (!st) return;
      const prev = peers.get(p.id); if (prev) prev.isHost = p.isHost;
      $('.name', st.el).innerHTML = `<span class="nm" title="${esc(p.name)}">${esc(p.name)}</span>` + (p.isHost ? '<span class="tag">호스트</span>' : '');
      $('.inst', st.el).textContent = p.instrument;
      $('.inst', st.el).title = p.instrument;
      st.avatar.style.setProperty('--hue', p.hue);
      if (prev) { prev.name = p.name; prev.instrument = p.instrument; prev.hue = p.hue; }
    });
    updateRoomChrome();
  });
  backend.on('settings', () => { syncSelfControls(); syncSettings(); renderHomeStatus(); syncHomeProfile(); applyTheme(); syncIdleEngine(); });

  /* ── latency popover ── */
  const pop = $('#lat-pop');
  let popFor = null, popAnchor = null;

  function openPop(id, anchor) {
    closePop();
    popFor = id; popAnchor = anchor;
    anchor.classList.add('active');
    const p = peers.get(id);
    pop.innerHTML = `
      <div class="pop-title">${esc(p.name)} → 나</div>
      <div class="pop-total"><span class="n" data-f="total"></span><span class="u">ms</span><span class="lat-q" data-f="q"></span></div>
      <div class="lat-bar lg"><div class="lat-fill" data-f="bar"></div></div>
      <div class="legend" data-f="legend"></div>
      <div class="pop-sep"></div>
      <dl class="kv">
        <dt>경로</dt><dd data-f="path"></dd>
        <dt>상대 연결</dt><dd data-f="link"></dd>
        <dt>보내는 소리</dt><dd data-f="stream"></dd>
        <dt>보안</dt><dd data-f="sec"></dd>
        <dt>왕복 (RTT)</dt><dd data-f="rtt"></dd>
        <dt>패킷 손실</dt><dd data-f="loss"></dd>
      </dl>
      <div class="tip" data-f="tip" hidden></div>
      <div class="pop-sep"></div>
      <div class="row">
        <div><div class="jb-title">지터 버퍼</div><div class="note" data-f="jb-note"></div></div>
        <div class="stepper">
          <button class="chip sm" data-pop="auto">자동</button>
          <button class="step-btn" data-pop="dec" title="줄이기">${icon('minus')}</button>
          <span class="num" data-f="jb"></span>
          <button class="step-btn" data-pop="inc" title="늘리기">${icon('plus')}</button>
        </div>
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
      // not enough room below → open beside the strip so it never covers its own card
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
    return [`${st.bits}bit`, khz, st.channels === 1 ? '모노' : '스테레오', st.packetFrames ? `패킷 ${st.packetFrames}` : ''].filter(Boolean).join(' · ');
  }
  /** the same for what we send ourselves, from our settings */
  function myStream() {
    const s = backend.getSettings();
    const daw = s.connection === 'daw';
    return { bits: s.quality === 'pcm16' ? 16 : 24, channels: daw || s.inputChannel.includes('-') ? 2 : 1, sampleRate: s.sampleRate, packetFrames: backend.packetFrames() };
  }
  function updatePop() {
    const p = peers.get(popFor); if (!p) return closePop();
    const b = p.breakdown, [qc, ql] = p.measured === false ? ['accent', '측정 중…'] : quality(p.total);
    const n = $('[data-f=total]', pop); n.textContent = f1(p.total); n.className = 'n q-' + qc;
    const q = $('[data-f=q]', pop); q.textContent = ql; q.className = 'lat-q q-' + qc;
    paintBar($('[data-f=bar]', pop), b, p.total);
    $('[data-f=legend]', pop).innerHTML = SEG.filter(([k]) => b[k] > 0)
      .map(([k, l, c]) => `<div class="legend-row"><i style="background:${c}"></i><span>${l}</span><span class="v">${f1(b[k])} ms</span></div>`).join('');
    $('[data-f=path]', pop).textContent = p.conn.type === 'relay' ? '릴레이 서버 경유' : `P2P 직결${p.conn.ipv6 ? ' · IPv6' : ' · 홀펀칭'}`;
    $('[data-f=link]', pop).innerHTML = { wifi: '<span class="q-warn">Wi-Fi</span>', wired: '유선' }[p.conn.link] || '알 수 없음';
    $('[data-f=stream]', pop).textContent = streamText(p.stream) || '확인 중…';
    $('[data-f=sec]', pop).innerHTML = p.conn.encrypted
      ? (p.conn.cipher?.startsWith('AES') ? 'AES-256' : 'ChaCha20')
      : '<span class="q-warn">키 교환 중…</span>';
    $('[data-f=rtt]', pop).textContent = p.rtt > 0 ? f1(p.rtt) + ' ms' : '측정 중…';
    $('[data-f=loss]', pop).textContent = f1(p.loss) + ' %';
    const tip = $('[data-f=tip]', pop);
    const tipText = p.conn.type === 'relay'
      ? '직접 연결에 실패해 릴레이 서버를 거치고 있어요. 상대 공유기의 UPnP를 켜면 직결되어 지연이 줄어들 수 있어요.'
      : p.outageMs > 0
        // the stream stopped for a moment (not loss: more buffer would not have helped)
        ? `연결이 잠깐 끊겼어요 (${p.outageMs >= 1000 ? f1(p.outageMs / 1000) + '초' : Math.round(p.outageMs) + 'ms'}). 그동안 소리가 비었어요. 자주 그러면 공유기·랜선이나 ${p.name} 님 PC의 네트워크를 확인해 보세요.`
      : p.conn.link === 'wifi' && b.jitter - (p.jitterIfWired ?? b.jitter) >= 0.5
        // compared with what the same stream would need on a wired link
        ? `${p.name} 님이 Wi-Fi를 쓰고 있어요. 유선으로 바꾸면 지터 버퍼를 약 ${f1(b.jitter - p.jitterIfWired)}ms 줄일 수 있어요.`
        : p.conn.link === 'wifi'
        ? `${p.name} 님이 Wi-Fi를 쓰고 있어요. 지금은 안정적이지만, 유선이 더 고르게 유지돼요.`
        : p.loss > 1 ? '패킷 손실이 많아요. 지터 버퍼를 한 단계 늘려 보세요.' : '';
    tip.hidden = !tipText;
    if (tipText) tip.innerHTML = icon('alert') + `<span>${esc(tipText)}</span>`;
    $('[data-f=jb]', pop).textContent = (+p.packets).toFixed(1);
    $('[data-pop=auto]', pop).classList.toggle('on', p.jitterAuto);
    $('[data-f=jb-note]', pop).textContent = p.jitterAuto
      ? `자동 · ${(+p.packets).toFixed(1)}패킷 (${f1(b.jitter)} ms)`
      : `수동 · 자동 권장값 ${(+p.autoPackets).toFixed(1)}패킷`;
  }
  function closePop() {
    pop.classList.remove('open');
    popAnchor?.classList.remove('active');
    popFor = null; popAnchor = null;
  }
  pop.addEventListener('click', (e) => {
    const b = e.target.closest('[data-pop]'); if (!b || !popFor) return;
    const p = peers.get(popFor);
    // manual sizes are whole packets
    const whole = Math.max(1, Math.round(p.packets));
    if (b.dataset.pop === 'auto') backend.setPeer(popFor, { jitterAuto: !p.jitterAuto, jitterPackets: whole });
    if (b.dataset.pop === 'dec') backend.setPeer(popFor, { jitterAuto: false, jitterPackets: Math.max(1, whole - 1) });
    if (b.dataset.pop === 'inc') backend.setPeer(popFor, { jitterAuto: false, jitterPackets: whole + 1 });
  });
  document.addEventListener('pointerdown', (e) => {
    if (popFor && !pop.contains(e.target) && !popAnchor?.contains(e.target)) closePop();
  });
  window.addEventListener('resize', placePop);
  stage.addEventListener('scroll', closePop);

  /* ── mic / master / leave ── */
  $('#mic-btn').addEventListener('click', toggleMute);
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
    const reset = () => { clearTimeout(leaveArm); leaveArm = null; btn.classList.remove('arm'); $('.leave-label', btn).textContent = '나가기'; };
    if (!leaveArm) {
      btn.classList.add('arm');
      $('.leave-label', btn).textContent = '한 번 더 누르면 나가요';
      leaveArm = setTimeout(reset, 2500);
      return;
    }
    reset();
    leaveRoom();
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
    $('#metro-toggle').title = metroOn ? `메트로놈 끄기 · ${bpm} BPM · ${sigLabel(beats)} (방 전체)` : '메트로놈 켜기 (방 전체)';
  }
  /** `counted`: bars the room is being counted in with, from `startMs` */
  function setMetro(on, { send = true, startMs = null, counted = null } = {}) {
    const turnedOn = on && !metroOn;
    metroOn = on;
    $('#metro').classList.toggle('on', on);
    syncMetroMenu();
    clearInterval(beatTimer);
    clearTimeout(sendTimer);
    if (send) backend.setMetronome({ on, bpm, beats, countIn });
    if (!on) return;
    const origin = startMs ?? Date.now();
    const period = 60000 / bpm;
    // (the room's word when it comes; until then what we asked for, when turning it on)
    const lead = (counted ?? (turnedOn && send ? countIn : 0)) * beats;
    let last = null;
    const d = $('#metro-beat');
    beatTimer = setInterval(() => {
      const n = Math.floor((Date.now() - origin) / period);
      if (n < 0 || n === last) return;
      last = n;
      d.classList.add('flash');
      d.classList.toggle('count', n < lead);
      d.classList.toggle('down', n >= lead && (n - lead) % beats === 0);
      setTimeout(() => d.classList.remove('flash'), 90);
    }, 10);
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
    toast(metroOn ? `메트로놈 ${bpm} BPM · ${sigLabel(beats)}${countIn ? ` · ${countIn}마디 세고 시작` : ''} · 모두에게 동기화됨` : '메트로놈 끔');
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
    if (!byMe && byName) sysMsg(on ? `${byName}님이 메트로놈을 ${bpm} BPM · ${sigLabel(beats)}로 맞췄어요` : `${byName}님이 메트로놈을 껐어요`);
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
  function syncMetroVol() {
    metroVolMenu.value = backend.getSettings().metronomeVolume ?? 60; paintRange(metroVolMenu);
    $('#metro-vol-menu-val').textContent = metroVolMenu.value + '%';
  }

  /* ── recording ── */
  const rec = { on: false, start: 0, timer: null, busy: false };
  /** what the recording's files are, in words: "WAV 16bit", "FLAC 24bit" */
  function recFormatText(s = backend.getSettings()) {
    const bits = s.recFormat === 'flac' ? Math.min(24, s.recBits || 16) : (s.recBits || 16);
    return `${s.recFormat === 'flac' ? 'FLAC' : 'WAV'} ${bits === 32 ? '32bit float' : bits + 'bit'}`;
  }
  function syncRecMenu() {
    const s = backend.getSettings();
    const format = s.recFormat === 'flac' ? 'flac' : 'wav';
    const bits = format === 'flac' ? Math.min(24, s.recBits || 16) : (s.recBits || 16);
    segSet($('#rec-format'), format);
    segSet($('#rec-bits'), bits);
    // (FLAC has no float)
    $('#rec-bits button[data-v="32"]').hidden = format === 'flac';
    $('#rec-mix').checked = s.recMix !== false;
    $('#rec-menu').classList.toggle('locked', rec.on);
    $('#rec-locked').hidden = !rec.on;
    // what an hour of one stereo track comes to (a mono one: half)
    const rate = s.sampleRate || 48000;
    const wav = rate * 2 * (bits / 8) * 3600;
    const size = (b) => (b >= 1e9 ? f1(b / 1e9) + ' GB' : Math.round(b / 1e6) + ' MB');
    $('#rec-note').textContent = format === 'flac'
      ? `스테레오 한 트랙이 1시간에 대략 ${size(wav * 0.55)}예요(소리에 따라 달라요). 무손실이고 WAV보다 작아요.`
      : `스테레오 한 트랙이 1시간에 약 ${size(wav)}예요. 무손실이고 어느 DAW에서나 열려요.`;
    $('#rec-btn').title = rec.on ? '녹음 끝내기' : `녹음 시작 · ${recFormatText(s)}`;
  }
  for (const [id, key, num] of [['rec-format', 'recFormat', false], ['rec-bits', 'recBits', true]]) {
    $('#' + id).addEventListener('click', async (e) => {
      const b = e.target.closest('button'); if (!b || rec.on) return;
      const patch = { [key]: num ? +b.dataset.v : b.dataset.v };
      // FLAC has no 32 bit float: 24 bit there
      if (patch.recFormat === 'flac' && (backend.getSettings().recBits || 16) > 24) patch.recBits = 24;
      await backend.updateSettings(patch);
      syncRecMenu();
    });
  }
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
      $('#rec-label').textContent = '녹음';
    }
    syncRecMenu();
  }
  function recSaved(r) {
    if (r?.error) { toast(r.error); if (room) sysMsg(`녹음이 멈췄어요: ${r.error}`); return; }
    if (!r || r.files == null) return;
    toast(`녹음 저장됨 · 파일 ${r.files}개 · ${recFormatText()} · ${mmss(r.seconds * 1000)}`);
    if (!room) return;
    const el = h(`<div class="msg sys">녹음을 저장했어요 (파일 ${r.files}개 · ${esc(r.dir || '')}) <button class="link-btn" type="button">폴더 열기</button></div>`);
    $('button', el).addEventListener('click', () => backend.openRecordings?.().catch((e) => toast(e.message)));
    pushMsg(el);
    if (r.dropped) sysMsg(`디스크가 느려서 녹음 일부(${r.dropped}블록)가 빠졌어요`);
  }
  async function toggleRec() {
    if (rec.busy) return;
    const want = !rec.on;
    // the tracks picked on the cards (all of them unless some were switched off)
    const picked = (backend.self.rec !== false ? 1 : 0) + [...peers.values()].filter((p) => p.rec !== false).length;
    if (want && !picked) { toast('녹음할 사람이 없어요. 카드의 녹음 버튼(●)을 켜 주세요'); return; }
    rec.busy = true;
    try {
      const r = await backend.setRecording(want);
      setRecUi(want);
      if (want) toast(`녹음 시작 · ${recFormatText()} · ${picked === peers.size + 1 ? '사람마다 트랙을 따로 저장해요' : `고른 ${picked}명만 저장해요`}`);
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
    const text = `방에 ${others + 1}명이 있어요. 지금 24bit로 보내고 있어서 업로드가 약 ${mbps(24)} Mbps 필요해요. 설정 → 네트워크 → 음질에서 16bit로 바꾸면 약 ${mbps(16)} Mbps로 줄어요.`;
    sysMsg(text);
    if (!chatOpen()) toast(text);
  }
  function sysMsg(text) { pushMsg(h(`<div class="msg sys">${esc(text)}</div>`)); }
  function addMsg({ from, text }) {
    if (from === 'me') { pushMsg(h(`<div class="msg me"><div class="bubble">${esc(text)}</div></div>`)); return; }
    const p = peers.get(from);
    pushMsg(h(`<div class="msg"><span class="from" style="--hue:${p?.hue ?? 0}">${esc(p?.name ?? '?')}</span><div class="bubble">${esc(text)}</div></div>`));
    if (!chatOpen()) { unread++; updateBadge(); }
  }
  $('#btn-chat').addEventListener('click', () => toggleChat());
  $('#btn-chat-close').addEventListener('click', () => toggleChat(false));
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
      ? '입장 코드가 꺼져 있어서 지금은 아무도 들어올 수 없어요.'
      : '방장이 입장 코드를 꺼 둬서 지금은 아무도 들어올 수 없어요.';
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
    $('#rm-code-note').textContent = open ? '코드를 아는 사람은 누구나 들어올 수 있어요' : '꺼짐 · 아무도 못 들어와요';
    $('#rm-size').textContent = `${room.maxPeers}명`;
    $('#rm-size-note').textContent = `지금 ${count}명 · ${count >= room.maxPeers ? '가득 참' : `${room.maxPeers - count}자리 남음`}`;
    $('#rm-dec').disabled = !room.isHost || room.maxPeers <= Math.max(2, count);
    $('#rm-inc').disabled = !room.isHost || room.maxPeers >= (room.maxCap || 16);
    $('#rm-new').disabled = !room.isHost || !open;
    $('#rm-open').disabled = !room.isHost;
    renderSafety();
  }
  async function setRoom(patch) {
    try {
      await backend.setRoom(patch);
      if (patch.open === false) toast('입장 코드를 껐어요 · 새로운 사람은 못 들어와요');
      if (patch.open === true) toast('입장 코드를 켰어요');
      if (patch.newCode) toast('새 코드를 만들었어요 · 예전 코드는 더 이상 안 돼요');
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
    el.textContent = code || '연결이 끝나면 나와요';
    el.classList.toggle('pending', !code);
  }
  $('#btn-room').addEventListener('click', openInvite);
  $('#copy-code').addEventListener('click', () => copy(fmtCode(room.code), '코드를 복사했어요'));
  $('#copy-link').addEventListener('click', () => copy(inviteLink(room.code), '링크를 복사했어요'));
  $('#copy-msg').addEventListener('click', () => copy(
    `[gachi] '${room.name}' 합주에 초대할게요!\n앱에서 코드 ${fmtCode(room.code)} 를 입력하거나 링크를 열어 주세요.\n${inviteLink(room.code)}`,
    '초대 메시지를 복사했어요 · 메신저에 붙여넣으세요'));

  /* ═══════════════════════ MODALS ═══════════════════════ */
  function openModal(id) { closePop(); $('#' + id).classList.add('open'); }
  function closeModal(el) {
    el.classList.remove('open');
    if (el.id === 'modal-settings') syncIdleEngine();
  }

  /** Outside a room the audio engine runs only while settings are open (it holds the ASIO
   *  device), except in DAW mode: there it holds no device, and it is what the gachi plugin
   *  connects to, so it stays up on the home screen too (and the plugin's state shows). */
  function syncIdleEngine() {
    if (room) return;
    const want = backend.getSettings().connection === 'daw' || $('#modal-settings').classList.contains('open');
    Promise.resolve(backend.setPreview?.(want)).catch((e) => { if (want) toast(e.message); });
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
  const chName = (names, c) => names[c - 1] || `입력 ${c}`;
  /** "Loopback 1" + "Loopback 2" → "Loopback 1/2" */
  function pairName(names, c) {
    const a = names[c - 1], b = names[c];
    if (!a || !b) return `입력 ${c}/${c + 1}`;
    const ma = a.match(/^(.*?)(\d+)\s*([LR]?)$/i), mb = b.match(/^(.*?)(\d+)\s*([LR]?)$/i);
    if (ma && mb && ma[1] === mb[1]) return ma[3] && mb[3] ? `${ma[1]}${ma[2]}` : `${ma[1]}${ma[2]}/${mb[2]}`;
    return `${a} / ${b}`;
  }
  function inputOptions(n, names = []) {
    const mono = [], pairs = [], mixes = [];
    for (let c = 1; c <= n; c++) mono.push([`${c}`, `${chName(names, c)} (모노)`]);
    for (let c = 1; c + 1 <= n; c += 2) pairs.push([`${c}-${c + 1}`, `${pairName(names, c)} (스테레오)`]);
    // a mic on 1 or 2, plus any later stereo pair (an interface's loopback carries PC audio)
    for (const m of [1, 2]) for (let c = 3; c + 1 <= n; c += 2) mixes.push([`${m}+${c}-${c + 1}`, `${chName(names, m)} + ${pairName(names, c)}`]);
    return [['모노', mono], ['스테레오', pairs], ['마이크 + 스테레오 (반주·루프백 같이 보내기)', mixes]];
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
      const extra = new Option(`입력 ${s.inputChannel} (이 장치에 없음)`, s.inputChannel);
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
    box.hidden = $('#ch-note').hidden = n <= 2;
    const names = inputNames(s);
    const key = `${n}|${names.join(',')}`;
    if (box.dataset.n !== key) {
      box.dataset.n = key;
      box.innerHTML = channelGroups(names, n).map((g) => `<div class="ch-group">
        <span class="g">${esc(g.name)}</span>
        <div class="chs">${g.chans.map(([c, label]) => `<div class="ch" data-c="${c}" title="${esc(chName(names, c))}"><div class="bar"><i></i></div><span class="n">${esc(label)}</span></div>`).join('')}</div>
      </div>`).join('');
    }
    const used = usedChannels(s.inputChannel);
    $$('.ch', box).forEach((el) => el.classList.toggle('used', used.has(+el.dataset.c)));
    chBars.clear();
    $$('.ch', box).forEach((el) => chBars.set(+el.dataset.c, $('i', el)));
  }

  /* ── theme: one button flips light/dark ("system" until first flipped) ── */
  const prefersDark = matchMedia('(prefers-color-scheme: dark)');
  const effectiveTheme = () => {
    const t = backend.getSettings().theme;
    return t === 'light' || t === 'dark' ? t : prefersDark.matches ? 'dark' : 'light';
  };
  function applyTheme() {
    const t = backend.getSettings().theme;
    if (t === 'light' || t === 'dark') document.documentElement.dataset.theme = t;
    else delete document.documentElement.dataset.theme;
    try { localStorage.setItem('gachi-theme', t); } catch { /* fine without */ }
    // the button shows what you switch to
    $$('[data-act=theme]').forEach((b) => { b.innerHTML = icon(effectiveTheme() === 'dark' ? 'sun' : 'moon'); });
  }
  $$('[data-act=theme]').forEach((b) => b.addEventListener('click', () => {
    const flip = () => backend.updateSettings({ theme: effectiveTheme() === 'dark' ? 'light' : 'dark' });
    const calm = matchMedia('(prefers-reduced-motion: reduce)').matches;
    // a soft cross-fade from the old look to the new one (instant where unsupported)
    if (document.startViewTransition && !calm) document.startViewTransition(flip);
    else flip();
  }));
  prefersDark.addEventListener('change', applyTheme);

  /* ── rename yourself / change your instrument (also inside a room: everyone sees it at once) ── */
  function startRename(btn, key = 'name') {
    const cur = backend.getSettings()[key] || '';
    const input = h(`<input class="name-input" maxlength="12" autocomplete="off" spellcheck="false" placeholder="${key === 'name' ? '이름' : '악기'}">`);
    input.value = key === 'name' && cur === '나' ? '' : cur;
    btn.replaceWith(input);
    input.focus(); input.select();
    let done = false;
    const finish = (save) => {
      if (done) return; done = true;
      const v = input.value.trim();
      input.replaceWith(btn);
      if (save && v !== cur && (v || key === 'instrument')) {
        backend.updateSettings({ [key]: v });
        toast(key === 'name' ? '이름을 바꿨어요' : '악기를 바꿨어요');
      }
    };
    input.addEventListener('keydown', (e) => {
      if (e.key === 'Enter') finish(true);
      if (e.key === 'Escape') finish(false);
      e.stopPropagation();
    });
    input.addEventListener('blur', () => finish(true));
  }
  document.addEventListener('click', (e) => {
    const b = e.target.closest('[data-act=rename], [data-act=reinst]');
    if (b) startRename(b, b.dataset.act === 'rename' ? 'name' : 'instrument');
  });

  /* ── my colour: click my own avatar, pick one of the presets ── */
  const colorPop = h('<div class="color-pop" role="dialog" aria-label="내 색"></div>');
  document.body.append(colorPop);
  function closeColorPop() { colorPop.classList.remove('open'); }
  function openColorPop(anchor) {
    const mine = backend.getSettings().color;
    colorPop.innerHTML = '<div class="color-title">내 색</div><div class="swatches">'
      + HUES.map((hh, i) => `<button class="swatch${i === mine ? ' on' : ''}" type="button" data-color="${i}" style="--hue:${hh}" title="${COLOR_NAMES[i]}" aria-label="${COLOR_NAMES[i]}"></button>`).join('')
      + '</div>';
    colorPop.classList.add('open');
    const a = anchor.getBoundingClientRect(), w = colorPop.offsetWidth, hgt = colorPop.offsetHeight;
    const left = clamp(a.left + a.width / 2 - w / 2, 8, innerWidth - w - 8);
    const below = a.bottom + 10;
    colorPop.style.left = `${left}px`;
    colorPop.style.top = `${below + hgt > innerHeight - 8 ? a.top - hgt - 10 : below}px`;
  }
  document.addEventListener('click', (e) => {
    const sw = e.target.closest('.swatch');
    if (sw) { backend.updateSettings({ color: Number(sw.dataset.color) }); closeColorPop(); return; }
    const av = e.target.closest('[data-act=color]');
    if (av) { colorPop.classList.contains('open') ? closeColorPop() : openColorPop(av); return; }
    if (!e.target.closest('.color-pop')) closeColorPop();
  });
  document.addEventListener('keydown', (e) => { if (e.key === 'Escape') closeColorPop(); });
  window.addEventListener('resize', closeColorPop);

  /* ── home: my name and instrument ── */
  const homeName = $('#home-name');
  const homeInst = $('#home-inst');
  let homeFocused = false;
  let nameTimer = null;
  function syncHomeProfile() {
    const s = backend.getSettings();
    const first = !s.name || s.name === '나';
    if (document.activeElement !== homeName) homeName.value = first ? '' : s.name;
    $('#home-avatar').textContent = initial(first ? '?' : s.name);
    $('#home-avatar').style.setProperty('--hue', myHue());
    $('#me-row').classList.toggle('first', first);
    $('#me-row').classList.toggle('loading', !backend.ready);
    // first run: put the cursor in the name box (once the saved name is known)
    if (backend.ready && !homeFocused) {
      homeFocused = true;
      if (first && $('#screen-home').classList.contains('active')) homeName.focus();
    }
    if (document.activeElement !== homeInst) homeInst.value = s.instrument || '';
  }
  homeName.addEventListener('input', () => {
    clearTimeout(nameTimer);
    const v = homeName.value.trim();
    $('#home-avatar').textContent = initial(v || '?');
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
    $('#nearby-empty').hidden = rooms.length > 0;
    $('#nearby-list').innerHTML = rooms.map((r) => {
      const full = r.members >= r.max_peers;
      return `<button class="nearby-room" data-code="${esc(r.code)}" ${full ? 'disabled' : ''}>
        <span class="avatar sm" style="--hue:${hue(r.host)}">${esc(initial(r.host))}</span>
        <span class="nr-text"><b>${esc(r.name || '이름 없는 방')}</b><span>${esc(r.host)} · ${r.members}/${r.max_peers}명${full ? ' · 가득 참' : ''}</span></span>
        ${r.locked ? '<span class="nr-lock" data-icon="lock" title="비밀번호"></span>' : ''}
        <span class="nr-go">${full ? '' : '참여'}</span>
      </button>`;
    }).join('');
    hydrateIcons($('#nearby-list'));
  }
  $('#nearby-list').addEventListener('click', (e) => {
    const b = e.target.closest('.nearby-room');
    if (b && !b.disabled) join(b.dataset.code);
  });

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
    if (byHand) { btn.disabled = true; out.hidden = false; out.textContent = '확인하는 중…'; }
    try {
      const u = await backend.checkUpdate();
      showUpdate(u);
      if (byHand) {
        out.textContent = u ? `새 버전 ${u.version}이 나왔어요` : '지금 버전이 최신이에요';
        if (u) openModal('modal-update');
      }
    } catch (e) {
      // at startup: say nothing (offline is no news)
      if (byHand) out.textContent = String(e?.message || e || '확인하지 못했어요');
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
  function openSettings(tab = 'audio') {
    selectTab(tab);
    syncSettings();
    openModal('modal-settings');
    // outside a room, open the audio device so the input meter and test tone work
    if (!room) Promise.resolve(backend.setPreview?.(true)).catch((e) => toast(e.message));
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
    if (tab === 'network' && !netInfo) runNetCheck();
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
    toast('테스트 톤 재생 중 (1 kHz)');
    Promise.resolve(backend.playTestTone()).catch((e) => toast(e.message));
  });
  $('#net-recheck').addEventListener('click', runNetCheck);

  /** DAW mode: whether the gachi plugin is connected (from the engine, every ~0.5 s). */
  function syncPluginStatus() {
    const daw = backend.getSettings().connection === 'daw';
    const st = $('#daw-status');
    st.hidden = !daw;
    st.textContent = (backend._plugin ? '● 플러그인 연결됨' : '○ 플러그인 기다리는 중') + (backend._pluginWaiting ? ' · 다른 트랙 1개 쉬는 중' : '');
    st.className = 'aside ' + (backend._plugin && !backend._pluginWaiting ? 'q-good' : 'q-warn');
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
        ? '⚠ 상대 소리가 gachi 플러그인 입력으로 다시 들어가서 상대에게 되돌아가고 있어요. DAW에서 gachi 출력(또는 멀티 출력 트랙, 루프백)이 gachi 플러그인 앞으로 들어가지 않게 해 주세요.'
        : '⚠ 상대 소리가 입력으로 다시 들어가서 상대에게 되돌아가고 있어요. 루프백 채널을 입력으로 골랐거나 스피커 소리가 마이크로 들어가는 것 같아요. 헤드폰을 쓰거나 입력 채널을 바꿔 주세요.');
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
      toast('gachi 플러그인이 여러 트랙에 꽂혀 있어요. 먼저 켜진 하나만 소리를 보내고, 나머지는 소리를 그대로 통과시켜요. 보낼 트랙들은 그룹 트랙으로 모아 그 끝에 하나만 꽂아 주세요.');
    }
    waitingShown = w;
  }

  /** The device's format, as it runs right now (set in the ASIO panel / Windows). */
  /** What follows from the packet size in use (automatic: it follows the device's block,
   *  so this is drawn again as the device's figures come in). */
  function syncPacket(s) {
    const frames = backend.packetFrames();
    const pkt = backend.frameMs(frames);
    $('#pkt-ms').textContent = s.packetSize ? `${f1(pkt)} ms / 패킷` : `자동 · ${frames} · ${f1(pkt)} ms / 패킷`;
    // what one stream really costs on the wire: payload + audio header (12) + encryption (28) + IP/UDP (28)
    const ch = s.inputChannel.includes('-') ? 2 : 1; // a pair (or mic + pair) is stereo
    const pps = s.sampleRate / frames;
    const payload = frames * ch * (s.quality === 'pcm16' ? 2 : 3);
    const bw = (payload + 68) * 8 * pps;
    $('#q-bw').textContent = `상대 1명당 ${bw >= 1e6 ? f1(bw / 1e6) + ' Mbps' : Math.round(bw / 1000) + ' kbps'}`;
    // upload grows with the room: everyone sends to everyone
    const mb = (n) => f1((bw * (n - 1)) / 1e6);
    const here = room ? `지금 방(${peers.size + 1}명)은 약 ${mb(peers.size + 1)} Mbps, ` : '';
    $('#q-note').textContent = `압축 없이 그대로 보내요 (코덱 지연 0 ms). 업로드가 ${here}8명 방이면 약 ${mb(8)} Mbps, 16명 방이면 약 ${mb(16)} Mbps 필요해요. ${s.quality === 'pcm16' ? '더 높은 음질을 원하면 24bit를 골라 보세요(업로드가 약 1.4배 필요해요).' : '업로드가 부족하면 16bit로 줄여 보세요.'}`;
    $('#jb-val').textContent = `${s.jitterPackets} · ${f1(s.jitterPackets * pkt)}ms`;
  }

  function syncBuffer(s) {
    const rate = backend._sampleRate, frames = backend._bufferFrames;
    $('#fmt-rate').textContent = rate ? `${rate % 1000 ? (rate / 1000).toFixed(1) : rate / 1000} kHz` : '-';
    $('#fmt-buf').textContent = frames && rate ? `${frames} 샘플 · ${f1((frames / rate) * 1000)} ms` : '-';
    // the real latency: more than the buffer (USB transfer, the driver's safety margin)
    const lat = backend._latency;
    const daw = s.connection === 'daw';
    $('#fmt-lat-k').textContent = !lat ? '장치 지연' : daw ? '장치 지연 (DAW 블록 기준 추정)' : lat?.reported ? '장치 지연 (드라이버 보고)' : '장치 지연 (버퍼 기준, 드라이버가 알려주지 않음)';
    $('#fmt-lat').textContent = lat ? `입력 ${f1(lat.inMs)} ms · 출력 ${f1(lat.outMs)} ms` : '-';
    $('#fmt-note').textContent = s.connection === 'daw'
      ? 'DAW의 오디오 설정에서 바꾸면 여기에 반영돼요.'
      : '샘플레이트와 버퍼 크기는 컨트롤 패널(위 버튼이나 제조사 프로그램)에서 바꾸면 몇 초 안에 여기에 반영돼요.';
  }
  const segValue = (s, key) => String(s[key]);
  function syncSettings() {
    const s = backend.getSettings();
    $$('#modal-settings .seg[data-key]').forEach((seg) => {
      $$('button', seg).forEach((b) => b.classList.toggle('on', segValue(s, seg.dataset.key) === b.dataset.v));
    });
    const daw = s.connection === 'daw';
    $$('#modal-settings [data-device-only]').forEach((el) => { el.hidden = daw; });
    $('#conn-note').innerHTML = daw
      ? 'DAW가 오디오 인터페이스를 쓰고, gachi 플러그인이 소리를 주고받아요. 보낼 트랙들을 <b>그룹 트랙</b>으로 모으고 그 맨 끝에 gachi 플러그인을 꽂으세요. 샘플레이트와 버퍼는 DAW 설정을 따라요.<br>플러그인(gachi, VST3)은 내려받기 페이지에서 따로 받아 DAW의 VST3 폴더에 넣어요. <a href="#" id="plugin-get">내려받기 페이지 열기</a>'
      : 'gachi가 ASIO 장치를 직접 열어요. DAW와 같이 쓰려면 VST 연결을 고르세요 (드라이버가 한 프로그램만 허용하는 경우가 많아요).';
    $('#plugin-get')?.addEventListener('click', (e) => { e.preventDefault(); backend.openRelease(); });
    syncPluginStatus();
    const devs = backend.getDevices();
    const sel = $('#set-device');
    sel.innerHTML = devs.map((d) => `<option value="${d.id}">${esc(d.label)}</option>`).join('');
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
    jbRange.value = s.jitterPackets; paintRange(jbRange);
    $('#set-nearby').checked = s.showNearby !== false;
    $('#set-gpu').checked = s.gpu !== false;
    $('#set-verbose').checked = s.verboseLog === true;
    $('#set-updates').checked = s.checkUpdates !== false;
    syncGpuRestart();
  }

  async function runNetCheck() {
    const box = $('#net-checks');
    box.innerHTML = `<div class="check"><span class="dot pending"></span><span class="v">검사 중…</span></div>`;
    netInfo = await backend.checkNetwork();
    const n = netInfo;
    const LINK = { wired: ['유선 (이더넷)', ''], wifi: ['Wi-Fi', 'warn'], unknown: ['확인 불가', 'warn'] };
    const NAT = {
      none: ['공유기 없음 (공인 IP 직결)', ''],
      cone: ['직접 연결 가능', ''],
      symmetric: ['직접 연결이 어려운 유형 (대칭형 NAT)', 'warn'],
      unknown: ['직결 여부 확인 불가', 'warn'],
      offline: ['서버 연결 안 됨', 'bad'],
    };
    // straight on the modem / wall jack: no port to open
    const port = n.nat === 'none' ? ['필요 없음 (공유기 없음)', '']
      : n.upnp ? ['자동으로 열 수 있음 (UPnP·NAT-PMP)', ''] : ['공유기가 지원 안 함', 'warn'];
    const items = [
      ['연결', ...(LINK[n.link] || LINK.unknown)],
      ['IPv6', n.ipv6 ? '사용 가능' : '없음', n.ipv6 ? '' : 'warn'],
      ['NAT', ...(NAT[n.nat] || NAT.unknown)],
      ['포트 열기', ...port],
    ];
    box.innerHTML = items.map(([k, v, c]) => `<div class="check"><span class="dot ${c}"></span><div><span class="k">${k}</span><span class="v">${v}</span></div></div>`).join('');
    renderHomeStatus();
  }

  function renderHomeStatus() {
    const s = backend.getSettings();
    const dev = backend.getDevices().find((d) => d.id === s.device);
    const daw = s.connection === 'daw';
    // (the plugin's state is known while an engine runs: in a room or with settings open)
    $('#st-audio').textContent = daw ? (backend._plugin == null ? 'VST 연결' : `VST 연결 · ${backend._plugin ? '플러그인 연결됨' : '플러그인 기다리는 중'}${backend._pluginWaiting ? ' · 여러 개 꽂힘' : ''}`)
      : backend.hasAsio() ? `${dev?.label ?? '-'} · ${s.bufferSize} samples` : 'ASIO 장치 없음';
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
        $('#st-net').textContent = '서버 연결 안 됨';
        $('#st-net-dot').className = 'dot bad';
        return;
      }
      const link = { wired: '유선', wifi: 'Wi-Fi' }[n.link];
      const direct = n.nat === 'cone' || n.nat === 'none';
      $('#st-net').textContent = [link, n.ipv6 && 'IPv6', direct ? 'P2P 가능' : '서버 연결됨'].filter(Boolean).join(' · ');
      $('#st-net-dot').className = 'dot' + (n.link === 'wired' && direct ? '' : ' warn');
    }
  }

  /* ═══════════════════════ KEYBOARD ═══════════════════════ */
  document.addEventListener('keydown', (e) => {
    if (e.key === 'Escape') {
      if (menuOpen()) return closeMenus();
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
    toast(ok ? 'ASIO 장치를 찾았어요' : '아직 ASIO 드라이버를 찾지 못했어요. 설치 후 PC를 다시 시작해야 할 수도 있어요.');
  });

  /* ═══════════════════════ INIT ═══════════════════════ */
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
