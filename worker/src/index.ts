/**
 * gachi connection server (Cloudflare Worker + Durable Objects).
 *
 * It only introduces people. Audio, chat and member lists never come here, and join
 * requests and answers pass through sealed between newcomer and host (it cannot read them,
 * and never sees a room password).
 *
 *   /ws/host              the host opens a new code, or reopens / takes over one (?code=…)
 *   /ws/join/{code}       a newcomer asks to join; the host decides; the newcomer leaves
 *   /ws/nearby?lan=…      open rooms on the caller's network (same public address + subnet)
 *   /j/{code}             invite page (opens the app with gachi://join/{code})
 *   /health               "ok"
 *
 * Nothing kept here says who connected. An address is used only while its request is being
 * handled: attempts are counted in memory, and "the same network" is kept as a short tag
 * that about a thousand addresses share (see `whereOf`). The objects that hold these are a
 * fixed set of shards, not one per address, so they do not pile up either.
 *
 * One `Jam` Durable Object per code (a room) holds the host's WebSocket (hibernating, so an idle
 * room costs nothing), the code's `host_key`, and the room size. When the host turns the
 * code off, the room is forgotten. If the host's connection drops, the code is kept for
 * 30 s so the host (or the next host, who has the key) can take it back. A host that
 * vanished without its connection closing is noticed by its missing keepalives, within the
 * hour, and its room is forgotten too: nothing stays here that nobody holds.
 *
 * "Forgotten" means the object's storage is empty again, alarm included (`forget`): an
 * object with anything left in it, even an empty table or a pending alarm, stays on the
 * account's list of objects. For the same reason the storage is first touched when a host
 * opens the room (`host_open`), and by nothing before that: not by a newcomer asking for a
 * code nobody holds, not by a host connection that never opens, not by picking a new code.
 *
 * Message shapes: see `crates/proto/src/signal.rs` (`ToWorker` / `FromWorker`).
 */
import { DurableObject } from "cloudflare:workers";

export interface Env {
  ROOMS: DurableObjectNamespace<Jam>;
  LIMITER: DurableObjectNamespace<Counts>;
  LOBBY: DurableObjectNamespace<Local>;
  /** optional override of the per-IP attempt limit (tests) */
  ATTEMPTS_PER_MIN?: string;
  /** optional overrides of `HOST_CHECK_MS` / `HOST_STALE_MS` (tests) */
  HOST_CHECK_MS?: string;
  HOST_STALE_MS?: string;
}

const ALPHABET = "ABCDEFGHJKMNPQRSTUVWXYZ23456789";
const CODE_LEN = 6;
const GRACE_MS = 30_000;
/** how often a room looks whether its host is still there */
const HOST_CHECK_MS = 3600_000;
/** a host that has not sent its keepalive ("ping", every 20 s) for this long is gone: its
 *  PC lost power or its network, and the connection was never closed */
const HOST_STALE_MS = 5 * 60_000;
const MAX_ROOM = 16;
/** join/open attempts per IP per minute (codes are short: no guessing sprees) */
const ATTEMPTS_PER_MIN = 20;

/** "k7m-q2x" → "K7MQ2X", or null if it is not a valid code. */
export function normalizeCode(raw: string | null): string | null {
  if (!raw) return null;
  const c = raw.toUpperCase().replace(/[^A-Z0-9]/g, "");
  if (c.length !== CODE_LEN) return null;
  for (const ch of c) if (!ALPHABET.includes(ch)) return null;
  return c;
}

function randomCode(): string {
  // every letter equally likely: bytes past the last whole multiple of the alphabet are
  // drawn again (256 % 31 = 8: the first 8 letters came up 9 times in 256, the rest 8)
  const fair = 256 - (256 % ALPHABET.length);
  let out = "";
  while (out.length < CODE_LEN) {
    for (const b of crypto.getRandomValues(new Uint8Array(CODE_LEN * 2))) {
      if (b < fair && out.length < CODE_LEN) out += ALPHABET[b % ALPHABET.length];
    }
  }
  return out;
}

/** `decodeURIComponent` that gives null for a broken escape (it throws: a 500 otherwise). */
function decodePath(raw: string): string | null {
  try {
    return decodeURIComponent(raw);
  } catch {
    return null;
  }
}

/** A close reason must fit 123 bytes, or `close` throws and the socket stays open. */
function reason(kind: string): string {
  return kind.replace(/[^a-z_]/g, "").slice(0, 40) || "error";
}

function randomKey(): string {
  return Array.from(crypto.getRandomValues(new Uint8Array(16)), (b) => b.toString(16).padStart(2, "0")).join("");
}

