// End-to-end tests against a local Worker (`wrangler dev`), over real WebSockets.
// Run: npm test   (starts the Worker itself on a free port)
import { test, before, after } from "node:test";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";

const PORT = 8799;
const BASE = `127.0.0.1:${PORT}`;
// (the tests before the one about it make about 60 attempts in the first minute)
const LIMIT = 90;
let dev;

before(async () => {
  dev = spawn(
    "npx",
    ["wrangler", "dev", "--port", String(PORT), "--ip", "127.0.0.1", "--var", `ATTEMPTS_PER_MIN:${LIMIT}`, "--var", "HOST_CHECK_MS:8000", "--var", "HOST_STALE_MS:4000"],
    { cwd: new URL("..", import.meta.url), shell: true, env: { ...process.env, WRANGLER_SEND_METRICS: "false" } },
  );
  // wait until it answers
  for (let i = 0; i < 120; i++) {
    try {
      if ((await fetch(`http://${BASE}/health`)).ok) return;
    } catch {}
    await sleep(500);
  }
  throw new Error("wrangler dev did not start");
});

after(() => {
  if (process.platform === "win32") spawn("taskkill", ["/pid", String(dev.pid), "/T", "/F"]);
  else dev.kill();
});

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/** A WebSocket with a message queue: `await c.next()` gets the next JSON message. */
async function connect(path) {
  const ws = new WebSocket(`ws://${BASE}${path}`);
  const queue = [];
  const waiters = [];
  ws.onmessage = (e) => {
    const m = JSON.parse(e.data);
    const w = waiters.shift();
    w ? w(m) : queue.push(m);
  };
  let closed = false;
  ws.onclose = () => {
    closed = true;
    while (waiters.length) waiters.shift()(null);
  };
  await new Promise((res, rej) => {
    ws.onopen = res;
    ws.onerror = rej;
  });
  return {
    ws,
    send: (m) => ws.send(JSON.stringify(m)),
    next: (ms = 3000) =>
      queue.length
        ? Promise.resolve(queue.shift())
        : closed
          ? Promise.resolve(null)
          : new Promise((res) => {
              waiters.push(res);
              setTimeout(() => res("timeout"), ms);
            }),
    close: () => ws.close(),
    get closed() {
      return closed;
    },
  };
}

const JOIN_KEY = "ab".repeat(32);
const joinMsg = (pw = false) => ({ t: "join", eph: "11".repeat(32), pw, sealed: "c0ffee" });
const hostOpen = (extra) => ({ t: "host_open", code: null, host_key: null, max_peers: 4, members: 1, join_key: JOIN_KEY, locked: false, ...extra });

/** A newcomer: connects, and gets the host's join key first. */
async function joiner(code) {
  const c = await connect(`/ws/join/${code}`);
  const hello = await c.next();
  assert.deepEqual(hello, { t: "host", join_key: JOIN_KEY, locked: hello.locked });
  return c;
}

async function openRoom(maxPeers = 4, locked = false) {
  const host = await connect("/ws/host");
  host.send(hostOpen({ max_peers: maxPeers, locked }));
  const opened = await host.next();
  assert.equal(opened.t, "opened");
  assert.match(opened.code, /^[A-HJ-NP-Z2-9]{6}$/);
  return { host, code: opened.code, key: opened.host_key };
}

test("host admits and rejects; the newcomer only ever talks to the Worker briefly", async () => {
  const { host, code } = await openRoom(4, true);

  // the newcomer learns the host's key and that the room is locked
  const a = await joiner(`${code.slice(0, 3)}-${code.slice(3).toLowerCase()}`);
  a.send({ ...joinMsg(true), password: "never forwarded", profile: { name: "x" } });
  const req1 = await host.next();
  assert.deepEqual(req1, { t: "join_request", req: req1.req, eph: "11".repeat(32), pw: true, sealed: "c0ffee" }, "only the sealed parts");
  host.send({ t: "admit", req: req1.req, result: "reject", kind: "wrong_password", message: "비번" });
  assert.deepEqual(await a.next(), { t: "error", kind: "wrong_password", message: "비번" });

  // accepted → the sealed welcome, then the Worker closes the newcomer's connection
  const b = await joiner(code);
  b.send(joinMsg(true));
  const req2 = await host.next();
  host.send({ t: "admit", req: req2.req, result: "accept", welcome: { sealed: "beef", extra: "dropped" } });
  assert.deepEqual(await b.next(), { t: "welcome", sealed: "beef" });
  // (the app closes the connection itself at this point; one that does not is closed)
  await sleep(2500);
  assert.ok(b.closed, "newcomer is done with the Worker");
  host.close();
});

