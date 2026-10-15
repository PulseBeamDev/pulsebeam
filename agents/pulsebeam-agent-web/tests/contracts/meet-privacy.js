(() => {
  const effects = [];
  const snapshot = (value) => {
    const seen = new WeakSet();
    return (
      JSON.stringify(value, (_, item) => {
        if (item && typeof item === "object") {
          if (seen.has(item)) return "[Circular]";
          seen.add(item);
        }
        return typeof item === "bigint" ? String(item) : item;
      }) ?? String(value)
    );
  };
  const record = (kind, values) =>
    effects.push({ kind, text: snapshot(values) });
  for (const name of ["localStorage", "sessionStorage"]) {
    const storage = window[name];
    const proxy = new Proxy(storage, {
      get(target, key) {
        const value = Reflect.get(target, key, target);
        if (typeof value !== "function") return value;
        return (...args) => {
          if (key === "setItem") record(name, args);
          return Reflect.apply(value, target, args);
        };
      },
      set(target, key, value) {
        record(name, [key, value]);
        return Reflect.set(target, key, value, target);
      },
    });
    Object.defineProperty(window, name, { configurable: true, value: proxy });
  }
  for (const method of ["add", "put"]) {
    const original = IDBObjectStore.prototype[method];
    IDBObjectStore.prototype[method] = function (...args) {
      record("indexedDB", args);
      return Reflect.apply(original, this, args);
    };
  }
  const cookie = Object.getOwnPropertyDescriptor(Document.prototype, "cookie");
  Object.defineProperty(Document.prototype, "cookie", {
    ...cookie,
    set(value) {
      record("cookie", value);
      cookie.set.call(this, value);
    },
  });
  for (const method of ["pushState", "replaceState"]) {
    const original = history[method];
    history[method] = function (...args) {
      record("history", args);
      return Reflect.apply(original, this, args);
    };
  }
  const fetch = window.fetch;
  window.fetch = function (input, ...args) {
    record("request URL", input instanceof Request ? input.url : String(input));
    return Reflect.apply(fetch, this, [input, ...args]);
  };
  for (const method of ["debug", "info", "log", "warn", "error", "trace"]) {
    const original = console[method];
    console[method] = function (...args) {
      record("console", args);
      if (
        args.some(
          (value) =>
            value &&
            typeof value === "object" &&
            ("token" in value || ("endpoint" in value && "topology" in value)),
        )
      )
        record("configuration log", "configuration object");
      return Reflect.apply(original, this, args);
    };
  }
  const leaks = (tokens) =>
    effects.filter(({ text }) =>
      tokens.some(
        (token) =>
          text.includes(token) || text.includes(encodeURIComponent(token)),
      ),
    );
  // A write removed before inspection must still be rejected by the oracle.
  const sentinel = crypto.randomUUID();
  const key = crypto.randomUUID();
  localStorage.setItem(key, sentinel);
  localStorage.removeItem(key);
  if (!leaks([sentinel]).some(({ kind }) => kind === "localStorage"))
    throw new Error("privacy oracle missed a transient storage write");
  effects.length = 0;
  window.__meetPrivacy = {
    check(tokens, phase) {
      record("current URL", location.href);
      record("current cookies", document.cookie);
      for (const storage of [localStorage, sessionStorage])
        for (let index = 0; index < storage.length; index++) {
          const key = storage.key(index);
          record("current storage", [key, storage.getItem(key)]);
        }
      const violations = leaks(tokens);
      if (
        violations.length ||
        effects.some(({ kind }) => kind === "configuration log")
      )
        throw new Error(
          `Meet credential exposure at ${phase}: ${violations.map(({ kind }) => kind).join(", ")}`,
        );
      return true;
    },
    lobby() {
      const inputs = [...document.querySelectorAll("form input")];
      if (
        inputs.length !== 2 ||
        inputs[0].type !== "password" ||
        inputs[0].autocomplete !== "off" ||
        inputs[1].inputMode !== "url"
      )
        throw new Error(
          `Meet must accept only an opaque password token and server endpoint: ${JSON.stringify(inputs.map(({ type, autocomplete, inputMode }) => ({ type, autocomplete, inputMode })))}`,
        );
      return true;
    },
    async identity() {
      await __meet.wait(() => {
        const text = document.querySelector("header")?.innerText;
        return (
          text?.includes("meet-receiver") &&
          text.includes("Room meet-media-continuity")
        );
      }, "authoritative external participant and room identity");
      return true;
    },
  };
  return true;
})();