function displayCode(c: string): string {
  return `${c.slice(0, 3)}-${c.slice(3)}`;
}

/** How long a connection that has been told all it will hear may take to close itself. */
const LINGER_MS = 2000;

/**
 * A WebSocket that only says one thing. The other side closes it (the app does, as soon
 * as it has the answer) and that close is answered; only a client that does not is closed
 * from here, after `LINGER_MS`. Closing first from here ends the connection in a way that
 * is counted as an error (`responseStreamDisconnected`), on every refusal and every lookup.
 */
function sayOnce(msg: unknown, code: number, why: string): Response {
  const [client, server] = Object.values(new WebSocketPair());
  server.accept();
  server.send(JSON.stringify(msg));
  let over = false;
  const end = (c: number, w: string) => {
    if (over) return;
    over = true;
    try {
      server.close(c, w);
    } catch {
      /* gone already */
    }
  };
  server.addEventListener("close", () => end(1000, "bye"));
  server.addEventListener("error", () => end(1000, "bye"));
  setTimeout(() => end(code, why), LINGER_MS);
  return new Response(null, { status: 101, webSocket: client });
}

/** A WebSocket that only says one error (for refusals before reaching a room). */
function refuse(kind: string, message: string): Response {
  return sayOnce({ t: "error", kind, message }, 1008, reason(kind));
}

export default {
  async fetch(req: Request, env: Env): Promise<Response> {
    const url = new URL(req.url);
    if (url.pathname === "/health") return new Response("ok");

    const invite = url.pathname.match(/^\/j\/([^/]+)\/?$/);
    if (invite) return invitePage(invite[1]);

    const isHost = url.pathname === "/ws/host";
    const join = url.pathname.match(/^\/ws\/join\/([^/]+)$/);
    const nearby = url.pathname === "/ws/nearby";
    if (!isHost && !join && !nearby) return new Response("not found", { status: 404 });
    if (req.headers.get("Upgrade") !== "websocket") return new Response("expected a WebSocket", { status: 426 });

    // the caller's address, as far as it goes: a shard, a tag, a key (see `whereOf`)
    const at = await whereOf(req.headers.get("CF-Connecting-IP") ?? "local");
    if (nearby) {
      // one Durable Object call per lookup (the lobby counts the attempts itself): the free
      // plan allows 100,000 calls a day in all
      const lan = cleanLan(url.searchParams.get("lan"));
      const gw = cleanGw(url.searchParams.get("gw"));
      const rooms = await env.LOBBY.get(env.LOBBY.idFromName(`nearby-${at.shard}`)).list(at.tag, gw, lan, at.key);
      if (rooms === null) return refuse("rate_limited", "잠시 후 다시 해 주세요.");
      return sayOnce({ t: "nearby", rooms }, 1000, "done");
    }

    // brute-force guard: codes are short, so limit attempts per IP
    const limiter = env.LIMITER.get(env.LIMITER.idFromName(`attempts-${at.shard}`));
    if (!(await limiter.hit(at.key, Number(env.ATTEMPTS_PER_MIN) || ATTEMPTS_PER_MIN))) {
      return refuse("rate_limited", "시도가 너무 많아요. 잠시 후 다시 해 주세요.");
    }

    if (join) {
      const code = normalizeCode(decodePath(join[1]));
      if (!code) return refuse("not_found", "방을 찾을 수 없어요. 코드를 다시 확인해 주세요.");
      return env.ROOMS.get(env.ROOMS.idFromName(code)).fetch(withCode(req, code, "join", at.net));
    }

    // host: reopen/take over a given code, or find a free one
    const wanted = url.searchParams.get("code");
    if (wanted) {
      const code = normalizeCode(wanted);
      if (!code) return refuse("bad_request", "코드 형식이 맞지 않아요.");
      return env.ROOMS.get(env.ROOMS.idFromName(code)).fetch(withCode(req, code, "host", at.net));
    }
    // a new code: picked at random, without asking whether a room has it. Asking would
    // read that object's storage, and leave it behind if the host then never opens (among
    // 887 million codes a clash takes luck; `host_open` refuses it and the app asks again)
    const code = randomCode();
    return env.ROOMS.get(env.ROOMS.idFromName(code)).fetch(withCode(req, code, "host", at.net));
  },
};

function withCode(req: Request, code: string, role: "host" | "join", net: string): Request {
  const r = new Request(req);
  r.headers.set("x-gachi-code", code);
  r.headers.set("x-gachi-role", role);
  r.headers.set("x-gachi-net", net);
  // (the room never learns the address itself)
  r.headers.delete("CF-Connecting-IP");
  return r;
}

