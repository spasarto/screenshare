interface Env {
  SESSION_DO: DurableObjectNamespace;
  RATE_LIMITER: DurableObjectNamespace;
  TURN_TOKEN_ID: string;
  TURN_API_TOKEN: string;
}

interface SessionData {
  offer: string | null;
  answer: string | null;
}

const SESSION_TTL = 300; // 5 minutes
const CORS_HEADERS = {
  "Access-Control-Allow-Origin": "*",
  "Access-Control-Allow-Methods": "GET, POST, PUT, DELETE, OPTIONS",
  "Access-Control-Allow-Headers": "Content-Type",
};

// 6 digits + 4 lowercase letters: ~4.6 * 10^11 combinations. Large enough
// that sweeping the whole space through the rate limiter below is not
// feasible (see RateLimiter).
const CODE_PATTERN = /^[0-9]{6}[a-z]{4}$/;
const CODE_DIGITS = 6;
const CODE_LETTERS = 4;
const LETTER_ALPHABET = "abcdefghijklmnopqrstuvwxyz";

function json(data: unknown, status = 200): Response {
  return new Response(JSON.stringify(data), {
    status,
    headers: { "Content-Type": "application/json", ...CORS_HEADERS },
  });
}

function err(message: string, status: number): Response {
  return json({ error: message }, status);
}

/** Uniform random integer in [0, max) using rejection sampling over crypto.getRandomValues. */
function randomInt(max: number): number {
  const range = Math.floor(0x100000000 / max) * max;
  const buf = new Uint32Array(1);
  let x: number;
  do {
    crypto.getRandomValues(buf);
    x = buf[0];
  } while (x >= range);
  return x % max;
}

/** Generate a random 6-digit + 4-letter session code, e.g. "482917bcdq". */
function generateCode(): string {
  let digits = "";
  for (let i = 0; i < CODE_DIGITS; i++) digits += randomInt(10).toString();
  let letters = "";
  for (let i = 0; i < CODE_LETTERS; i++) letters += LETTER_ALPHABET[randomInt(LETTER_ALPHABET.length)];
  return digits + letters;
}

/** Client IP as seen by Cloudflare's edge; falls back to a constant bucket if absent (e.g. local dev). */
function clientIp(request: Request): string {
  return request.headers.get("CF-Connecting-IP") ?? "unknown";
}

const RATE_LIMIT_WINDOW_MS = 60_000;
// Generous enough for two peers behind the same NAT each polling their
// session once a second, while capping any single IP's guess rate at a
// fraction the code space above makes brute force unattainable at.
const RATE_LIMIT_MAX_REQUESTS = 120;

/**
 * Per-IP rate limiter for the code-guessing surface (`/api/session/:code/*`).
 * One Durable Object instance per IP, holding a fixed-window counter. This
 * is what actually stands between the code space above and a brute-force
 * sweep — the DO gives a strongly consistent, edge-wide count that a
 * per-isolate in-memory counter could not.
 */
export class RateLimiter implements DurableObject {
  private windowStart = 0;
  private count = 0;

  constructor(private state: DurableObjectState) {}

  async fetch(): Promise<Response> {
    const now = Date.now();
    if (this.windowStart === 0) {
      const stored = await this.state.storage.get<{ windowStart: number; count: number }>("bucket");
      if (stored) {
        this.windowStart = stored.windowStart;
        this.count = stored.count;
      }
    }

    if (now - this.windowStart >= RATE_LIMIT_WINDOW_MS) {
      this.windowStart = now;
      this.count = 0;
    }

    this.count++;
    await this.state.storage.put("bucket", { windowStart: this.windowStart, count: this.count });
    // Let idle buckets expire on their own so storage doesn't accumulate forever.
    await this.state.storage.setAlarm(now + RATE_LIMIT_WINDOW_MS);

    return json({ allowed: this.count <= RATE_LIMIT_MAX_REQUESTS });
  }

  async alarm(): Promise<void> {
    await this.state.storage.deleteAll();
    this.windowStart = 0;
    this.count = 0;
  }
}