test("JSON that is no message is refused, and an answer without a request number reaches nobody", async () => {
  const { host, code } = await openRoom();
  // a newcomer that has not asked yet: an `admit` without `req` must not be taken as its
  // answer (it was: the newcomer was welcomed and done with, and its request then ignored)
  const waiting = await joiner(code);
  host.send({ t: "admit", result: "accept", welcome: { sealed: "beef" } });
  await sleep(300);
  waiting.send(joinMsg());
  assert.equal((await host.next()).t, "join_request", "it can still ask");
  // `null` and a list parse, and are no messages
  const a = await joiner(code);
  a.ws.send("null");
  assert.equal((await a.next())?.kind, "bad_request");
  const b = await joiner(code);
  b.ws.send("[1]");
  assert.equal((await b.next())?.kind, "bad_request");
  // the room is as it was
  const c = await joiner(code);
  c.send(joinMsg());
  assert.equal((await host.next()).t, "join_request");
  waiting.close();
  c.close();
  host.close();
});

test("a room for 32: open until the thirty-second is in; a bigger size is cut to 32", async () => {
  const { host, code } = await openRoom(32);
  host.send({ t: "capacity", max_peers: 32, members: 31 });
  await sleep(100);
  const last = await joiner(code);
  last.close();
  host.send({ t: "capacity", max_peers: 32, members: 32 });
  await sleep(100);
  const full = await connect(`/ws/join/${code}`);
  assert.equal((await full.next()).kind, "full");
  // asked for 40: kept at 32, so 32 members still fill it
  host.send({ t: "capacity", max_peers: 40, members: 32 });
  await sleep(100);
  const still = await connect(`/ws/join/${code}`);
  assert.equal((await still.next()).kind, "full");
  host.send({ t: "close" });
});

test("a room for 16 (today's app) fills at 16 as before", async () => {
  const { host, code } = await openRoom(16);
  host.send({ t: "capacity", max_peers: 16, members: 16 });
  await sleep(100);
  const full = await connect(`/ws/join/${code}`);
  assert.equal((await full.next()).kind, "full");
  host.send({ t: "close" });
});

test("full rooms, turned-off codes and unknown codes are refused by the Worker", async () => {
  const { host, code, key } = await openRoom(2);
  host.send({ t: "capacity", max_peers: 2, members: 2 });
  await sleep(100);
  const full = await connect(`/ws/join/${code}`);
  assert.equal((await full.next()).kind, "full", "refused right when connecting");

  // code off: the room is forgotten
  host.send({ t: "close" });
  await sleep(200);
  const gone = await connect(`/ws/join/${code}`);
  assert.equal((await gone.next()).kind, "not_found");

  // an old app without a join key cannot open a code
  const old = await connect("/ws/host");
  old.send({ t: "host_open", code: null, host_key: null, max_peers: 4, members: 1 });
  assert.equal((await old.next()).kind, "bad_request");

  // reopen the same code (it is free now)
  const again = await connect(`/ws/host?code=${code}`);
  again.send(hostOpen({ code, host_key: key, members: 2 }));
  const opened = await again.next();
  assert.equal(opened.t, "opened");
  assert.equal(opened.code, code);
  again.send({ t: "close" });

  const bogus = await connect(`/ws/join/ZZZZZZ`);
  assert.equal((await bogus.next()).kind, "not_found");
  const bad = await connect(`/ws/join/abc`);
  assert.equal((await bad.next()).kind, "not_found");
});

