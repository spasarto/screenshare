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

/**
 * The landing page behind the share link the host copies (and the QR it
 * shows): `/j/:code`. Deliberately static — it neither creates nor looks up
 * the session, so opening a link costs the recipient's IP nothing from the
 * rate-limit budget and tells a stranger with a guessed code nothing about
 * whether it exists. All it does is carry the code somewhere it can be
 * copied on whatever device the peer opened it on.
 */
function joinPage(code: string): Response {
  const grouped = `${code.slice(0, CODE_DIGITS)} · ${code.slice(CODE_DIGITS)}`;
  const html = `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1,viewport-fit=cover">
<title>Join a screen share</title>
<style>
  :root {
    color-scheme: light dark;
    --bg: #F2F5F5; --surface: #FFFFFF; --ink: #101719; --ink-2: #3D4A4D;
    --ink-3: #6B7A7E; --hairline: #D6DEDE; --accent: #16707F; --on-accent: #FFFFFF;
  }
  @media (prefers-color-scheme: dark) {
    :root {
      --bg: #0C1214; --surface: #131B1E; --ink: #E4EBEC; --ink-2: #AFBEC1;
      --ink-3: #7D8E92; --hairline: #263236; --accent: #5BC6D6; --on-accent: #0C1214;
    }
  }
  * { box-sizing: border-box; }
  body {
    margin: 0; min-height: 100vh; display: flex; align-items: center; justify-content: center;
    padding: 24px; background: var(--bg); color: var(--ink);
    font: 16px/1.6 system-ui, -apple-system, "Segoe UI", sans-serif;
  }
  main {
    width: 100%; max-width: 27rem; background: var(--surface);
    border: 1px solid var(--hairline); border-radius: 12px; padding: 28px 24px;
  }
  h1 { margin: 0 0 4px; font-size: 1.25rem; letter-spacing: -.01em; }
  p { margin: 0; color: var(--ink-2); }
  .label {
    margin: 24px 0 6px; font-size: .6875rem; letter-spacing: .12em;
    text-transform: uppercase; color: var(--ink-3);
  }
  .code {
    font-family: ui-monospace, "SF Mono", Consolas, monospace;
    font-size: clamp(1.75rem, 9vw, 2.5rem); font-weight: 600; letter-spacing: .02em;
    word-break: break-word;
  }
  button {
    margin-top: 16px; width: 100%; padding: 12px 16px; font: inherit; font-weight: 600;
    color: var(--on-accent); background: var(--accent); border: 0; border-radius: 8px;
    cursor: pointer;
  }
  button:focus-visible { outline: 2px solid var(--ink); outline-offset: 2px; }
  ol { margin: 20px 0 0; padding-left: 1.25em; color: var(--ink-2); }
  li { margin-bottom: 4px; }
  .note { margin-top: 20px; font-size: .875rem; color: var(--ink-3); }
</style>
</head>
<body>
<main>
  <h1>You've been invited to a screen share</h1>
  <p>Both sides share a window, so you'll pick something to share too.</p>

  <div class="label">Session code</div>
  <div class="code" id="code">${grouped}</div>
  <button id="copy" data-code="${code}">Copy code</button>

  <ol>
    <li>Open Screen Share on the computer you want to share from.</li>
    <li>Choose <strong>Join a session</strong>.</li>
    <li>Paste the code, pick a window, and press Start.</li>
  </ol>

  <p class="note">Codes are single-use and expire a few minutes after the host creates them. If this one has expired, ask for a new link.</p>
</main>
<script>
  const button = document.getElementById("copy");
  button.addEventListener("click", async () => {
    try {
      await navigator.clipboard.writeText(button.dataset.code);
      button.textContent = "Copied";
    } catch {
      // Clipboard access can be refused (insecure context, permissions);
      // selecting the code is the fallback that always works.
      getSelection().selectAllChildren(document.getElementById("code"));
      button.textContent = "Copy it from above";
    }
    setTimeout(() => { button.textContent = "Copy code"; }, 2000);
  });
</script>
</body>
</html>`;

  return new Response(html, {
    status: 200,
    headers: {
      "Content-Type": "text/html; charset=utf-8",
      // The page is the same for a given code, but a session is short-lived
      // and single-use — no reason for an intermediary to hold on to it.
      "Cache-Control": "no-store",
    },
  });
}

async function handleRequest(request: Request, env: Env): Promise<Response> {
  const url = new URL(request.url);
  const path = url.pathname;

  // CORS preflight
  if (request.method === "OPTIONS") {
    return new Response(null, { status: 204, headers: CORS_HEADERS });
  }

  // GET /j/:code — the human-facing share link (and what the QR encodes)
  const join = path.match(/^\/j\/([0-9]{6}[a-z]{4})$/);
  if (join && request.method === "GET") {
    return joinPage(join[1]);
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