/** The objects that count attempts and list nearby rooms: this many of each, whoever comes. */
const SHARDS = 64;

/**
 * What is kept of where a connection comes from. Its network (see `networkOf`) is hashed
 * and cut down to 22 bits: 6 pick one of the `SHARDS` objects, 16 are the tag stored with a
 * listed room. About a thousand IPv4 addresses share each pair, so what is stored does not
 * say which one it was (a longer tag would; a shorter one would show strangers each other's
 * rooms: that is what the router value is for, see `cleanGw`).
 *
 * `key` tells one caller from another while attempts are counted: an IPv4 address, or an
 * IPv6 /64 (one home has all of its /64 to pick addresses from: counted per address, it
 * would never run out of attempts). It is only ever held in memory, for a minute.
 */
export async function whereOf(ip: string): Promise<{ shard: number; tag: string; net: string; key: string }> {
  const hex = async (text: string) =>
    Array.from(new Uint8Array(await crypto.subtle.digest("SHA-256", new TextEncoder().encode(text))), (b) => b.toString(16).padStart(2, "0")).join("");
  const n = await hex("gachi network v1 " + networkOf(ip));
  const bits = parseInt(n.slice(0, 6), 16); // 24 bits, of which 22 are used
  const shard = bits >> 18;
  const tag = ((bits >> 2) & 0xffff).toString(16).padStart(4, "0");
  return { shard, tag, net: `${shard}.${tag}`, key: (await hex("gachi attempt v1 " + networkOf(ip))).slice(0, 16) };
}

/** "17.a3f0" (see `whereOf`) back into its parts; null for anything else (an older room's). */
function parseNet(net: string | undefined): { shard: number; tag: string } | null {
  const m = /^(\d{1,2})\.([0-9a-f]{4})$/.exec(net ?? "");
  return m && Number(m[1]) < SHARDS ? { shard: Number(m[1]), tag: m[2] } : null;
}

/** nearby-room lookups per IP per minute (the home screen asks every ~30 s) */
const NEARBY_PER_MIN = 10;
/** a listed room is dropped after this long without the host refreshing it */
const LOBBY_TTL_MS = 12 * 3600_000;

/**
 * The "network" a connection comes from: its public IPv4 address, or the /64 of an IPv6
 * address (one home gets one /64).
 */
export function networkOf(ip: string): string {
  if (!ip.includes(":")) return ip;
  const [head, tail = ""] = ip.toLowerCase().split("::");
  const h = head ? head.split(":") : [];
  const t = tail ? tail.split(":") : [];
  const groups = ip.includes("::") ? [...h, ...Array(Math.max(0, 8 - h.length - t.length)).fill("0"), ...t] : h;
  return groups.slice(0, 4).map((g) => g.replace(/^0+(?=.)/, "")).join(":") + "::/64";
}

/** "192.168.0" — only private IPv4 /24 prefixes are accepted. */
export function cleanLan(raw: unknown): string | null {
  if (typeof raw !== "string") return null;
  const m = raw.match(/^(\d{1,3})\.(\d{1,3})\.(\d{1,3})$/);
  if (!m) return null;
  const [a, b, c] = m.slice(1).map(Number);
  if (a > 255 || b > 255 || c > 255) return null;
  const priv = a === 10 || (a === 172 && b >= 16 && b <= 31) || (a === 192 && b === 168);
  return priv ? `${a}.${b}.${c}` : null;
}

/** What devices behind one router have in common (the app's `router_key`): 32 hex digits. */
export function cleanGw(raw: unknown): string | null {
  return typeof raw === "string" && /^[0-9a-f]{32}$/.test(raw) ? raw : null;
}

interface LobbyEntry {
  name: string;
  host: string;
  lan: string | null;
  /** see cleanGw: only callers who bring the same value are shown the room */
  gw: string | null;
  locked: boolean;
}

interface Stored {
  code: string;
  hostKey: string;
  maxPeers: number;
  members: number;
  /** the host's public key newcomers seal their requests to (hex) */
  joinKey: string;
  /** the room has a password */
  locked: boolean;
  /** where the room is listed for nearby people (the host's `whereOf` net), if at all */
  lobby?: { net: string; entry: LobbyEntry };
}

