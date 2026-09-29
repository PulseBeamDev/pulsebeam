(async () => {
  const { BrowserRuntime, default: initialize } = await import(
    "/dist/wasm/pulsebeam_agent_web.js"
  );
  await initialize();

  const runtime = new BrowserRuntime({
    endpoint: location.origin,
    token: "runtime-local-operation-contract",
    topology: { localVideos: 1 },
  });
  runtime.connect();

  const waitFor = async (condition, label) => {
    const deadline = Date.now() + 2_000;
    while (!condition()) {
      if (Date.now() >= deadline)
        throw new Error(`timed out waiting for ${label}`);
      await new Promise((resolve) => setTimeout(resolve, 10));
    }
  };
  const withTimeout = (promise, label) =>
    Promise.race([
      promise,
      new Promise((_, reject) =>
        setTimeout(
          () =>
            reject(
              new Error(
                `timed out waiting for ${typeof label === "function" ? label() : label}`,
              ),
            ),
          2_000,
        ),
      ),
    ]);
  await waitFor(() => runtime.diagnostics().peers === 1, "runtime peer");

  const canvas = document.createElement("canvas");
  const firstTrack = canvas.captureStream(1).getVideoTracks()[0];
  const secondTrack = canvas.captureStream(1).getVideoTracks()[0];
  const thirdTrack = canvas.captureStream(1).getVideoTracks()[0];
  const originalSetParameters = RTCRtpSender.prototype.setParameters;
  const releases = [];
  let parameterCalls = 0;
  let holdParameters = true;
  RTCRtpSender.prototype.setParameters = () => {
    parameterCalls += 1;
    return holdParameters
      ? new Promise((resolve) => releases.push(resolve))
      : Promise.resolve();
  };

  try {
    let firstSettled = false;
    let secondSettled = false;
    const first = runtime
      .replace_local_track("v0", firstTrack, { contentHint: "motion" })
      .finally(() => {
        firstSettled = true;
      });
    await waitFor(() => parameterCalls === 1, "first sender operation");
    const second = runtime
      .replace_local_track("v0", secondTrack, { contentHint: "motion" })
      .finally(() => {
        secondSettled = true;
      });
    await new Promise((resolve) => setTimeout(resolve, 20));
    const serializedBeforeRelease = parameterCalls === 1;

    releases[0]();
    await waitFor(() => parameterCalls === 2, "second sender operation");
    holdParameters = false;
    releases[1]();
    await withTimeout(
      Promise.all([first, second]),
      () =>
        `serialized operations: calls=${parameterCalls}, releases=${releases.length}, first=${firstSettled}, second=${secondSettled}`,
    );
    const statistics = await runtime.statistics();
    const finalTrackWins = statistics.senders[0]?.trackId === secondTrack.id;

    holdParameters = true;
    const callsBeforeClosing = parameterCalls;
    const closing = runtime.replace_local_track("v0", thirdTrack, {
      contentHint: "motion",
    });
    await waitFor(
      () => parameterCalls > callsBeforeClosing,
      "closing sender operation",
    );
    runtime.close();
    releases.at(-1)();
    const closeFenced = await withTimeout(
      closing.then(
        () => false,
        (error) =>
          error instanceof Error &&
          error.message === "browser runtime is closed",
      ),
      "closing operation",
    );
    const postCloseFenced = await runtime.set_local_muted("v0", true).then(
      () => false,
      (error) =>
        error instanceof Error && error.message === "browser runtime is closed",
    );

    return {
      serializedBeforeRelease,
      finalTrackWins,
      closeFenced,
      postCloseFenced,
    };
  } finally {
    RTCRtpSender.prototype.setParameters = originalSetParameters;
    runtime.abort();
    firstTrack.stop();
    secondTrack.stop();
    thirdTrack.stop();
  }
})();
