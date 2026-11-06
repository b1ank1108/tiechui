import { TextDecoder } from "node:util";

const MAX_FRAME = 1024 * 1024;
const object = (v) => v !== null && typeof v === "object" && !Array.isArray(v);
const idValid = (id) => Number.isSafeInteger(id) && id >= 0;
const own = (v, key) => Object.hasOwn(v, key);
const keys = (v, allowed) => Object.keys(v).every((key) => allowed.includes(key));
const cancelled = () => new RpcError(-32800, "Cancelled");

export class RpcError extends Error {
  constructor(code, message, data) {
    super(message);
    this.code = code;
    if (data !== undefined) this.data = data;
  }
}

function envelope(value) {
  if (!object(value) || value.jsonrpc !== "2.0") throw new RpcError(-32600, "Invalid envelope");
  if (own(value, "id") && !idValid(value.id)) throw new RpcError(-32600, "Invalid ID");
  if (typeof value.method === "string") {
    if (!keys(value, ["jsonrpc", "id", "method", "params"]) ||
        (own(value, "params") && !object(value.params) && !Array.isArray(value.params))) {
      throw new RpcError(-32600, "Invalid request");
    }
  } else if (!own(value, "id") || own(value, "result") === own(value, "error") ||
      !keys(value, ["jsonrpc", "id", "result", "error"]) ||
      (own(value, "error") && (!object(value.error) || !Number.isSafeInteger(value.error.code) ||
        typeof value.error.message !== "string" || !keys(value.error, ["code", "message", "data"])))) {
    throw new RpcError(-32600, "Invalid response");
  }
  return value;
}

function initialize(params) {
  const capabilities = ["tools", "commands", "tool.intercept", "ui.notify", "ui.select", "ui.confirm", "ui.input", "status.set", "ui.panel"];
  const stages = ["before_policy", "policy", "after_policy"];
  if (!object(params) || !keys(params, ["apiVersion", "capabilities", "contributions"]) ||
      !Number.isInteger(params.apiVersion) || !Array.isArray(params.capabilities) ||
      !params.capabilities.every((v) => capabilities.includes(v)) || !object(params.contributions)) {
    throw new RpcError(-32602, "Invalid initialization");
  }
  if (params.apiVersion !== 1) throw new RpcError(-32002, "Incompatible API version");
  const c = params.contributions;
  if (!keys(c, ["tools", "commands", "interceptors"])) throw new RpcError(-32602, "Invalid contributions");
  const tools = c.tools === undefined ? [] : c.tools;
  const commands = c.commands === undefined ? [] : c.commands;
  const interceptors = c.interceptors === undefined ? [] : c.interceptors;
  if (!Array.isArray(tools) || !Array.isArray(commands) || !Array.isArray(interceptors) ||
      !tools.every((t) => object(t) && keys(t, ["name", "description", "inputSchema"]) && typeof t.name === "string" && typeof t.description === "string" && own(t, "inputSchema")) ||
      !commands.every((c) => object(c) && keys(c, ["name", "description"]) && typeof c.name === "string" && typeof c.description === "string") ||
      !interceptors.every((s) => stages.includes(s))) throw new RpcError(-32602, "Invalid contributions");
  return { apiVersion: 1, capabilities: params.capabilities, contributions: { tools, commands, interceptors } };
}

// Racing cancellation lets the transport settle promptly even if a handler ignores its signal.
function abortable(promise, signal) {
  return new Promise((resolve, reject) => {
    const abort = () => { signal.removeEventListener("abort", abort); reject(cancelled()); };
    signal.addEventListener("abort", abort, { once: true });
    if (signal.aborted) abort();
    Promise.resolve(promise).then(resolve, reject).finally(() => signal.removeEventListener("abort", abort));
  });
}