interface Attachment {
  role: "host" | "join";
  /** the code this connection came in on */
  code: string;
  /** where the connection comes from, as kept (see whereOf) */
  net?: string;
  /** for newcomers: the id of their pending request */
  req?: number;
  /** for hosts: this socket has sent host_open successfully */
  open?: boolean;
  /** when it connected (ms) */
  at?: number;
  /** it has been told all it will hear (an error, or the welcome) and is on its way out */
  done?: number;
  /** for hosts: messages in the minute that began at `since` (ms) */
  n?: number;
  since?: number;
}

/** what a newcomer is told when no host holds the code: it was mistyped, the room is over,
 *  or the host's connection dropped a moment ago (which of these is not looked up) */
const NO_HOST: [string, string] = ["not_found", "방을 찾을 수 없어요. 코드를 다시 확인해 주세요. 방장 연결이 잠시 끊긴 것이라면 조금 뒤에 다시 시도해 주세요."];

/** messages a host connection may send per minute. A room sends a few when someone comes or
 *  goes; each one wakes the object, and most write the room down again: without a limit one
 *  connection could use up the day's writes for everybody */
const HOST_MSGS_PER_MIN = 120;
/** newcomers waiting for the host's answer, per room */
const MAX_WAITING = 16;
/** a newcomer waits this long at most */
const JOIN_WAIT_MS = 60_000;
const HEX = /^[0-9a-f]*$/;

export class Jam extends DurableObject<Env> {
  constructor(ctx: DurableObjectState, env: Env) {
    super(ctx, env);
    // keepalive pings are answered without waking the object (no cost while idle)
    ctx.setWebSocketAutoResponse(new WebSocketRequestResponsePair("ping", "pong"));
  }

  async fetch(req: Request): Promise<Response> {
    const code = req.headers.get("x-gachi-code") ?? "";
    const role = req.headers.get("x-gachi-role") === "host" ? "host" : "join";
    const net = req.headers.get("x-gachi-net") ?? "local";
    // a code no host is connected to: refused here, without touching the object's storage
    // (most of these are mistyped or old codes, and each would leave an object behind)
    if (role === "join" && this.hosts().length === 0) return refuse(NO_HOST[0], NO_HOST[1]);
    const [client, server] = Object.values(new WebSocketPair());
    if (role === "join") {
      // newcomers wait here until the host answers. One that never asks, or is never
      // answered, would wait for ever: the stale ones go, and past the limit nobody new
      // gets in line (each one costs the host a message and a decryption)
      const now = Date.now();
      let waiting = 0;
      for (const j of this.ctx.getWebSockets("join")) {
        const a = j.deserializeAttachment() as Attachment;
        if (a?.done) {
          // (one that was answered and should be gone: if it is still here after a
          // minute, its closing was lost with the object's memory)
          if (now - a.done > JOIN_WAIT_MS) {
            try {
              j.close(1000, "bye");
            } catch {
              /* closed meanwhile */
            }
          }
        } else if (now - (a?.at ?? 0) > JOIN_WAIT_MS) this.fail(j, "timeout", "방장이 응답하지 않았어요. 다시 시도해 주세요.");
        else waiting++;
      }
      if (waiting >= MAX_WAITING) return refuse("rate_limited", "지금 들어오려는 사람이 너무 많아요. 잠시 후 다시 해 주세요.");
    }
    this.ctx.acceptWebSocket(server, [role]);
    server.serializeAttachment({ role, code, net, at: Date.now() } satisfies Attachment);
    // a newcomer first needs the host's join key to seal its request
    if (role === "join") {
      const room = await this.room();
      const refusal = this.joinRefusal(room);
      if (refusal) this.fail(server, refusal[0], refusal[1]);
      else this.send(server, { t: "host", join_key: room!.joinKey, locked: room!.locked });
    }
    return new Response(null, { status: 101, webSocket: client });
  }

  /** Why a newcomer cannot ask to join right now, if it cannot. */
  private joinRefusal(room: Stored | undefined): [string, string] | null {
    if (!room || this.hosts().length === 0) return NO_HOST;
    if (room.members >= room.maxPeers) return ["full", "방이 가득 찼어요."];
    return null;
  }

  private hosts(): WebSocket[] {
    return this.ctx.getWebSockets("host").filter((ws) => ws.readyState === 1 && (ws.deserializeAttachment() as Attachment)?.open);
  }

  /** The room, if a host is connected (only then is the storage read: see the top). */
  private async room(): Promise<Stored | undefined> {
    return this.hosts().length === 0 ? undefined : this.ctx.storage.get<Stored>("room");
  }