test("only the key holder can take over a code; a dropped host keeps it for a while", async () => {
  const { host, code, key } = await openRoom();

  const thief = await connect(`/ws/host?code=${code}`);
  thief.send(hostOpen({ code, host_key: "wrong" }));
  assert.equal((await thief.next()).kind, "code_taken");

  // the host's connection drops without turning the code off
  host.close();
  await sleep(300);
  const meanwhile = await connect(`/ws/join/${code}`);
  const m = await meanwhile.next();
  assert.equal(m.kind, "not_found");
  assert.match(m.message, /방장 연결/);

  // the next host (who has the key) takes the code back within the grace period
  const next = await connect(`/ws/host?code=${code}`);
  next.send(hostOpen({ code, host_key: key }));
  assert.deepEqual(await next.next(), { t: "opened", code, host_key: key });
  next.send({ t: "close" });
});

test("a new key for the code: the old one no longer takes it over, the new one does", async () => {
  const { host, code, key } = await openRoom();
  host.send({ t: "rekey" });
  const m = await host.next();
  assert.equal(m.t, "rekeyed");
  assert.match(m.host_key, /^[0-9a-f]{32}$/);
  assert.notEqual(m.host_key, key);

  // someone put out (who had the old key) cannot take the code
  const kicked = await connect(`/ws/host?code=${code}`);
  kicked.send(hostOpen({ code, host_key: key }));
  assert.equal((await kicked.next()).kind, "code_taken");

  // the next host, told the new key, can
  host.close();
  await sleep(300);
  const next = await connect(`/ws/host?code=${code}`);
  next.send(hostOpen({ code, host_key: m.host_key }));
  assert.deepEqual(await next.next(), { t: "opened", code, host_key: m.host_key });
  next.send({ t: "close" });

  // (a host connection that has not opened a room cannot ask for one)
  const stranger = await connect(`/ws/host?code=${code}`);
  stranger.send({ t: "rekey" });
  assert.equal(await stranger.next(500), "timeout");
  stranger.close();
});

test("one who gives a room's password wrong too often is kept from that room only", async () => {
  const { host, code } = await openRoom(4, true);
  const other = await openRoom(4, true);
  // five wrong passwords: each reaches the host, which says so
  for (let i = 0; i < 5; i++) {
    const j = await joiner(code);
    j.send(joinMsg(true));
    const r = await host.next();
    assert.equal(r.t, "join_request");
    host.send({ t: "admit", req: r.req, result: "reject", kind: "wrong_password", message: "비밀번호가 맞지 않아요." });
    assert.equal((await j.next()).kind, "wrong_password");
    j.close();
  }
  await sleep(100);
  // the sixth is refused before it reaches the host
  const locked = await connect(`/ws/join/${code}`);
  const m = await locked.next();
  assert.equal(m.kind, "rate_limited");
  assert.match(m.message, /비밀번호/);
  assert.equal(await host.next(300), "timeout", "the host heard nothing of it");
  locked.close();
  // another room is not affected
  const elsewhere = await joiner(other.code);
  elsewhere.send(joinMsg(true));
  assert.equal((await other.host.next()).t, "join_request");
  elsewhere.close();
  host.send({ t: "close" });
  other.host.send({ t: "close" });
});

test("a host that leaves before its room heard the new key: the next one gets in with the old", async () => {
  const { host, code, key } = await openRoom();
  host.send({ t: "rekey" });
  const fresh = (await host.next()).host_key;
  host.close();
  await sleep(300);
  // the next host only has the old key: let in once, and told the current one
  const next = await connect(`/ws/host?code=${code}`);
  next.send(hostOpen({ code, host_key: key }));
  assert.deepEqual(await next.next(), { t: "opened", code, host_key: fresh });
  // while it holds the code, the old key takes nothing
  const other = await connect(`/ws/host?code=${code}`);
  other.send(hostOpen({ code, host_key: key }));
  assert.equal((await other.next()).kind, "code_taken");
  other.close();
  // nor after it leaves (it was used)
  next.close();
  await sleep(300);
  const again = await connect(`/ws/host?code=${code}`);
  again.send(hostOpen({ code, host_key: key }));
  assert.equal((await again.next()).kind, "code_taken");
  again.close();
});

