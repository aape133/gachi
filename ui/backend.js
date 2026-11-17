/*
 * gachi — backend bridge
 *
 * The UI (app.js) talks ONLY to `window.backend`. Two implementations:
 *  - TauriBackend: the desktop app. Methods are Tauri commands (app/src/commands.rs),
 *    events are emitted from Rust (app/src/session.rs).
 *  - MockBackend: simulates the audio engine + network so the UI can be designed and
 *    tested in a plain browser.
 *
 * Events:
 *   levels       { self:[l,r], master:[l,r], peers:{ id:[l,r] } }   ~30 Hz
 *   stats        { peers:[PeerView] }                                1 Hz (+ on change)
 *   peer-joined  { peer:PeerView, silent:boolean }
 *   peer-left    { id, name }
 *   chat         { from:'id', text }
 *   settings     Settings
 */
(() => {
  'use strict';

  // No 0/O, 1/I/L — easy to read aloud and type.
  const CODE_ALPHABET = 'ABCDEFGHJKMNPQRSTUVWXYZ23456789';
  const wait = (ms) => new Promise((r) => setTimeout(r, ms));

  class Emitter {
    constructor() { this._h = {}; }
    on(ev, fn) { (this._h[ev] ||= []).push(fn); return () => this.off(ev, fn); }
    off(ev, fn) { this._h[ev] = (this._h[ev] || []).filter((f) => f !== fn); }
    emit(ev, data) { (this._h[ev] || []).forEach((f) => f(data)); }
  }

  const DEVICES = {
    ASIO: [
      { id: 'focusrite', label: 'Focusrite USB ASIO', inputs: 10, inputNames: ['Analogue 1', 'Analogue 2', 'Mobile 1', 'Mobile 2', 'Loopback 1', 'Loopback 2', 'Loopback 3', 'Loopback 4', 'Loopback 5', 'Loopback 6'] },
      { id: 'volt', label: 'UA Volt ASIO' },
    ],
  };

  const DEMO_PEERS = [
    { id: 'p1', name: '지민', instrument: '보컬', hue: 320, conn: { type: 'p2p', ipv6: true, link: 'wired', encrypted: true, cipher: 'AES-256-GCM' }, netMs: 4.8, jitterMs: 0.35, loss: 0.0 },
    { id: 'p2', name: '현우', instrument: '베이스', hue: 190, conn: { type: 'p2p', ipv6: false, link: 'wifi', encrypted: true, cipher: 'AES-256-GCM' }, netMs: 6.2, jitterMs: 2.3, loss: 0.4 },
    { id: 'p3', name: '서연', instrument: '드럼', hue: 48, conn: { type: 'p2p', ipv6: false, link: 'wired', encrypted: true, cipher: 'AES-256-GCM' }, netMs: 15.5, jitterMs: 0.9, loss: 0.1 },
  ];

  /** Latency math shared by both backends (ms). */
  class BaseBackend extends Emitter {
    frameMs(n) { return (n / this.settings.sampleRate) * 1000; }
    /** Frames per packet we send. Setting 0 = automatic: what the engine uses now, else
     *  what fits the device buffer (the same rule as the engine's auto_packet_frames: the
     *  buffer itself up to 64, else its largest equal part that is not bigger). */
    packetFrames() {
      const s = this.settings;
      if (s.packetSize) return s.packetSize;
      if (this._packetFrames) return this._packetFrames;
      const b = s.bufferSize || 64;
      if (b <= 64) return Math.max(8, b);
      for (let d = 64; d >= 16; d--) if (b % d === 0) return d;
      return 64;
    }
    localMs() { return this.frameMs(this.settings.bufferSize) * 2; }
  }

  /* ═══════════════════ Native (Tauri) ═══════════════════ */
  class TauriBackend extends BaseBackend {
    constructor() {
      super();
      const { invoke } = window.__TAURI__.core;
      const { listen } = window.__TAURI__.event;
      this._invoke = invoke;
      this._listen = listen;
      this.settings = {
        device: '', sampleRate: 48000, bufferSize: 64, inputChannel: '1-2',
        quality: 'pcm16', packetSize: 0, jitterMode: 'auto', jitterStyle: 'balanced', jitterPackets: 3, name: '나', instrument: '기타',
        metronomeVolume: 60, upnp: true, theme: 'system', showNearby: true, connection: 'device', recFormat: 'wav', recBits: 16, recMix: true,
      };
      this.self = { gain: 0, muted: false, monitor: false, monitorDb: 0, own: true, rec: true };
      this._devices = { devices: [], hasAsio: true };
      this.devicesLoaded = false;
      ['levels', 'stats', 'peer-joined', 'peer-left', 'chat', 'settings', 'notice', 'room', 'room-lost', 'kicked', 'metronome', 'recording'].forEach((ev) =>
        listen(ev, (e) => {
          if (ev === 'settings') this.settings = e.payload;
          // in a room: our real input+output buffer (WASAPI often differs from the setting)
          if (ev === 'stats' && typeof e.payload.localMs === 'number') this._localMs = e.payload.localMs;
          // the device's real format (it decides: ASIO panel / Windows settings)
          if (ev === 'stats' && e.payload.bufferFrames) this._bufferFrames = this.settings.bufferSize = e.payload.bufferFrames;
          if (ev === 'stats' && e.payload.packetFrames) this._packetFrames = e.payload.packetFrames;
          if (ev === 'stats' && e.payload.sampleRate) this._sampleRate = this.settings.sampleRate = e.payload.sampleRate;
          // the device's real input/output latency (the driver's report, or block-based)
          if (ev === 'stats' && e.payload.latency) this._latency = e.payload.latency;
          // DAW mode: the gachi plugin is connected (null: not in DAW mode)
          if (ev === 'stats' && 'plugin' in e.payload) this._plugin = !!e.payload.plugin;
          // another gachi plugin (a second track) waits: only one sends at a time
          if (ev === 'stats' && 'pluginWaiting' in e.payload) this._pluginWaiting = !!e.payload.pluginWaiting;
          // the others' sound comes back in what we send
          if (ev === 'stats' && 'feedback' in e.payload) this._feedback = !!e.payload.feedback;
          this.emit(ev, e.payload);
        }));
      // settings first (instant): the home screen shows the saved name right away;
      // the device list (slower: ASIO drivers load) follows
      this.ready = false;
      invoke('get_settings').then((s) => {
        this.settings = s;
        this.ready = true;
        this.emit('settings', this.getSettings());
        // the page now shows the saved name, theme and colour: let the window appear
        // (after the next paint, so the first thing seen is the finished page)
        requestAnimationFrame(() => requestAnimationFrame(() => invoke('window_ready')));
        // the app may have been opened by an invite link
        invoke('take_pending_join').then((code) => code && window.gachiJoin?.(code));
      });
      this.refreshDevices();
      listen('join-link', (e) => {
        invoke('take_pending_join');
        window.gachiJoin?.(e.payload.code);
      });
    }
    async call(cmd, args) {
      try { return await this._invoke(cmd, args); }
      catch (e) {
        const err = new Error(typeof e === 'string' ? e : e?.message || String(e));
        if (e && typeof e === 'object' && e.kind) err.kind = e.kind; // e.g. password_required
        throw err;
      }
    }
    getSettings() { return { ...this.settings }; }
    getDevices() { return this._devices.devices || []; }
    /** an ASIO driver is installed (else the home screen says how to get one) */
    hasAsio() { return !this.devicesLoaded || this._devices.hasAsio; }
    /** lists the ASIO drivers again (after installing one) */
    async refreshDevices() {
      this._devices = await this._invoke('list_devices');
      this.devicesLoaded = true;
      this.emit('settings', this.getSettings());
      return this._devices.hasAsio;
    }
    updateSettings(patch) {
      Object.assign(this.settings, patch);
      this.emit('settings', this.getSettings());
      this.call('update_settings', { patch }).catch((e) => console.error(e));
    }
    checkNetwork() { return this.call('check_network'); }
    /** rooms open on this network: [{code, name, host, locked, members, max_peers}] */
    nearbyRooms() { return this.call('nearby_rooms'); }
    /** open a connection to the server ahead of time (fast "방 만들기") */
    createRoom(opts) { return this.call('create_room', { opts }); }
    async joinRoom(code, onStep, password) {
      const un = await this._listen('connect-step', (e) => onStep?.(e.payload.id, e.payload.state, e.payload.detail));
      try { return await this.call('join_room', { code, password: password || null }); } finally { un(); }
    }
    cancelJoin() { this.call('cancel_join'); }
    leaveRoom() { this._localMs = null; this.call('leave_room'); }
    localMs() { return this._localMs ?? super.localMs(); }
    // host: code on/off, size, new code → the app answers with a 'room' event
    setRoom({ open, maxPeers, newCode } = {}) {
      return this.call('set_room', { open: open ?? null, maxPeers: maxPeers ?? null, newCode: !!newCode });
    }
    setPeer(id, patch) { this.call('set_peer', { id, patch }); }
    /** host: put a member out of the room (they cannot come back in) */
    kickPeer(id) { this.call('kick_peer', { id }).catch((e) => this.emit('notice', { text: e.message })); }
    setSelf(patch) { Object.assign(this.self, patch); this.call('set_self', { patch }); }
    setMaster(db) { this.call('set_master', { db }); }
    sendChat(text) { this.call('send_chat', { text }); }
    // the server answers everyone (us too) with a shared start time → 'metronome' event
    setMetronome({ on, bpm, beats, countIn = 0 }) { this.call('set_metronome', { on, bpm, beats, countIn }).catch((e) => console.error(e)); }
    // settings screen outside a room: open the device for the input meter / test tone
    setPreview(on) { return this.call('set_preview', { on }); }
    probeDevice() { return this.call('probe_device'); }
    restartApp() { return this.call('restart_app'); }
    /** the ASIO driver's own settings window */
    openControlPanel() { return this.call('open_control_panel'); }
    setRecording(on) { return this.call('set_recording', { on }); }
    playTestTone() { return this.call('play_test_tone'); }
    openRecordings() { return this.call('open_recordings'); }
    openLogs() { this.call('open_logs').catch(() => {}); }
    openRepo() { this.call('open_repo').catch(() => {}); }
    appVersion() { return this.call('app_version'); }
    /** a newer version, if one is out: { version } or null */
    checkUpdate() { return this.call('check_update'); }
    openRelease() { this.call('open_release').catch(() => {}); }
    qrSvg(text) { return this.call('qr_svg', { text }); }
  }

  /* ═══════════════════ Mock (browser preview) ═══════════════════ */
  class MockBackend extends BaseBackend {
    constructor() {
      super();
      this.ready = true;
      this._sampleRate = 48000; this._bufferFrames = 64;
      this.settings = {
        device: 'focusrite', sampleRate: 48000, bufferSize: 64, inputChannel: '1-2',
        quality: 'pcm16', packetSize: 0, jitterMode: 'auto', jitterStyle: 'balanced', jitterPackets: 3,
        name: '나', instrument: '기타', metronomeVolume: 60, upnp: true, theme: 'system', showNearby: true, connection: 'device', recFormat: 'wav', recBits: 16, recMix: true,
      };
      this.self = { gain: 0, muted: false, monitor: false, monitorDb: 0, own: true, rec: true };
      this.master = 0;
      this.room = null;
      this.peers = new Map();
      this._timers = [];
      this._joinToken = 0;
      this._levels = {};
      this._t0 = performance.now();
      setInterval(() => this._tick(), 33);
      setInterval(() => this._drift(), 1000);
    }

    /* ── settings ── */
    getSettings() { return { ...this.settings }; }
    getDevices() { return DEVICES.ASIO; }
    hasAsio() { return !/[?&]noasio/.test(location.search); }
    async refreshDevices() { this.emit('settings', this.getSettings()); return this.hasAsio(); }
    updateSettings(patch) {
      Object.assign(this.settings, patch);
      this.emit('settings', this.getSettings());
      this._emitStats();
    }
    async checkNetwork() {
      await wait(900);
      return { link: 'wired', ipv6: true, nat: 'cone', upnp: true, serverRtt: 7 };
    }

    /* ── latency model (ms) ── */
    autoPackets(p) {
      const pkt = this.frameMs(this.packetFrames());
      return Math.min(12, Math.max(1, Math.ceil((p.jitterMs * 2.5) / pkt)));
    }
    _view(p) {
      const s = this.settings;
      const packets = p.jitterAuto
        ? (s.jitterMode === 'manual' ? s.jitterPackets : this.autoPackets(p))
        : p.jitterPackets;
      const b = {
        inBuf: (64 / 48000) * 1000, // peer's own input buffer (reported by peer)
        net: p.netNow,
        jitter: packets * this.frameMs(this.packetFrames()),
        outBuf: this.frameMs(s.bufferSize),
      };
      const total = Object.values(b).reduce((a, v) => a + v, 0);
      const { speed, base, netNow, ...rest } = p;
      const stream = { bits: 24, channels: 2, sampleRate: 48000, packetFrames: 64 };
      return { ...rest, stream, packets, autoPackets: this.autoPackets(p), breakdown: b, total, rtt: netNow * 2 };
    }

    /* ── rooms ── */
    getRoom() { return this.room && { ...this.room, peers: [...this.peers.values()].map((p) => this._view(p)) }; }

    async createRoom({ name, maxPeers = 4, password = '' } = {}) {
      await wait(350);
      let code = '';
      for (let i = 0; i < 6; i++) code += CODE_ALPHABET[Math.floor(Math.random() * CODE_ALPHABET.length)];
      this._openRoom({ code, name: name || '새 합주실', maxPeers, isHost: true, hasPassword: !!password });
      // ?peers=15: a full room at once (to look at big rooms)
      const many = +new URLSearchParams(location.search).get('peers') || 0;
      if (many) {
        const names = ['지민', '현우', '서연', '도윤', '하은', '준서', '수아', '민재', '예린', '태오', '유나', '시우', '채원', '건우', '다인'];
        const insts = ['보컬', '베이스', '드럼', '기타', '건반', '색소폰', '바이올린', '코러스'];
        for (let i = 0; i < many; i++) {
          const d = DEMO_PEERS[i % DEMO_PEERS.length];
          this._later(300 + i * 120, () => this._addPeer({ ...d, id: 'p' + (i + 1), name: names[i % names.length], instrument: insts[i % insts.length], hue: (i * 47 + 20) % 360 }));
        }
        return this.getRoom();
      }
      this._later(3000, () => this._addPeer(DEMO_PEERS[0]));
      this._later(6500, () => this._addPeer(DEMO_PEERS[1]));
      this._later(10000, () => this.emit('chat', { from: 'p1', text: '다들 들려요? 한 곡 가볼까요' }));
      return this.getRoom();
    }

    async joinRoom(code, onStep, password) {
      // demo: code PWPW22 is a password room (password 1234)
      if (code === 'PWPW22' && password !== '1234') {
        const err = new Error(password ? '비밀번호가 맞지 않아요.' : '비밀번호가 있는 방이에요.');
        err.kind = password ? 'wrong_password' : 'password_required';
        throw err;
      }
      const token = ++this._joinToken;
      const steps = [
        ['signal', 420, '7 ms'],
        ['exchange', 480, '후보 5개'],
        ['punch', 700, 'IPv6 직결'],
        ['measure', 650, '평균 10.4 ms'],
      ];
      for (const [id, ms, detail] of steps) {
        onStep?.(id, 'doing');
        await wait(ms);
        if (token !== this._joinToken) throw new Error('cancelled');
        if (id === 'signal' && code === 'ZZZZZZ') throw new Error('방을 찾을 수 없어요. 코드를 다시 확인해 주세요.');
        onStep?.(id, 'done', detail);
      }
      await wait(250);
      if (token !== this._joinToken) throw new Error('cancelled');
      this._openRoom({ code, name: '금요일 합주', maxPeers: 4, isHost: false });
      this._addPeer(DEMO_PEERS[0], { isHost: true, silent: true });
      this._addPeer(DEMO_PEERS[1], { silent: true });
      this._later(4000, () => this.emit('chat', { from: 'p1', text: '왔구나! 소리 잘 들려요?' }));
      this._later(8000, () => this._addPeer(DEMO_PEERS[2]));
      return this.getRoom();
    }
    cancelJoin() { this._joinToken++; }

    leaveRoom() {
      this._timers.forEach(clearTimeout);
      this._timers = [];
      this.peers.clear();
      this.room = null;
      this.recording = false;
    }

    _openRoom(r) {
      this.leaveRoom();
      this.room = { open: true, ...r, startedAt: Date.now(), safetyCode: '4829 1305 7716' };
    }
    _later(ms, fn) { this._timers.push(setTimeout(fn, ms)); }
    _addPeer(def, { isHost = false, silent = false } = {}) {
      if (!this.room || this.peers.size + 1 >= this.room.maxPeers) return;
      const p = {
        ...def, isHost, volume: 0, pan: 0, mute: false, solo: false, rec: true,
        jitterAuto: true, jitterPackets: 3, netNow: def.netMs,
        speed: 1.2 + Math.random() * 2, base: 0.5 + Math.random() * 0.3,
      };
      this.peers.set(p.id, p);
      this.emit('peer-joined', { peer: this._view(p), silent });
    }

    kickPeer(id) { if (this.peers.delete(id)) this.emit('peer-left', { id }); }
    /* ── mixer ── */
    setPeer(id, patch) {
      const p = this.peers.get(id);
      if (!p) return;
      if ('jitterPackets' in patch) patch.jitterPackets = Math.min(12, Math.max(1, patch.jitterPackets));
      Object.assign(p, patch);
      this._emitStats();
    }
    setSelf(patch) { Object.assign(this.self, patch); }
    setMaster(db) { this.master = db; }
    sendChat(text) { /* would go over the data channel */ }
    setMetronome(m) { this.metronome = m; }
    async setRecording(on) {
      this.recording = on;
      if (on) { this._recStart = Date.now(); return { recording: true }; }
      return { recording: false, files: this.peers.size + 1, seconds: (Date.now() - this._recStart) / 1000, dir: '문서/gachi/녹음' };
    }
    async playTestTone() { /* native: plays a 1 kHz sine on the output */ }
    async openRecordings() {}
    openLogs() {}
    openRepo() {}
    async appVersion() { return '0.2.6'; }
    // (preview: add ?update to the address to see a newer version announced)
    async checkUpdate() {
      await new Promise((r) => setTimeout(r, 300));
      if (!new URLSearchParams(location.search).has('update')) return null;
      return { version: '0.2.7' };
    }
    openRelease() {}
    async setPreview() {}
    async probeDevice() {}
    async restartApp() { location.reload(); }
    async setRoom({ open, maxPeers, newCode } = {}) {
      if (!this.room) return;
      const count = this.peers.size + 1;
      if (maxPeers != null) this.room.maxPeers = Math.min(16, Math.max(count, 2, maxPeers));
      if (open != null) this.room.open = open;
      if (newCode) this.room.code = Array.from({ length: 6 }, () => 'ABCDEFGHJKMNPQRSTUVWXYZ23456789'[Math.floor(Math.random() * 31)]).join('');
      this.emit('room', this.getRoom());
    }
    async nearbyRooms() {
      return [{ code: 'K7MQ2X', name: '금요일 밤 합주', host: '지민', locked: false, members: 2, max_peers: 4 },
        { code: 'P3RT8W', name: '밴드 연습', host: '현우', locked: true, members: 4, max_peers: 4 }];
    }
    async qrSvg() { return null; } // browser preview draws it with qrcode.js instead

    /* ── simulation ── */
    _drift() {
      for (const p of this.peers.values()) {
        const target = p.netMs + (Math.random() - 0.5) * p.jitterMs * 0.6;
        p.netNow += (target - p.netNow) * 0.5;
        p.loss = Math.max(0, +(p.loss + (Math.random() - 0.5) * 0.1).toFixed(2));
      }
      this._emitStats();
    }
    _emitStats() {
      if (!this.room) return;
      this.emit('stats', { peers: [...this.peers.values()].map((p) => this._view(p)) });
    }
    _tick() {
      const t = (performance.now() - this._t0) / 1000;
      const gen = (key, active, speed, base, phase) => {
        const prev = this._levels[key] || [0, 0];
        let target = 0;
        if (active) {
          const env = Math.sin(t * speed + phase) * 0.5 + 0.5;
          target = base * (0.45 + 0.55 * env) * (0.8 + Math.random() * 0.2);
          if (Math.sin(t * speed * 0.21 + phase) < -0.75) target *= 0.15; // phrase rests
        }
        const step = (cur, tgt) => cur + (tgt - cur) * (tgt > cur ? 0.6 : 0.12);
        const lv = [step(prev[0], target), step(prev[1], target * (0.9 + Math.random() * 0.1))];
        this._levels[key] = lv;
        return lv;
      };
      const gain = Math.min(1.6, Math.pow(10, this.self.gain / 20));
      const self = gen('self', !this.self.muted, 2.3, Math.min(0.98, 0.6 * gain), 0);
      const peers = {};
      let i = 1;
      for (const p of this.peers.values()) peers[p.id] = gen(p.id, true, p.speed, p.base, i++ * 1.7);
      const all = [self, ...Object.values(peers)];
      const m = Math.pow(10, this.master / 20);
      const master = [0, 1].map((c) => Math.min(1, Math.max(...all.map((l) => l[c])) * m));
      // demo interface: mic on input 1, PC audio on the loopback pair 5/6
      const bg = gen('loop', true, 1.1, 0.45, 3.3);
      const inputs = Array.from({ length: 10 }, (_, c) => (c === 0 ? self[0] : c === 4 ? bg[0] : c === 5 ? bg[1] : 0));
      this.emit('levels', { self, peers, master, inputs });
    }
  }

  // Inside the desktop app Tauri injects window.__TAURI__ (withGlobalTauri); in a plain browser use the mock.
  window.backend = window.__TAURI__ ? new TauriBackend() : new MockBackend();
  window.CODE_ALPHABET = CODE_ALPHABET;
})();