  /** Forgets the room: off the nearby list, and nothing left in storage (the alarm first:
   *  emptying the storage does not take it along). */
  private async forget(room: Stored | undefined) {
    if (room) await this.unlist(room);
    await this.ctx.storage.deleteAlarm();
    await this.ctx.storage.deleteAll();
  }

  private send(ws: WebSocket, msg: unknown) {
    try {
      ws.send(JSON.stringify(msg));
    } catch {
      /* closed meanwhile */
    }
  }

  private fail(ws: WebSocket, kind: string, message: string) {
    this.send(ws, { t: "error", kind, message });
    this.finish(ws, 1008, reason(kind));
  }

  /** This connection has heard all it will: it is left to close itself (see `sayOnce`),
   *  and closed from here if it has not after `LINGER_MS`. Nothing it sends meanwhile counts. */
  private finish(ws: WebSocket, code: number, why: string) {
    try {
      const att = ws.deserializeAttachment() as Attachment;
      ws.serializeAttachment({ ...att, open: false, done: Date.now() } satisfies Attachment);
    } catch {
      /* closed meanwhile */
    }
    setTimeout(() => {
      try {
        ws.close(code, why);
      } catch {
        /* it closed itself: the usual case */
      }
    }, LINGER_MS);
  }

  async webSocketMessage(ws: WebSocket, raw: string | ArrayBuffer) {
    if (typeof raw !== "string" || raw.length > 32_768) return this.fail(ws, "bad_request", "잘못된 요청이에요.");
    let msg: any;
    try {
      msg = JSON.parse(raw);
    } catch {
      return this.fail(ws, "bad_request", "잘못된 요청이에요.");
    }
    const att = ws.deserializeAttachment() as Attachment;
    if (att.done) return;
    if (att.role === "host") {
      const now = Date.now();
      const fresh = !att.since || now - att.since > 60_000;
      att.since = fresh ? now : att.since;
      att.n = fresh ? 1 : (att.n ?? 0) + 1;
      ws.serializeAttachment(att);
      if (att.n > HOST_MSGS_PER_MIN) {
        // (its room goes the way of any host whose connection ended: kept 30 s, then forgotten)
        const held = att.open === true;
        this.fail(ws, "rate_limited", "요청이 너무 많아요. 잠시 후 다시 해 주세요.");
        if (held) await this.hostLeft(ws);
        return;
      }
      return this.onHost(ws, att, msg);
    }
    return this.onJoin(ws, att, msg);
  }

  private async onHost(ws: WebSocket, att: Attachment, msg: any) {
    if (msg?.t !== "host_open" && !att.open) {
      // (nothing to do for a host that has not opened; and nothing read for it)
      if (["capacity", "lobby", "admit", "close"].includes(msg?.t)) return;
      return this.fail(ws, "bad_request", "잘못된 요청이에요.");
    }
    const room = await this.ctx.storage.get<Stored>("room");
    switch (msg.t) {
      case "host_open": {
        const code = room?.code ?? att.code;
        if (room && msg.host_key !== room.hostKey) {
          return this.fail(ws, "code_taken", "이 코드는 다른 방이 쓰고 있어요. 새 코드를 만들어 주세요.");
        }
        // (a host reopening a forgotten code brings the key we gave it; anything else gets a new one)
        const hostKey = room?.hostKey ?? (typeof msg.host_key === "string" && /^[0-9a-f]{32}$/.test(msg.host_key) ? msg.host_key : randomKey());
        if (typeof msg.join_key !== "string" || !/^[0-9a-f]{64}$/.test(msg.join_key)) {
          return this.fail(ws, "bad_request", "앱을 최신 버전으로 업데이트해 주세요.");
        }
        const stored: Stored = {
          code,
          hostKey,
          maxPeers: clampSize(msg.max_peers),
          members: clampMembers(msg.members),
          joinKey: msg.join_key,
          locked: msg.locked === true,
          // a takeover from another network re-lists it with `lobby`
          lobby: room?.lobby?.net === att.net ? room?.lobby : undefined,
        };
        if (room?.lobby && !stored.lobby) await this.unlist(room);
        await this.ctx.storage.put("room", stored);
        await this.list(stored);
        // (this also ends a grace period that was running)
        await this.ctx.storage.setAlarm(Date.now() + (Number(this.env.HOST_CHECK_MS) || HOST_CHECK_MS));
        // one host per code: a takeover replaces the old connection
        for (const other of this.hosts()) if (other !== ws) this.fail(other, "code_taken", "다른 기기에서 방을 이어받았어요.");
        ws.serializeAttachment({ ...att, open: true } satisfies Attachment);
        this.send(ws, { t: "opened", code, host_key: hostKey });
        return;
      }
      case "capacity": {
        if (!att.open || !room) return;
        const [size, members] = [clampSize(msg.max_peers), clampMembers(msg.members)];
        // (told again what is already written down: nothing to write)
        if (size === room.maxPeers && members === room.members) return;
        room.maxPeers = size;
        room.members = members;
        await this.ctx.storage.put("room", room);
        await this.list(room);
        return;
      }
      case "lobby": {
        if (!att.open || !room) return;
        const entry = cleanEntry(msg.lobby);
        // (the same listing again, from the same network: nothing to write)
        const was = room.lobby;
        if ((!was && !entry) || (was && entry && was.net === (att.net ?? "local") && JSON.stringify(was.entry) === JSON.stringify(entry))) return;
        if (room.lobby && (!entry || room.lobby.net !== att.net)) await this.unlist(room);
        room.lobby = entry ? { net: att.net ?? "local", entry } : undefined;
        await this.ctx.storage.put("room", room);
        await this.list(room);
        return;
      }
      case "admit": {
        if (!att.open) return;
        const joiner = this.ctx
          .getWebSockets("join")
          .find((j) => (j.deserializeAttachment() as Attachment)?.req === msg.req);
        if (!joiner) return; // gave up meanwhile
        if (msg.result === "accept") {
          this.send(joiner, { t: "welcome", sealed: String(msg.welcome?.sealed ?? "") });
          this.finish(joiner, 1000, "welcome");
        } else {
          this.fail(joiner, reason(String(msg.kind ?? "bad_request")), String(msg.message ?? "들어갈 수 없어요.").slice(0, 200));
        }
        return;
      }
      case "close": {
        if (!att.open) return;
        await this.forget(room);
        this.finish(ws, 1000, "closed");
        return;
      }
      default:
        return this.fail(ws, "bad_request", "잘못된 요청이에요.");
    }
  }