const GW = "0123456789abcdef0123456789abcdef";
const OTHER_GW = "f".repeat(32);

test("nearby rooms: listed on the same network and subnet, gone when the code is off", async () => {
  const nearby = async (lan, gw = GW) => (await (await connect(`/ws/nearby?lan=${lan}${gw ? `&gw=${gw}` : ""}`)).next()).rooms;
  const host = await connect("/ws/host");
  host.send(hostOpen());
  const { code } = await host.next();

  // a room that does not say which router it is behind is not listed at all
  host.send({ t: "lobby", lobby: { name: "금요일 합주", host: "A", lan: "192.168.0", locked: true } });
  await sleep(200);
  assert.equal((await nearby("192.168.0")).some((r) => r.code === code), false, "no router value: not listed");

  host.send({ t: "lobby", lobby: { name: "금요일 합주", host: "A", lan: "192.168.0", gw: GW, locked: true } });
  await sleep(200);
  const mine = (await nearby("192.168.0")).find((r) => r.code === code);
  assert.deepEqual(mine, { code, name: "금요일 합주", host: "A", locked: true, members: 1, max_peers: 4 });
  assert.equal((await nearby("192.168.7")).some((r) => r.code === code), false, "other subnet");
  // the same address and subnet, but behind another router (or not saying): not shown
  assert.equal((await nearby("192.168.0", OTHER_GW)).some((r) => r.code === code), false, "another router");
  // (lookups are limited per minute: the caller without a router value is left to the code)

  host.send({ t: "capacity", max_peers: 6, members: 2 });
  await sleep(200);
  assert.equal((await nearby("192.168.0")).find((r) => r.code === code).members, 2);

  host.send({ t: "close" });
  await sleep(200);
  assert.equal((await nearby("192.168.0")).some((r) => r.code === code), false, "gone after close");
});

test("nearby rooms: a dropped host's room leaves the list at once, a takeover lists it again", async () => {
  const nearby = async (lan) => (await (await connect(`/ws/nearby?lan=${lan}&gw=${GW}`)).next()).rooms;
  const listed = async (code) => (await nearby("10.0.0")).some((r) => r.code === code);
  const { host, code, key } = await openRoom();
  host.send({ t: "lobby", lobby: { name: "토요일", host: "B", lan: "10.0.0", gw: GW, locked: false } });
  await sleep(200);
  assert.ok(await listed(code));

  // the connection drops without `close`: nobody could get in, so it should not be shown
  host.close();
  await sleep(300);
  assert.equal(await listed(code), false, "off the list during the grace period");

  // the next host takes the code over: back on the list
  const next = await connect(`/ws/host?code=${code}`);
  next.send(hostOpen({ code, host_key: key }));
  await next.next();
  await sleep(200);
  assert.ok(await listed(code), "listed again after the takeover");
  next.send({ t: "close" });
});

test("only a well-formed request reaches the host", async () => {
  const { host, code } = await openRoom();
  for (const bad of [{ eph: "zz".repeat(32) }, { eph: "11".repeat(31) }, { sealed: "not hex" }, { sealed: "ab".repeat(5000) }]) {
    const j = await joiner(code);
    j.send({ ...joinMsg(), ...bad });
    const m = await j.next();
    assert.equal(m.t, "error");
    assert.equal(m.kind, "bad_request");
  }
  // the next thing the host hears is the first good request: it saw none of the others
  const j = await joiner(code);
  j.send({ ...joinMsg(), sealed: "beef" });
  const req = await host.next();
  assert.equal(req.t, "join_request");
  assert.equal(req.sealed, "beef");
  // a refusal with an over-long kind still closes the newcomer
  host.send({ t: "admit", req: req.req, result: "reject", kind: "x".repeat(500), message: "no" });
  assert.equal((await j.next()).t, "error");
  assert.equal(await j.next(), null, "closed");
  host.close();
});

