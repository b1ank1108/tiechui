import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { PassThrough, Writable } from "node:stream";
import { test } from "node:test";
import { RpcError, serve } from "../index.js";
import type { Plugin } from "../index.d.ts";

const plugin: Plugin = {
  async tool(_name, arguments_, ctx) {
    await ctx.progress(1, "Working");
    return { text: (arguments_ as { text: string }).text, isError: false };
  },
  before() { return { action: "deny", reason: "Blocked" }; },
  after() { return null; },
  async command(_name, argument, ctx) {
    switch (argument) {
      case "notify": assert.equal(await ctx.notify("Title", "Text"), undefined); break;
      case "select": await ctx.select("Title", "Text", ["First"]); break;
      case "confirm": await ctx.confirm("Title", "Text"); break;
      case "input": await ctx.input("Title", "Text"); break;
      case "status": await ctx.status("Ready"); break;
      default: await ctx.panel({ title: "Title", nodes: [{ kind: "text", text: "Text" }] });
    }
    return "Done";
  },
};

function fixture(handlers: Plugin = plugin) {
  const input = new PassThrough(), output = new PassThrough(), controller = new AbortController();
  const frames: unknown[] = [], waiters: ((v: unknown) => void)[] = [];
  let buffer = "";
  output.setEncoding("utf8");
  output.on("data", (chunk) => {
    buffer += chunk;
    while (buffer.includes("\n")) {
      const end = buffer.indexOf("\n"), frame = JSON.parse(buffer.slice(0, end));
      buffer = buffer.slice(end + 1);
      if (waiters.length) waiters.shift()!(frame); else frames.push(frame);
    }
  });
  const done = serve(handlers, { input, output, signal: controller.signal });
  return {
    input, controller, done,
    send: (id: number, method: string, params: unknown = {}) => input.write(JSON.stringify({ jsonrpc: "2.0", id, method, params }) + "\n"),
    next: () => frames.length ? Promise.resolve(frames.shift()) : new Promise((resolve) => waiters.push(resolve)),
  };
}

test("recorded bidirectional transcript, including split UTF-8 and cancellation", { timeout: 5000 }, async () => {
  const f = fixture();
  try {
    const transcript = await readFile(new URL("../../../tests/plugins/transcripts/sdk.jsonl", import.meta.url), "utf8");
    for (const line of transcript.trim().split("\n")) {
      const step = JSON.parse(line);
      if (step.send) {
        const bytes = Buffer.from(JSON.stringify(step.send) + "\n");
        for (const byte of bytes) f.input.write(Buffer.from([byte]));
      } else assert.deepEqual(await f.next(), step.receive);
    }
    await f.done;
  } finally { f.controller.abort(); }
});

test("invalid envelopes, UTF-8, truncated and oversized frames close the transport", async () => {
  for (const bytes of [
    Buffer.from('[]\n'), Buffer.from('{"jsonrpc":"1.0","id":1,"result":null}\n'),
    Buffer.from('{"jsonrpc":"2.0","id":-1,"result":null}\n'),
    Buffer.from('{"jsonrpc":"2.0","id":1,"result":null,"error":{"code":1,"message":"x"}}\n'),
    Buffer.from('{"jsonrpc":"2.0","id":1,"method":"x","params":null}\n'),
    Buffer.from([0xff, 10]), Buffer.from('{'), Buffer.alloc(1024 * 1024 + 1, 32),
  ]) {
    const f = fixture();
    const rejected = assert.rejects(f.done);
    f.input.end(bytes);
    await rejected;
  }
});

const init = { apiVersion: 1, capabilities: ["tools"], contributions: { tools: [{ name: "echo", description: "Echo", inputSchema: {} }], commands: [], interceptors: [] } };

test("version, capabilities, invalid parameters, handler errors and shutdown", async () => {
  const f = fixture({ tool() { throw new RpcError(-32000, "Failed"); } });
  try {
    f.send(1, "initialize", { ...init, apiVersion: 2 });
    assert.equal((await f.next() as any).error.code, -32002);
    f.send(2, "initialize", init); await f.next();
    let id = 3;
    for (const [method, params, code] of [
      ["initialize", init, -32600], ["commands/list", {}, -32001],
      ["tools/call", { name: "missing", arguments: {} }, -32602],
      ["tools/call", { name: "echo" }, -32602],
      ["tools/call", { name: "echo", arguments: {} }, -32000],
    ] as const) {
      f.send(id++, method, params);
      assert.equal((await f.next() as any).error.code, code);
    }
    f.send(id, "shutdown"); await f.next(); await f.done;
  } finally { f.controller.abort(); }
});

test("cancelling an uncooperative handler settles and ignores its late completion", async () => {
  let complete: (value: any) => void = () => {};
  let handlerSignal: AbortSignal | undefined;
  const f = fixture({ tool(_name, _args, ctx) { handlerSignal = ctx.signal; return new Promise((resolve) => { complete = resolve; }); } });
  try {
    f.send(1, "initialize", init); await f.next();
    f.send(2, "tools/call", { name: "echo", arguments: {} });
    f.input.write('{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":2}}\n');
    assert.equal((await f.next() as any).error.code, -32800);
    assert.equal(handlerSignal?.aborted, true);
    complete({ text: "late", isError: false });
    f.send(3, "shutdown"); assert.equal((await f.next() as any).id, 3); await f.done;
  } finally { f.controller.abort(); }
});

test("disconnect aborts handlers and rejects pending UI without replay", async () => {
  let rejected = false, signal: AbortSignal | undefined;
  const f = fixture({ async tool(_name, _args, ctx) {
    signal = ctx.signal;
    try { await ctx.input("Title", "Text"); } catch { rejected = true; }
    return { text: "done", isError: false };
  } });
  f.send(1, "initialize", init); await f.next();
  f.send(2, "tools/call", { name: "echo", arguments: {} }); await f.next();
  f.input.end(); await f.done;
  assert.equal(signal?.aborted, true);
  assert.equal(rejected, true);
});

test("initialization can be cancelled without blocking the reader", { timeout: 1000 }, async () => {
  const f = fixture({ initialize() { return new Promise(() => {}); } });
  try {
    f.send(1, "initialize", init);
    f.input.write('{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":1}}\n');
    assert.equal((await f.next() as any).error.code, -32800);
  } finally { f.controller.abort(); await f.done; }
});

test("write failure rejects the server", async () => {
  const input = new PassThrough();
  const output = new Writable({ write(_chunk, _encoding, callback) { callback(new Error("broken pipe")); } });
  const done = serve(plugin, { input, output, signal: new AbortController().signal });
  const rejected = assert.rejects(done, /broken pipe/);
  input.write('{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}\n');
  await rejected;
});
