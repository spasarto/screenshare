interface Env {
  SESSIONS: KVNamespace;
  TURN_TOKEN_ID: string;
  TURN_API_TOKEN: string;
}

interface Session {
  offer?: string;
  answer?: string;
}

const SESSION_TTL = 300; // 5 minutes
const CORS_HEADERS = {
  "Access-Control-Allow-Origin": "*",
  "Access-Control-Allow-Methods": "GET, POST, PUT, DELETE, OPTIONS",
  "Access-Control-Allow-Headers": "Content-Type",
};

function json(data: unknown, status = 200): Response {
  return new Response(JSON.stringify(data), {
    status,
    headers: { "Content-Type": "application/json", ...CORS_HEADERS },
  });
}

function err(message: string, status: number): Response {
  return json({ error: message }, status);
}

/** Generate a random 6-digit numeric code. */
function generateCode(): string {
  const n = Math.floor(Math.random() * 900_000) + 100_000;
  return n.toString();
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
          if (!/^\d{6}$/.test(body.code)) {
            return err("Custom code must be exactly 6 digits", 400);
          }
          requestedCode = body.code;
        }
      } catch {
        // empty or invalid JSON body is fine, fall through to random generation
      }
    }

    let code: string;
    if (requestedCode) {
      // Use the requested code, but check for collision
      const existing = await env.SESSIONS.get(requestedCode);
      if (existing) {
        return err("Pin already in use", 409);
      }
      code = requestedCode;
    } else {
      // Generate a unique 6-digit code (retry on collision)
      code = generateCode();
      let attempts = 0;
      do {
        const existing = await env.SESSIONS.get(code);
        if (!existing) break;
        code = generateCode();
        attempts++;
      } while (attempts < 10);

      if (attempts >= 10) {
        return err("Too many active sessions, try again later", 503);
      }
    }

    let iceServers: unknown;
    try {
      iceServers = await fetchIceServers(env);
    } catch (e) {
      return err(`Failed to fetch ICE servers: ${e}`, 502);
    }

    const session: Session = {};
    await env.SESSIONS.put(code, JSON.stringify(session), {
      expirationTtl: SESSION_TTL,
    });

    return json({ code, iceServers });
  }

  // Match /api/session/:code routes
  const match = path.match(/^\/api\/session\/(\d{6})(\/.*)?$/);
  if (!match) {
    return err("Not found", 404);
  }

  const code = match[1];
  const sub = match[2] || "";

  // GET /api/session/:code — get session info + ICE servers
  if (sub === "" && request.method === "GET") {
    const raw = await env.SESSIONS.get(code);
    if (!raw) return err("Session not found or expired", 404);

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
    const raw = await env.SESSIONS.get(code);
    if (!raw) return err("Session not found or expired", 404);

    const session: Session = JSON.parse(raw);
    const body = await request.text();
    if (!body) return err("Empty body", 400);

    session.offer = body;
    await env.SESSIONS.put(code, JSON.stringify(session), {
      expirationTtl: SESSION_TTL,
    });
    return json({ ok: true });
  }

  // GET /api/session/:code/offer — get offer SDP
  if (sub === "/offer" && request.method === "GET") {
    const raw = await env.SESSIONS.get(code);
    if (!raw) return err("Session not found or expired", 404);

    const session: Session = JSON.parse(raw);
    if (!session.offer) {
      return json({ ready: false }, 200);
    }
    return json({ ready: true, offer: session.offer });
  }

  // PUT /api/session/:code/answer — store answer SDP
  if (sub === "/answer" && request.method === "PUT") {
    const raw = await env.SESSIONS.get(code);
    if (!raw) return err("Session not found or expired", 404);

    const session: Session = JSON.parse(raw);
    const body = await request.text();
    if (!body) return err("Empty body", 400);

    session.answer = body;
    await env.SESSIONS.put(code, JSON.stringify(session), {
      expirationTtl: SESSION_TTL,
    });
    return json({ ok: true });
  }

  // GET /api/session/:code/answer — get answer SDP
  if (sub === "/answer" && request.method === "GET") {
    const raw = await env.SESSIONS.get(code);
    if (!raw) return err("Session not found or expired", 404);

    const session: Session = JSON.parse(raw);
    if (!session.answer) {
      return json({ ready: false }, 200);
    }
    return json({ ready: true, answer: session.answer });
  }

  // DELETE /api/session/:code — remove a session (called by the host on exit)
  if (sub === "" && request.method === "DELETE") {
    await env.SESSIONS.delete(code);
    return json({ ok: true });
  }

  return err("Not found", 404);
}

export default {
  fetch: handleRequest,
} satisfies ExportedHandler<Env>;