test("a host that stopped answering (connection never closed) loses its room", async () => {
  // (the test's hosts send no keepalive: after HOST_STALE_MS without one, the next check
  // takes the room away; a host that pings keeps it)
  const quiet = await openRoom();
  // a host that keeps answering, on a plain socket (the helper reads every message as JSON,
  // and the answer to "ping" is the word "pong")
  const ws = new WebSocket(`ws://${BASE}/ws/host`);
  const aliveCode = await new Promise((res, rej) => {
    ws.onerror = rej;
    ws.onopen = () => ws.send(JSON.stringify(hostOpen()));
    ws.onmessage = (e) => e.data !== "pong" && res(JSON.parse(e.data).code);
  });
  const ping = setInterval(() => ws.send("ping"), 1000);
  await sleep(9500);
  clearInterval(ping);
  const gone = await connect(`/ws/join/${quiet.code}`);
  assert.equal((await gone.next()).kind, "not_found", "the silent host's room is gone");
  const still = await connect(`/ws/join/${aliveCode}`);
  assert.equal((await still.next()).t, "host", "the answering host's room is still there");
  ws.send(JSON.stringify({ t: "close" }));
});

test("a connection the client closes is closed back (not left hanging)", async () => {
  const { host, code } = await openRoom();
  const ends = (path, first) =>
    new Promise((res, rej) => {
      const ws = new WebSocket(`ws://${BASE}${path}`);
      const t = setTimeout(() => rej(new Error("the server never answered the close")), 4000);
      ws.onerror = rej;
      ws.onopen = () => first && ws.send(JSON.stringify(first));
      ws.onmessage = () => ws.close(1000, "done");
      ws.onclose = (e) => (clearTimeout(t), res(e));
    });
  // a newcomer that leaves after the host's key, and a host that leaves after opening
  for (const e of [await ends(`/ws/join/${code}`), await ends("/ws/host", hostOpen())]) {
    assert.equal(e.wasClean, true);
    assert.equal(e.code, 1000);
  }
  host.send({ t: "close" });
});

test("a code nobody holds is refused, whatever is asked of it", async () => {
  // (these never read the room's storage: see the top of the Worker)
  const j = await connect("/ws/join/ZZZZZZ");
  assert.equal((await j.next()).kind, "not_found");
  const h = await connect("/ws/host?code=ZZZZZY");
  h.send({ t: "capacity", max_peers: 4, members: 2 });
  h.send({ t: "close" });
  h.send({ t: "nonsense" });
  assert.equal((await h.next()).kind, "bad_request");
  // and the code is still free to open
  const again = await connect("/ws/host?code=ZZZZZY");
  again.send(hostOpen());
  assert.equal((await again.next()).t, "opened");
  again.send({ t: "close" });
});

test("a host connection that floods is cut off; an ordinary one is not", async () => {
  const host = await connect("/ws/host");
  host.send(hostOpen({ max_peers: 8 }));
  const { code } = await host.next();
  // what a busy room sends: nothing happens
  for (let i = 0; i < 40; i++) host.send({ t: "capacity", max_peers: 8, members: 1 + (i % 7) });
  await sleep(300);
  assert.equal(host.closed, false);
  (await joiner(code)).close(); // (the room still answers)
  // far more than any room: refused, and the connection goes
  for (let i = 0; i < 200; i++) host.send({ t: "capacity", max_peers: 8, members: 1 + (i % 7) });
  let m;
  do m = await host.next(4000);
  while (m && m !== "timeout" && m.t !== "error");
  assert.equal(m?.kind, "rate_limited");
  await sleep(2500);
  assert.equal(host.closed, true);
  // (nobody holds the code now)
  const late = await connect(`/ws/join/${code}`);
  assert.equal((await late.next()).kind, "not_found");
  late.close();
});

