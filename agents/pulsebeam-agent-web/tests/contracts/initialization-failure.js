new Promise((resolve, reject) => {
  const agent = window.pulsebeam.createAgent({
    endpoint: location.origin,
    token: "token",
    topology: {},
  });
  const timeout = setTimeout(
    () => reject(new Error("initialization did not fail")),
    5000,
  );
  agent.subscribe(() => {
    if (agent.connection !== "terminal-failure") return;
    clearTimeout(timeout);
    const failure = agent.getSnapshot().failure;
    const error = agent.error;
    agent.setState({ connected: false });
    const after = agent.getSnapshot();
    resolve(
      after.connection === "terminal-failure" &&
      agent.connection === after.connection &&
      after.failure === failure &&
      error === failure,
    );
  });
  agent.setState({ connected: true });
});
