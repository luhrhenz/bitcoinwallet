import { afterEach, describe, expect, it, vi } from "vitest";
import { POST as chat } from "../api/assistant/chat/completions";
import { POST as rpc } from "../api/rpc";
import { checkChat, checkRpc, MAX_COMPLETION_TOKENS, RateLimiter, RPC_METHODS } from "./guard";

const rpcCall = (method: string) => JSON.stringify({ jsonrpc: "2.0", id: 1, method, params: [] });
const request = (body: string, ip = "1.2.3.4") =>
  new Request("https://proxy.test/api", { method: "POST", body, headers: { "x-forwarded-for": ip } });

afterEach(() => {
  vi.unstubAllGlobals();
  vi.unstubAllEnvs();
});

describe("checkRpc", () => {
  it("allows exactly the wallet's methods", () => {
    for (const method of RPC_METHODS) expect(checkRpc(rpcCall(method)).ok).toBe(true);
    for (const method of ["generatetoaddress", "stop", "dumpprivkey", "listwallets", "sendtoaddress"]) {
      expect(checkRpc(rpcCall(method))).toMatchObject({ ok: false, status: 403 });
    }
  });

  it("refuses batches, junk and oversized bodies", () => {
    expect(checkRpc(`[${rpcCall("getblockcount")}]`)).toMatchObject({ ok: false, status: 400 });
    expect(checkRpc("not json")).toMatchObject({ ok: false, status: 400 });
    expect(checkRpc("x".repeat(300_000))).toMatchObject({ ok: false, status: 413 });
  });
});

describe("checkChat", () => {
  const tools = [{ type: "function", function: { name: "get_balance", parameters: {} } }];

  it("forces our model and caps the answer", () => {
    const check = checkChat(
      JSON.stringify({ model: "some-expensive-model", messages: [{ role: "user", content: "hi" }], tools, max_tokens: 99999 }),
      "openai/gpt-oss-120b",
    );
    expect(check.ok).toBe(true);
    if (!check.ok) return;
    expect(check.value.model).toBe("openai/gpt-oss-120b");
    expect(check.value.max_completion_tokens).toBe(MAX_COMPLETION_TOKENS);
    expect(check.value).not.toHaveProperty("max_tokens");
  });

  it("requires messages and the wallet's tools, and no streaming", () => {
    const messages = [{ role: "user", content: "hi" }];
    expect(checkChat(JSON.stringify({ tools }), "m")).toMatchObject({ ok: false, status: 400 });
    expect(checkChat(JSON.stringify({ messages }), "m")).toMatchObject({ ok: false, status: 400 });
    expect(checkChat(JSON.stringify({ messages, tools, stream: true }), "m")).toMatchObject({ ok: false });
  });
});

describe("RateLimiter", () => {
  it("allows up to the limit per window, per client", () => {
    const limiter = new RateLimiter(2, 1000);
    expect([limiter.allow("a", 0), limiter.allow("a", 1), limiter.allow("a", 2)]).toEqual([true, true, false]);
    expect(limiter.allow("b", 2)).toBe(true);
    expect(limiter.allow("a", 1000)).toBe(true);
  });
});

describe("/api/rpc", () => {
  it("relays allowed calls to the configured node, without exposing it", async () => {
    vi.stubEnv("ALCHEMY_RPC_URL", "https://node.example/v2/SECRETKEY");
    const upstream = vi.fn(async () => new Response('{"result":156263,"error":null,"id":1}', { status: 200 }));
    vi.stubGlobal("fetch", upstream);
    const res = await rpc(request(rpcCall("getblockcount")));
    expect(res.status).toBe(200);
    expect(await res.json()).toMatchObject({ result: 156263 });
    expect(upstream).toHaveBeenCalledWith("https://node.example/v2/SECRETKEY", expect.anything());
  });

  it("refuses other methods without contacting the node", async () => {
    vi.stubEnv("ALCHEMY_RPC_URL", "https://node.example/v2/SECRETKEY");
    const upstream = vi.fn();
    vi.stubGlobal("fetch", upstream);
    const res = await rpc(request(rpcCall("dumpprivkey")));
    expect(res.status).toBe(403);
    expect(await res.text()).not.toContain("SECRETKEY");
    expect(upstream).not.toHaveBeenCalled();
  });

  it("says when it isn't configured", async () => {
    vi.stubEnv("ALCHEMY_RPC_URL", "");
    expect((await rpc(request(rpcCall("getblockcount")))).status).toBe(503);
  });
});

describe("/api/assistant/chat/completions", () => {
  it("adds the key server-side and forwards our model", async () => {
    vi.stubEnv("GROQ_API_KEY", "gsk_SERVERKEY");
    vi.stubEnv("GROQ_MODEL", "");
    let sent: { headers?: Record<string, string>; body?: string } = {};
    vi.stubGlobal(
      "fetch",
      vi.fn(async (_url: string, init: { headers: Record<string, string>; body: string }) => {
        sent = init;
        return new Response('{"choices":[{"message":{"role":"assistant","content":"ok"}}]}', { status: 200 });
      }),
    );
    const body = JSON.stringify({
      model: "anything",
      messages: [{ role: "user", content: "balance?" }],
      tools: [{ type: "function", function: { name: "get_balance", parameters: {} } }],
    });
    const res = await chat(request(body, "5.6.7.8"));
    expect(res.status).toBe(200);
    expect(sent.headers?.authorization).toBe("Bearer gsk_SERVERKEY");
    expect(JSON.parse(sent.body ?? "{}").model).toBe("openai/gpt-oss-120b");
    expect(await res.text()).not.toContain("gsk_SERVERKEY");
  });
});
