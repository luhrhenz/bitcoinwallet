// POST /api/rpc: Bitcoin testnet4 JSON-RPC relay to the endpoint in ALCHEMY_RPC_URL.
// Only the methods the wallet uses pass (see lib/guard.ts); the endpoint and its key stay here.

import { checkRpc, clientKey, json, RateLimiter } from "../lib/guard.js";

// A first sync fetches a few requests per block; this allows a steady catch-up without
// letting one client hammer the upstream plan.
const limiter = new RateLimiter(1200, 60_000);

export async function POST(req: Request): Promise<Response> {
  const upstream = process.env.ALCHEMY_RPC_URL;
  if (!upstream) return json(503, rpcError("relay not configured"));
  if (!limiter.allow(clientKey(req))) {
    return json(429, rpcError("too many requests; slow down"), { "retry-after": "10" });
  }

  const body = await req.text();
  const check = checkRpc(body);
  if (!check.ok) return json(check.status, rpcError(check.error));

  let answer: Response;
  try {
    answer = await fetch(upstream, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body,
      signal: AbortSignal.timeout(55_000),
    });
  } catch {
    return json(502, rpcError("upstream node unreachable"));
  }
  // Pass the node's reply through as-is (JSON-RPC errors included), never upstream headers.
  return new Response(await answer.text(), {
    status: answer.status,
    headers: { "content-type": "application/json", "cache-control": "no-store" },
  });
}

export function GET(): Response {
  return json(405, rpcError("use POST with a JSON-RPC body"));
}

function rpcError(message: string) {
  return { result: null, error: { code: -32600, message }, id: null };
}
