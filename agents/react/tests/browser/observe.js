// Navigation awaits module loading; observe completion rather than elapsed time.
globalThis.__pulsebeamReactObservation ??
  Promise.reject(
    new Error(
      "React fixture did not start; build //agents/react:browser_fixture through Bazel",
    ),
  );