  private async onJoin(ws: WebSocket, att: Attachment, msg: any) {
    if (msg.t !== "join" || att.req !== undefined) return this.fail(ws, "bad_request", "잘못된 요청이에요.");
    const room = await this.room();
    const refusal = this.joinRefusal(room);
    if (refusal) return this.fail(ws, refusal[0], refusal[1]);
    const host = this.hosts()[0];
    // a one-time public key and a sealed request, in hex, of sensible size: nothing else is passed on
    const shaped = typeof msg.eph === "string" && /^[0-9a-f]{64}$/.test(msg.eph) && typeof msg.sealed === "string" && msg.sealed.length <= 8192 && HEX.test(msg.sealed);
    if (!shaped) return this.fail(ws, "bad_request", "잘못된 요청이에요.");
    const req = crypto.getRandomValues(new Uint32Array(1))[0];
    ws.serializeAttachment({ ...att, req } satisfies Attachment);
    // pass on only the sealed request; the host opens it and answers with `admit`
    this.send(host, { t: "join_request", req, eph: msg.eph, pw: msg.pw === true, sealed: msg.sealed });
  }

  async webSocketClose(ws: WebSocket) {
    const att = ws.deserializeAttachment() as Attachment;
    const wasHost = att?.role === "host" && att.open === true;
    // answer the close: without it the connection never finishes closing, the other side
    // gives up on it, and it is counted as an error
    try {
      if (att) ws.serializeAttachment({ ...att, open: false } satisfies Attachment);
      ws.close(1000, "bye");
    } catch {
      /* gone already (a connection that was cut) */
    }
    // the host vanished without turning the code off: keep the code for a while (the next
    // host takes it over), but off the nearby list until then: nobody could get in meanwhile.
    // A takeover lists it again (host_open keeps the room's lobby entry).
    if (wasHost) await this.hostLeft(ws);
    // (a host connection that never opened a room has not touched the storage, and its
    // closing does not either)
  }

  /** The host's connection `ws` is over, and no other holds the code: see `webSocketClose`. */
  private async hostLeft(ws: WebSocket) {
    if (this.hosts().filter((h) => h !== ws).length > 0) return;
    await this.ctx.storage.setAlarm(Date.now() + GRACE_MS);
    const room = await this.ctx.storage.get<Stored>("room");
    if (room) await this.unlist(room);
  }

  async webSocketError(ws: WebSocket) {
    await this.webSocketClose(ws);
  }

