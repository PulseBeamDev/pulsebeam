new Promise((resolve) =>
  setTimeout(
    () => resolve(globalThis.__pulsebeamUnhandledRejections.length),
    20,
  ),
);
