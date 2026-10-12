// Checks shared by both relays: what may pass, how big, how often.

/** The JSON-RPC methods the wallet uses. Anything else (wallet RPCs, mining, admin) is refused. */
export const RPC_METHODS = new Set([
  "getblockchaininfo",
  "getblockcount",
  "getblockhash",
  "getblock",
  "getrawmempool",
  "getrawtransaction",
  "sendrawtransaction",
  "estimatesmartfee",
]);

/** Largest request body accepted: a raw transaction for sendrawtransaction fits easily. */
export const MAX_RPC_BODY = 256 * 1024;
/** Chat requests carry the conversation and tool definitions. */
export const MAX_CHAT_BODY = 128 * 1024;
/** Cap on the assistant's answer length. */
export const MAX_COMPLETION_TOKENS = 1024;
export const DEFAULT_GROQ_MODEL = "openai/gpt-oss-120b";

export type Check<T> = { ok: true; value: T } | { ok: false; status: number; error: string };

const fail = (status: number, error: string): Check<never> => ({ ok: false, status, error });

/** A single JSON-RPC call to an allowed method. Batches are refused. */
export function checkRpc(body: string): Check<{ method: string }> {
  if (body.length > MAX_RPC_BODY) return fail(413, "request too large");
  let parsed: unknown;
  try {
    parsed = JSON.parse(body);
  } catch {
    return fail(400, "request is not JSON");
  }
  if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) {
    return fail(400, "expected one JSON-RPC call");
  }
  const method = (parsed as { method?: unknown }).method;
  if (typeof method !== "string" || !RPC_METHODS.has(method)) {
    return fail(403, `method not allowed: ${String(method).slice(0, 40)}`);
  }
  return { ok: true, value: { method } };
}

/**
 * The chat request to forward: the client's messages and tools, but our model and a capped
 * answer length, so the relay can't be used as a general-purpose free model.
 */
export function checkChat(body: string, model: string): Check<Record<string, unknown>> {
  if (body.length > MAX_CHAT_BODY) return fail(413, "request too large");
  let parsed: unknown;
  try {
    parsed = JSON.parse(body);
  } catch {
    return fail(400, "request is not JSON");
  }
  if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) {
    return fail(400, "expected a chat completion request");
  }
  const req = parsed as Record<string, unknown>;
  if (!Array.isArray(req.messages) || req.messages.length === 0) return fail(400, "messages are required");
  // The wallet always sends its tools; requiring them keeps this a wallet assistant.
  if (!Array.isArray(req.tools) || req.tools.length === 0) return fail(400, "tools are required");
  if (req.stream === true) return fail(400, "streaming is not supported");
  return {
    ok: true,
    value: {
      model,
      messages: req.messages,
      tools: req.tools,
      tool_choice: req.tool_choice ?? "auto",
      temperature: typeof req.temperature === "number" ? Math.min(Math.max(req.temperature, 0), 1) : 0.2,
      max_completion_tokens: MAX_COMPLETION_TOKENS,
    },
  };
}

/**
 * Fixed-window rate limit per client address. Best effort: state lives in one function
 * instance, so it slows down abuse without guaranteeing a global limit (use Vercel's firewall
 * rules for that).
 */
export class RateLimiter {
  private readonly hits = new Map<string, { windowStart: number; count: number }>();

  constructor(
    private readonly limit: number,
    private readonly windowMs: number,
  ) {}

  allow(key: string, now = Date.now()): boolean {
    const entry = this.hits.get(key);
    if (!entry || now - entry.windowStart >= this.windowMs) {
      this.hits.set(key, { windowStart: now, count: 1 });
      if (this.hits.size > 10_000) this.prune(now);
      return true;
    }
    entry.count += 1;
    return entry.count <= this.limit;
  }

  private prune(now: number) {
    for (const [key, entry] of this.hits) {
      if (now - entry.windowStart >= this.windowMs) this.hits.delete(key);
    }
  }
}

/** The caller's address as Vercel reports it. */
export function clientKey(req: Request): string {
  const forwarded = req.headers.get("x-forwarded-for") ?? "";
  return forwarded.split(",")[0]?.trim() || req.headers.get("x-real-ip") || "unknown";
}

export function json(status: number, body: unknown, extra: Record<string, string> = {}): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json", "cache-control": "no-store", ...extra },
  });
}