export function serve(plugin, { signal, input = process.stdin, output = process.stdout }) {
  return new Promise((resolve, reject) => {
    const active = new Map(), pending = new Map();
    let next = 1, declaration, initializing = false, closed = false;
    let buffer = Buffer.alloc(0), writing = Promise.resolve();
    const finish = (error) => {
      if (closed) return;
      closed = true;
      input.off("data", data); input.off("end", end); input.off("error", finish);
      output.off("error", finish); output.off("close", outputClosed);
      signal.removeEventListener("abort", stop);
      input.pause();
      for (const controller of active.values()) controller.abort();
      for (const request of pending.values()) request.reject(error ?? new RpcError(-32000, "Transport closed"));
      pending.clear(); active.clear();
      if (error) reject(error); else resolve();
    };
    const stop = () => finish();
    const outputClosed = () => finish(new RpcError(-32000, "Transport closed"));
    const send = (value) => {
      if (closed) return Promise.reject(new RpcError(-32000, "Transport closed"));
      const bytes = Buffer.from(JSON.stringify(value) + "\n");
      if (bytes.length > MAX_FRAME) return Promise.reject(new RpcError(-32600, "Frame exceeds 1 MiB"));
      writing = writing.then(() => new Promise((resolve, reject) => {
        output.write(bytes, (error) => error ? reject(error) : resolve());
      }));
      writing.catch(finish);
      return writing;
    };
    const request = (method, params, signal) => {
      if (signal.aborted) return Promise.reject(cancelled());
      if (closed) return Promise.reject(new RpcError(-32000, "Transport closed"));
      const id = next++;
      return new Promise((resolve, reject) => {
        const cleanup = () => { pending.delete(id); signal.removeEventListener("abort", abort); };
        const abort = () => {
          cleanup(); reject(cancelled());
          if (!closed) send({ jsonrpc: "2.0", method: "notifications/cancelled", params: { requestId: id } }).catch(finish);
        };
        pending.set(id, {
          resolve: (value) => { cleanup(); resolve(value); },
          reject: (error) => { cleanup(); reject(error); },
        });
        signal.addEventListener("abort", abort, { once: true });
        send({ jsonrpc: "2.0", id, method, params }).catch((error) => { cleanup(); reject(error); });
      });
    };
    const context = (signal, token) => ({
      signal,
      progress: async (progress, message) => {
        if (signal.aborted) throw cancelled();
        if (!Number.isFinite(progress) || typeof message !== "string") throw new RpcError(-32602, "Invalid progress");
        if (token !== undefined) await send({ jsonrpc: "2.0", method: "notifications/progress", params: { progressToken: token, progress, message } });
      },
      request: (method, params) => request(method, params, signal),
      notify: async (title, text) => { await request("ui.notify", { title, text }, signal); },
      select: (title, text, options) => request("ui.select", { title, text, options }, signal),
      confirm: (title, text) => request("ui.confirm", { title, text }, signal),
      input: (title, text) => request("ui.input", { title, text }, signal),
      panel: (panel) => request("panel.show", panel, signal),
      status: async (text) => { await request("status.set", { text }, signal); },
    });
    const dispatch = async (method, params, ctx) => {
      if (method === "initialize") {
        if (declaration || initializing) throw new RpcError(-32600, "Already initialized");
        initializing = true;
        try {
          const init = initialize(params);
          await abortable(plugin.initialize?.(structuredClone(init), ctx.signal), ctx.signal);
          declaration = init;
          return init;
        } finally { initializing = false; }
      }
      if (!declaration) throw new RpcError(-32001, "Not initialized");
      if (method === "shutdown") return null;
      const methods = { "tools/list": "tools", "tools/call": "tools", "commands/list": "commands", "commands/run": "commands", "tool.intercept": "tool.intercept" };
      const capability = own(methods, method) ? methods[method] : undefined;
      if (!capability) throw new RpcError(-32601, "Unknown host method");
      if (!declaration.capabilities.includes(capability)) throw new RpcError(-32001, "Capability not declared");
      if (!object(params)) throw new RpcError(-32602, "Expected object parameters");
      const c = declaration.contributions;
      if (method === "tools/list") return c.tools;
      if (method === "commands/list") return c.commands;
      let handler, args;
      if (method === "tools/call" || method === "commands/run") {
        const tool = method === "tools/call";
        if (!(tool ? c.tools : c.commands).some((v) => v.name === params.name) || !own(params, "arguments") ||
            (!tool && typeof params.arguments !== "string")) throw new RpcError(-32602, "Invalid call");
        handler = tool ? plugin.tool : plugin.command;
        args = [params.name, params.arguments, ctx];
      } else {
        if (!c.interceptors.includes(params.stage) || !object(params.call) || typeof params.call.id !== "string" ||
            typeof params.call.name !== "string" || !own(params.call, "arguments") ||
            !["before", "after"].includes(params.phase) || (params.phase === "after" && !own(params, "output"))) {
          throw new RpcError(-32602, "Invalid interception");
        }
        handler = params.phase === "before" ? plugin.before : plugin.after;
        args = [params.stage, params.call, ...(params.phase === "after" ? [params.output] : []), ctx];
      }
      if (!handler) throw new RpcError(-32601, "No handler");
      return handler(...args);
    };
    const receive = (message) => {
      if (!own(message, "method")) {
        const waiter = pending.get(message.id);
        if (own(message, "result")) waiter?.resolve(message.result);
        else waiter?.reject(new RpcError(message.error.code, message.error.message, message.error.data));
        return;
      }
      if (!own(message, "id")) {
        if (message.method === "notifications/cancelled") {
          if (!idValid(message.params?.requestId)) throw new RpcError(-32602, "Invalid cancellation ID");
          active.get(message.params.requestId)?.abort();
        }
        return;
      }
      if (active.has(message.id)) throw new RpcError(-32600, "Duplicate active request ID");
      const controller = new AbortController();
      active.set(message.id, controller);
      const params = message.params ?? {};
      const token = params?._meta?.progressToken;
      if (object(params)) delete params._meta;
      const ctx = context(controller.signal, idValid(token) ? token : undefined);
      abortable(dispatch(message.method, params, ctx), controller.signal)
        .then((result) => {
          if (result === undefined) throw new RpcError(-32000, "Handler returned undefined");
          return { jsonrpc: "2.0", id: message.id, result };
        })
        .catch((error) => ({ jsonrpc: "2.0", id: message.id, error: { code: error instanceof RpcError ? error.code : -32000, message: String(error.message ?? error), ...(error.data !== undefined ? { data: error.data } : {}) } }))
        .then(async (response) => {
          if (closed) return;
          await send(response);
          if (message.method === "shutdown" && own(response, "result")) finish();
        })
        .catch(finish)
        .finally(() => { active.delete(message.id); controller.abort(); });
    };
    const decoder = new TextDecoder("utf-8", { fatal: true });
    const data = (chunk) => {
      try {
        // Check each slice before copying so oversized partial frames stay bounded.
        let start = 0;
        while (start < chunk.length && !closed) {
          const newline = chunk.indexOf(10, start);
          const end = newline < 0 ? chunk.length : newline + 1;
          if (buffer.length + end - start > MAX_FRAME) throw new RpcError(-32600, "Frame exceeds 1 MiB");
          buffer = Buffer.concat([buffer, chunk.subarray(start, end)]);
          start = end;
          if (newline >= 0) {
            receive(envelope(JSON.parse(decoder.decode(buffer))));
            buffer = Buffer.alloc(0);
          }
        }
      } catch (error) { finish(error); }
    };
    const end = () => finish(buffer.length ? new RpcError(-32600, "Truncated frame") : undefined);
    input.on("data", data); input.on("end", end); input.on("error", finish);
    output.on("error", finish); output.on("close", outputClosed);
    signal.addEventListener("abort", stop, { once: true });
    if (signal.aborted) finish();
  });
}