  async alarm() {
    // a host whose connection never closed but who stopped answering (power cut, network
    // gone): without this its room, and its code, would stay here for good
    const now = Date.now();
    const stale = Number(this.env.HOST_STALE_MS) || HOST_STALE_MS;
    for (const ws of this.hosts()) {
      const att = ws.deserializeAttachment() as Attachment;
      const seen = this.ctx.getWebSocketAutoResponseTimestamp(ws)?.getTime() ?? att.at ?? 0;
      if (now - seen > stale) {
        ws.serializeAttachment({ ...att, open: false } satisfies Attachment);
        try {
          ws.close(1001, "stale");
        } catch {
          /* already closed */
        }
      }
    }
    if (this.hosts().length > 0) {
      await this.ctx.storage.setAlarm(now + (Number(this.env.HOST_CHECK_MS) || HOST_CHECK_MS));
      return;
    }
    // nobody holds the code (or took it back in time): forget the room
    await this.forget(await this.ctx.storage.get<Stored>("room"));
  }

  /** Puts (or refreshes) the room on its network's nearby list. */
  private async list(room: Stored) {
    const at = parseNet(room.lobby?.net);
    if (!room.lobby || !at) return;
    await this.env.LOBBY.get(this.env.LOBBY.idFromName(`nearby-${at.shard}`)).put({
      code: room.code,
      tag: at.tag,
      ...room.lobby.entry,
      members: room.members,
      max_peers: room.maxPeers,
    });
  }

  private async unlist(room: Stored) {
    const at = parseNet(room.lobby?.net);
    if (at) await this.env.LOBBY.get(this.env.LOBBY.idFromName(`nearby-${at.shard}`)).drop(room.code);
  }
}

function cleanEntry(raw: any): LobbyEntry | null {
  if (!raw || typeof raw !== "object") return null;
  const text = (v: unknown, n: number) => (typeof v === "string" ? [...v.trim()].slice(0, n).join("") : "");
  // (a room that cannot say which router it is behind is not listed: its tag alone is
  // shared by about a thousand addresses)
  const gw = cleanGw(raw.gw);
  if (!gw) return null;
  return { name: text(raw.name, 40), host: text(raw.host, 12), lan: cleanLan(raw.lan), gw, locked: !!raw.locked };
}

interface Listed extends LobbyEntry {
  code: string;
  /** the listing network's tag (see whereOf) */
  tag: string;
  members: number;
  max_peers: number;
  at?: number;
}

/** how often a shard looks for listings nobody refreshed */
const SWEEP_MS = 3600_000;

/**
 * Open rooms for the "nearby rooms" list: one of `SHARDS` objects, each holding the rooms
 * of the networks that fall to it, told apart by their tag. A room is here from when its
 * host lists it until the code goes off or the host is gone; what is left behind anyway is
 * swept hourly, and a shard with nothing in it keeps nothing.
 */
export class Local extends DurableObject<Env> {
  /** lookups per caller this minute (in memory: losing it on eviction is fine) */
  private hits = new Map<string, { start: number; count: number }>();

  async put(room: Listed) {
    await this.ctx.storage.put(room.code, { ...room, at: Date.now() });
    if ((await this.ctx.storage.getAlarm()) === null) await this.ctx.storage.setAlarm(Date.now() + SWEEP_MS);
  }

  async drop(code: string) {
    // (only a shard that lists something is touched at all)
    if ((await this.ctx.storage.getAlarm()) === null) return;
    await this.ctx.storage.delete(code);
    if ((await this.ctx.storage.list({ limit: 1 })).size === 0) await this.empty();
  }

  /** Nothing listed: nothing kept (the alarm first: emptying the storage leaves it). */
  private async empty() {
    await this.ctx.storage.deleteAlarm();
    await this.ctx.storage.deleteAll();
  }

  async alarm() {
    const now = Date.now();
    const all = await this.ctx.storage.list<Listed>();
    let left = 0;
    for (const [code, r] of all) {
      if (now - (r.at ?? 0) > LOBBY_TTL_MS) await this.ctx.storage.delete(code);
      else left++;
    }
    if (left > 0) await this.ctx.storage.setAlarm(now + SWEEP_MS);
    else await this.empty();
  }