/** Returns true if this IP is still within its budget for the code-gated routes. */
async function checkRateLimit(env: Env, ip: string): Promise<boolean> {
  const stub = env.RATE_LIMITER.get(env.RATE_LIMITER.idFromName(ip));
  const resp = await stub.fetch("https://ratelimit/");
  const data = (await resp.json()) as { allowed: boolean };
  return data.allowed;
}

/** Fetch short-lived ICE server credentials from the Cloudflare TURN API. */
async function fetchIceServers(env: Env): Promise<unknown> {
  const url = `https://rtc.live.cloudflare.com/v1/turn/keys/${env.TURN_TOKEN_ID}/credentials/generate-ice-servers`;
  const resp = await fetch(url, {
    method: "POST",
    headers: {
      Authorization: `Bearer ${env.TURN_API_TOKEN}`,
      "Content-Type": "application/json",
    },
    body: JSON.stringify({ ttl: 86400 }),
  });
  if (!resp.ok) {
    throw new Error(`TURN API returned ${resp.status}: ${await resp.text()}`);
  }
  const data = (await resp.json()) as { iceServers: unknown };
  return data.iceServers;
}

/**
 * One Durable Object instance per session code. All reads/writes for a
 * given code are serialized through this single object, so an offer and
 * an answer landing at the same time can no longer clobber each other,
 * and there's no edge cache to serve a stale "no answer yet" response.
 */
export class Session implements DurableObject {
  private session: SessionData | null | undefined;

  constructor(private state: DurableObjectState) {}

  private async load(): Promise<void> {
    if (this.session === undefined) {
      this.session = (await this.state.storage.get<SessionData>("session")) ?? null;
    }
  }

  async fetch(request: Request): Promise<Response> {
    await this.load();
    const url = new URL(request.url);
    const path = url.pathname;

    if (path === "/" && request.method === "POST") {
      if (this.session) return err("Pin already in use", 409);
      this.session = { offer: null, answer: null };
      await this.state.storage.put("session", this.session);
      await this.state.storage.setAlarm(Date.now() + SESSION_TTL * 1000);
      return json({ ok: true });
    }

    if (!this.session) return err("Session not found or expired", 404);

    if (path === "/" && request.method === "GET") {
      return json({ ok: true });
    }

    if (path === "/offer" && request.method === "PUT") {
      this.session.offer = await request.text();
      await this.state.storage.put("session", this.session);
      return json({ ok: true });
    }

    if (path === "/offer" && request.method === "GET") {
      return json(this.session.offer ? { ready: true, offer: this.session.offer } : { ready: false });
    }

    if (path === "/answer" && request.method === "PUT") {
      this.session.answer = await request.text();
      await this.state.storage.put("session", this.session);
      return json({ ok: true });
    }

    if (path === "/answer" && request.method === "GET") {
      return json(this.session.answer ? { ready: true, answer: this.session.answer } : { ready: false });
    }

    if (path === "/" && request.method === "DELETE") {
      this.session = null;
      await this.state.storage.deleteAll();
      return json({ ok: true });
    }

    return err("Not found", 404);
  }

  async alarm(): Promise<void> {
    this.session = null;
    await this.state.storage.deleteAll();
  }
}

