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
  const originalFetch = window.fetch;
  window.fetch = (request, init) => {
    if (
      request instanceof Request &&
      request.method === "POST" &&
      new URL(request.url).origin === location.origin
    ) {
      return new Promise((_, reject) => {
        const abort = () => reject(new DOMException("Aborted", "AbortError"));
        if (request.signal.aborted) abort();
        else request.signal.addEventListener("abort", abort, { once: true });
      });
    }
    return originalFetch.call(window, request, init);
  };
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
  const originalReplaceTrack = RTCRtpSender.prototype.replaceTrack;
  const originalClone = MediaStreamTrack.prototype.clone;
  const ownedTracks = [];
  const cloneSources = new WeakMap();
  MediaStreamTrack.prototype.clone = function () {
    const clone = originalClone.call(this);
    ownedTracks.push(clone);
    cloneSources.set(clone, this);
    return clone;
  };
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
    const senderTrack = ownedTracks.at(-1);
    const finalTrackWins =
      cloneSources.get(senderTrack) === secondTrack &&
      senderTrack.id !== secondTrack.id;
    await runtime.set_local_muted("v0", true);
    const disabledRetainsSender =
      senderTrack.readyState === "live" &&
      !senderTrack.enabled &&
      secondTrack.enabled;
    await runtime.replace_local_track("v0", null, { contentHint: "motion" });
    await runtime.replace_local_track("v0", secondTrack, {
      contentHint: "motion",
    });
    const stickyDisabled = !ownedTracks.at(-1).enabled;
    await runtime.set_local_muted("v0", false);
    const enabledAgain = ownedTracks.at(-1).enabled;

    let pendingTrack;
    let previousTrack;
    let releaseReplacement;
    RTCRtpSender.prototype.replaceTrack = function (track) {
      previousTrack = this.track;
      pendingTrack = track;
      return new Promise((resolve, reject) => {
        releaseReplacement = () =>
          originalReplaceTrack.call(this, track).then(resolve, reject);
      });
    };
    const replacing = runtime.replace_local_track("v0", thirdTrack, {
      contentHint: "motion",
    });
    await waitFor(() => releaseReplacement, "blocked replacement");
    const previousStoppedBeforeReplacement =
      previousTrack.readyState === "ended";
    await runtime.set_local_muted("v0", true);
    const pendingDisabled = !pendingTrack.enabled && thirdTrack.enabled;
    const clonesBeforeSuperseded = ownedTracks.length;
    const superseded = runtime.replace_local_track("v0", firstTrack, {
      contentHint: "motion",
    });
    const detaching = runtime.replace_local_track("v0", null, {
      contentHint: "motion",
    });
    await waitFor(
      () => pendingTrack.readyState === "ended",
      "immediate detach",
    );
    const detachStopsOwnedOnly = thirdTrack.readyState === "live";
    RTCRtpSender.prototype.replaceTrack = originalReplaceTrack;
    releaseReplacement();
    await withTimeout(
      Promise.all([replacing, superseded, detaching]),
      "replacement and detach",
    );
    const supersededSkipped = ownedTracks.length === clonesBeforeSuperseded;
    await runtime.replace_local_track("v0", secondTrack, {
      contentHint: "motion",
    });

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
    const closeStopsOwnedImmediately =
      ownedTracks.length > 0 &&
      ownedTracks.every((track) => track.readyState === "ended");
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
      finalTrackWins:
        finalTrackWins &&
        disabledRetainsSender &&
        stickyDisabled &&
        enabledAgain &&
        previousStoppedBeforeReplacement &&
        pendingDisabled &&
        detachStopsOwnedOnly &&
        supersededSkipped,
      closeFenced: closeFenced && closeStopsOwnedImmediately,
      postCloseFenced,
    };
  } finally {
    RTCRtpSender.prototype.setParameters = originalSetParameters;
    RTCRtpSender.prototype.replaceTrack = originalReplaceTrack;
    MediaStreamTrack.prototype.clone = originalClone;
    window.fetch = originalFetch;
    runtime.abort();
    firstTrack.stop();
    secondTrack.stop();
    thirdTrack.stop();
  }
})();
