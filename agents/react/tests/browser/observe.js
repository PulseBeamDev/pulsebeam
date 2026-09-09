new Promise((resolve, reject) => {
  const deadline = Date.now() + 5000;
  const poll = () =>
    globalThis.__pulsebeamReactObservation
      ? resolve(globalThis.__pulsebeamReactObservation)
      : Date.now() >= deadline
        ? reject(
            new Error(
              "React fixture did not publish observations; run `just browser` to rebuild it",
            ),
          )
        : setTimeout(poll, 10);
  poll();
});
