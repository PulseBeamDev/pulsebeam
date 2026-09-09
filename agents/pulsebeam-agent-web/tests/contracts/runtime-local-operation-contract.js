(async () => {
  const { BrowserRuntime, default: initialize } = await import(
    "/dist/wasm/pulsebeam_agent_web.js"
  );
  await initialize();

  const runtime = new BrowserRuntime({
    endpoint: location.origin,
    roomId: "runtime-local-operation-contract",
    topology: { localVideo: ["camera"] },
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
          () => reject(new Error(`timed out waiting for ${label}`)),
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
  RTCRtpSender.prototype.setParameters = () => {
    parameterCalls += 1;
    return new Promise((resolve) => releases.push(resolve));
  };

  try {
    const first = runtime.replace_local_track("camera", firstTrack, {
      contentHint: "motion",
    });
    await waitFor(() => parameterCalls === 1, "first sender operation");
    const second = runtime.replace_local_track("camera", secondTrack, {
      contentHint: "motion",
    });
    await new Promise((resolve) => setTimeout(resolve, 20));
    const serializedBeforeRelease = parameterCalls === 1;

    releases[0]();
    await waitFor(() => parameterCalls === 2, "second sender operation");
    releases[1]();
    await withTimeout(Promise.all([first, second]), "serialized operations");
    const statistics = await runtime.statistics();
    const finalTrackWins = statistics.senders[0]?.trackId === secondTrack.id;

    const closing = runtime.replace_local_track("camera", thirdTrack, {
      contentHint: "motion",
    });
    await waitFor(() => parameterCalls === 3, "closing sender operation");
    runtime.close();
    releases[2]();
    const closeFenced = await withTimeout(
      closing.then(
        () => false,
        (error) =>
          error instanceof Error &&
          error.message === "browser runtime is closed",
      ),
      "closing operation",
    );
    const postCloseFenced = await runtime.set_local_muted("camera", true).then(
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