  /** Rooms listed from behind the caller's router (same tag, same router value, and the same
   *  subnet when both sides know theirs), newest first; `null` when `key` asked too often
   *  this minute. */
  async list(tag: string, gw: string | null, lan: string | null, key = "local") {
    const now = Date.now();
    const h = this.hits.get(key);
    if (!h || now - h.start > 60_000) {
      this.hits.set(key, { start: now, count: 1 });
      // forget callers that went quiet
      if (this.hits.size > 256) for (const [k, v] of this.hits) if (now - v.start > 60_000) this.hits.delete(k);
    } else if (++h.count > NEARBY_PER_MIN) {
      return null;
    }
    // (a shard with nothing listed has no alarm: answered without reading its storage)
    if ((await this.ctx.storage.getAlarm()) === null) return [];
    const all = await this.ctx.storage.list<Listed>();
    const out: Listed[] = [];
    for (const [code, r] of all) {
      if (now - (r.at ?? 0) > LOBBY_TTL_MS) {
        await this.ctx.storage.delete(code);
        continue;
      }
      // the tag narrows it to about a thousand addresses; the router value makes it exact
      if (r.tag !== tag || !gw || r.gw !== gw) continue;
      if (lan && r.lan && lan !== r.lan) continue;
      out.push(r);
    }
    out.sort((a, b) => (b.at ?? 0) - (a.at ?? 0));
    return out.slice(0, 10).map(({ code, name, host, locked, members, max_peers }) => ({ code, name, host, locked, members, max_peers }));
  }
}

function clampMembers(n: unknown): number {
  const v = Math.round(Number(n) || 1);
  return Math.min(MAX_ROOM, Math.max(1, v));
}

function clampSize(n: unknown): number {
  const v = Math.round(Number(n) || 4);
  return Math.min(MAX_ROOM, Math.max(2, v));
}

/**
 * Counts attempts per caller per minute: one of `SHARDS` objects, in memory only (nothing
 * is ever written, and losing the counts on eviction is fine).
 */
export class Counts extends DurableObject<Env> {
  private seen = new Map<string, { start: number; count: number }>();

  async hit(key: string, limit: number): Promise<boolean> {
    const now = Date.now();
    const h = this.seen.get(key);
    if (!h || now - h.start > 60_000) {
      this.seen.set(key, { start: now, count: 1 });
      if (this.seen.size > 1024) for (const [k, v] of this.seen) if (now - v.start > 60_000) this.seen.delete(k);
      return true;
    }
    return ++h.count <= limit;
  }
}

function invitePage(raw: string): Response {
  const code = normalizeCode(decodePath(raw));
  const headers = {
    "content-type": "text/html; charset=utf-8",
    // the page runs its own few lines and loads nothing; no other site may frame it (it
    // opens the app) or learn the code from a referrer
    "content-security-policy": "default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'",
    "x-frame-options": "DENY",
    "x-content-type-options": "nosniff",
    "referrer-policy": "no-referrer",
    "cache-control": "no-store",
  };
  if (!code) {
    return new Response(PAGE_NOT_FOUND, { status: 404, headers });
  }
  // only code-alphabet characters reach the page
  return new Response(PAGE.replaceAll("{CODE}", displayCode(code)), { headers });
}

const PAGE_NOT_FOUND = `<!doctype html><html lang="ko"><meta charset="utf-8"><title>gachi</title>
<body style="font-family:system-ui,sans-serif;background:#0c0d10;color:#e8e8ee;display:grid;place-items:center;height:100vh;margin:0">
<p>초대 링크가 올바르지 않아요.</p></body></html>`;

const PAGE = `<!doctype html>
<html lang="ko">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>gachi 합주 초대</title>
<style>
  :root { color-scheme: dark; }
  body { margin: 0; min-height: 100vh; display: grid; place-items: center; background: #0c0d10; color: #e8e8ee;
         font-family: system-ui, -apple-system, "Segoe UI", "Malgun Gothic", sans-serif; }
  main { text-align: center; padding: 32px 20px; max-width: 360px; }
  h1 { font-size: 18px; font-weight: 600; margin: 0 0 18px; }
  .code { font: 700 40px ui-monospace, "Cascadia Mono", Consolas, monospace; letter-spacing: .1em; margin: 6px 0 22px; }
  a.btn { display: inline-block; padding: 12px 22px; border-radius: 10px; background: #8b7cff; color: #0c0d10;
          font-weight: 700; text-decoration: none; }
  p { color: #9a9aa8; font-size: 14px; line-height: 1.6; margin-top: 22px; }
</style>
</head>
<body>
<main>
  <h1>합주에 초대받았어요</h1>
  <div class="code">{CODE}</div>
  <a class="btn" id="open" href="gachi://join/{CODE}">gachi 앱에서 열기</a>
  <p>앱이 열리지 않으면 gachi를 설치한 뒤<br>‘참여’에서 위 코드를 입력하세요.</p>
</main>
<script>setTimeout(function () { location.href = document.getElementById('open').href; }, 300);</script>
</body>
</html>
`;
