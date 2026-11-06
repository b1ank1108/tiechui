(send, log) => {
  const pending = new Map();
  return {
    tools: new Proxy(Object.create(null), {
      get(_, name) {
        if (typeof name !== "string" || name === "then") return undefined;
        return (args = {}) => new Promise((resolve, reject) => {
          let serialized, error = "";
          try {
            serialized = JSON.stringify(args);
            if (serialized === undefined) throw new Error("Tool arguments must be JSON");
          } catch (e) { error = String(e); }
          const index = send(name, serialized ?? "null", error);
          pending.set(index, { resolve, reject, error: new Error() });
        });
      }
    }),
    console: { log: (...args) => log(args.map(value =>
      typeof value === "string" ? value : JSON.stringify(value) ?? String(value)
    ).join(" ")) },
    deliver(index, name, text, failed) {
      const entry = pending.get(index);
      if (!entry) return;
      pending.delete(index);
      if (failed) {
        entry.error.message = text;
        entry.error.tool = name;
        entry.reject(entry.error);
      } else entry.resolve(text);
    },
    serialize: value => value === undefined ? undefined :
      typeof value === "string" ? value : JSON.stringify(value, null, 2)
  };
}