test("invite page and health", async () => {
  const ok = await fetch(`http://${BASE}/j/k7m-q2x`);
  assert.equal(ok.status, 200);
  assert.match(await ok.text(), /gachi:\/\/join\/K7M-Q2X/);
  assert.equal((await fetch(`http://${BASE}/j/%3Cscript%3E`)).status, 404);
  assert.equal((await fetch(`http://${BASE}/j/%E0%A4%A`)).status, 404, "a broken escape is not a server error");
  assert.match(ok.headers.get("content-security-policy"), /frame-ancestors 'none'/);
  assert.match(await (await fetch(`http://${BASE}/j/k7m-q2x`)).text(), /<link rel="icon" href="\/favicon.svg"/, "the tab shows gachi's icon");
  assert.equal((await fetch(`http://${BASE}/favicon.svg`)).status, 200);
  assert.equal(ok.headers.get("referrer-policy"), "no-referrer");
  assert.equal(await (await fetch(`http://${BASE}/health`)).text(), "ok");
});

test("the invite page speaks the reader's language: Korean, English, Japanese, else English", async () => {
  const page = async (accept) => {
    const r = await fetch(`http://${BASE}/j/k7m-q2x`, accept ? { headers: { "Accept-Language": accept } } : {});
    return { text: await r.text(), lang: r.headers.get("content-language"), vary: r.headers.get("vary") };
  };
  const ko = await page("ko-KR,ko;q=0.9,en-US;q=0.8");
  assert.equal(ko.lang, "ko");
  assert.match(ko.text, /<html lang="ko">[\s\S]*합주에 초대받았어요[\s\S]*gachi:\/\/join\/K7M-Q2X/);
  assert.match(ko.vary, /Accept-Language/i);
  assert.match((await page("ja,en;q=0.5")).text, /セッションに招待されました/);
  assert.match((await page("en-GB,en;q=0.9")).text, /You're invited to a jam/);
  assert.equal((await page("fr-FR,fr;q=0.9")).lang, "en", "a language the page lacks: English");
  assert.equal((await page(null)).lang, "en");
  assert.equal((await page("fr;q=0.9,ja;q=0.8,ko;q=0.5")).lang, "ja", "the first we have, by preference");
  assert.equal((await page("ko;q=0,en")).lang, "en", "q=0 means not wanted");
  const bad = await fetch(`http://${BASE}/j/%3Cscript%3E`, { headers: { "Accept-Language": "ja" } });
  assert.match(await bad.text(), /招待リンクが正しくありません/);
});

test("web pages may not use the server; the app (no Origin) may", async () => {
  const { request } = await import("node:http");
  const status = (origin) => new Promise((resolve, reject) => {
    const headers = { Connection: "Upgrade", Upgrade: "websocket", "Sec-WebSocket-Version": "13", "Sec-WebSocket-Key": "dGhlIHNhbXBsZSBub25jZQ==" };
    if (origin) headers.Origin = origin;
    const [host, port] = BASE.split(":");
    const r = request({ host, port, path: "/ws/join/ZZZZZZ", headers });
    r.on("upgrade", (res, socket) => { socket.destroy(); resolve(res.statusCode); });
    r.on("response", (res) => { res.resume(); resolve(res.statusCode); });
    r.on("error", reject);
    r.end();
  });
  assert.equal(await status("https://evil.example"), 403);
  assert.equal(await status("http://localhost:5180"), 403);
  assert.equal(await status(null), 101, "the app's request goes through");
  assert.equal(await status("https://gachi.band"), 101);
});

test("too many attempts from one address are refused", async () => {
  let limited = false;
  for (let i = 0; i < LIMIT + 5 && !limited; i++) {
    const c = await connect(`/ws/join/ZZZZZZ`);
    const first = await Promise.race([c.next(500), sleep(50).then(() => null)]);
    if (first?.kind === "rate_limited") limited = true;
    else c.close();
  }
  assert.ok(limited, "rate limit kicks in");
});

// last: it uses up this address's nearby lookups for the minute
test("nearby lookups are limited per address (one Durable Object call each)", async () => {
  let refused = null;
  for (let i = 0; i < 12 && !refused; i++) {
    const msg = await (await connect("/ws/nearby?lan=192.168.0")).next();
    if (msg.t === "error") refused = msg;
  }
  assert.ok(refused, "refused within a minute's allowance");
  assert.equal(refused.kind, "rate_limited");
});
