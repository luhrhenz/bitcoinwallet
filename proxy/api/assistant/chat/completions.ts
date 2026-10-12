// POST /api/assistant/chat/completions: the wallet assistant's OpenAI-compatible relay to Groq.
// The app uses `<proxy>/api/assistant` as its base URL; the Groq key is added here.

import { checkChat, clientKey, DEFAULT_GROQ_MODEL, json, RateLimiter } from "../../../lib/guard.js";

const GROQ_URL = "https://api.groq.com/openai/v1/chat/completions";
// A question takes one to seven model calls (tool rounds).
const limiter = new RateLimiter(40, 60_000);

export async function POST(req: Request): Promise<Response> {
  const key = process.env.GROQ_API_KEY;
  if (!key) return json(503, { error: { message: "assistant relay not configured" } });
  if (!limiter.allow(clientKey(req))) {
    return json(429, { error: { message: "too many requests; wait a minute" } }, { "retry-after": "30" });
  }

  const model = process.env.GROQ_MODEL?.trim() || DEFAULT_GROQ_MODEL;
  const check = checkChat(await req.text(), model);
  if (!check.ok) return json(check.status, { error: { message: check.error } });

  let answer: Response;
  try {
    answer = await fetch(GROQ_URL, {
      method: "POST",
      headers: { "content-type": "application/json", authorization: `Bearer ${key}` },
      body: JSON.stringify(check.value),
      signal: AbortSignal.timeout(55_000),
    });
  } catch {
    return json(502, { error: { message: "assistant provider unreachable" } });
  }
  return new Response(await answer.text(), {
    status: answer.status,
    headers: { "content-type": "application/json", "cache-control": "no-store" },
  });
}
