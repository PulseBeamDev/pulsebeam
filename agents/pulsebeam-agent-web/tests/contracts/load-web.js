(async () => {
  globalThis.__pulsebeamUnhandledRejections = [];
  addEventListener("unhandledrejection", (event) =>
    globalThis.__pulsebeamUnhandledRejections.push(String(event.reason)),
  );
  globalThis.__pulsebeamHttpFailures = [];
  const originalFetch = globalThis.fetch;
  globalThis.fetch = async function (...args) {
    const response = await originalFetch.apply(this, args);
    if (!response.ok) {
      globalThis.__pulsebeamHttpFailures.push({
        status: response.status,
        body: await response.clone().text(),
      });
    }
    return response;
  };
  globalThis.pulsebeam = await import("/dist/index.js");
  return null;
})();