async function handleRequest(request: Request, env: Env): Promise<Response> {
  const url = new URL(request.url);
  const path = url.pathname;

  // CORS preflight
  if (request.method === "OPTIONS") {
    return new Response(null, { status: 204, headers: CORS_HEADERS });
  }

  // POST /api/session — create a new session
  if (path === "/api/session" && request.method === "POST") {
    // Optionally accept a custom code in the request body
    let requestedCode: string | undefined;
    const contentType = request.headers.get("Content-Type") || "";
    if (contentType.includes("application/json")) {
      try {
        const body = (await request.json()) as { code?: string };
        if (body.code) {
          if (!CODE_PATTERN.test(body.code)) {
            return err("Custom code must be 6 digits followed by 4 lowercase letters", 400);
          }
          requestedCode = body.code;
        }
      } catch {
        // empty or invalid JSON body is fine, fall through to random generation
      }
    }

    let code: string;
    if (requestedCode) {
      const stub = env.SESSION_DO.get(env.SESSION_DO.idFromName(requestedCode));
      const created = await stub.fetch("https://session/", { method: "POST" });
      if (created.status === 409) {
        return err("Pin already in use", 409);
      }
      code = requestedCode;
    } else {
      // Generate a unique 6-digit code (retry on collision). The Durable
      // Object's create is atomic, so this can't race with another
      // request creating the same code between a check and a write.
      code = generateCode();
      let attempts = 0;
      for (;;) {
        const stub = env.SESSION_DO.get(env.SESSION_DO.idFromName(code));
        const created = await stub.fetch("https://session/", { method: "POST" });
        if (created.status !== 409) break;
        attempts++;
        if (attempts >= 10) {
          return err("Too many active sessions, try again later", 503);
        }
        code = generateCode();
      }
    }

    let iceServers: unknown;
    try {
      iceServers = await fetchIceServers(env);
    } catch (e) {
      return err(`Failed to fetch ICE servers: ${e}`, 502);
    }

    return json({ code, iceServers });
  }

  // Match /api/session/:code routes
  const match = path.match(/^\/api\/session\/([0-9]{6}[a-z]{4})(\/.*)?$/);
  if (!match) {
    return err("Not found", 404);
  }

  // Every request against a specific code is a potential guess at that
  // code, whichever sub-route it targets — so the budget is shared across
  // all of them, keyed by IP.
  if (!(await checkRateLimit(env, clientIp(request)))) {
    return err("Too many requests, slow down", 429);
  }

  const code = match[1];
  const sub = match[2] || "";
  const stub = env.SESSION_DO.get(env.SESSION_DO.idFromName(code));

  // GET /api/session/:code — get session info + ICE servers
  if (sub === "" && request.method === "GET") {
    const resp = await stub.fetch("https://session/", { method: "GET" });
    if (resp.status === 404) return err("Session not found or expired", 404);

    let iceServers: unknown;
    try {
      iceServers = await fetchIceServers(env);
    } catch (e) {
      return err(`Failed to fetch ICE servers: ${e}`, 502);
    }

    return json({ code, iceServers });
  }

  // PUT /api/session/:code/offer — store offer SDP
  if (sub === "/offer" && request.method === "PUT") {
    const body = await request.text();
    if (!body) return err("Empty body", 400);

    const resp = await stub.fetch("https://session/offer", { method: "PUT", body });
    if (resp.status === 404) return err("Session not found or expired", 404);
    return json({ ok: true });
  }

  // GET /api/session/:code/offer — get offer SDP
  if (sub === "/offer" && request.method === "GET") {
    const resp = await stub.fetch("https://session/offer", { method: "GET" });
    if (resp.status === 404) return err("Session not found or expired", 404);
    return json(await resp.json());
  }

  // PUT /api/session/:code/answer — store answer SDP
  if (sub === "/answer" && request.method === "PUT") {
    const body = await request.text();
    if (!body) return err("Empty body", 400);

    const resp = await stub.fetch("https://session/answer", { method: "PUT", body });
    if (resp.status === 404) return err("Session not found or expired", 404);
    return json({ ok: true });
  }

  // GET /api/session/:code/answer — get answer SDP
  if (sub === "/answer" && request.method === "GET") {
    const resp = await stub.fetch("https://session/answer", { method: "GET" });
    if (resp.status === 404) return err("Session not found or expired", 404);
    return json(await resp.json());
  }

  // DELETE /api/session/:code — remove a session (called by the host on exit)
  if (sub === "" && request.method === "DELETE") {
    await stub.fetch("https://session/", { method: "DELETE" });
    return json({ ok: true });
  }

  return err("Not found", 404);
}

export default {
  fetch: handleRequest,
} satisfies ExportedHandler<Env>;
