import { Track } from "./platform.mjs";

export async function localOperationContract(BrowserRuntime) {
  const runtime = new BrowserRuntime({
    endpoint: "http://localhost",
    token: "runtime-local-operation-contract",
    topology: { localVideos: 1 },
  });
  runtime.connect();

  const waitFor = async (condition, label) => {
    for (let turn = 0; turn < 1000; turn++) {
      if (condition()) return;
      await Promise.resolve();
    }
    throw new Error(`pending completion: ${label}`);
  };
  await waitFor(() => runtime.diagnostics().peers === 1, "runtime peer");

  const firstTrack = new Track("video");
  const secondTrack = new Track("video");
  const thirdTrack = new Track("video");
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
    await Promise.resolve();
    await Promise.resolve();
    const serializedBeforeRelease = parameterCalls === 1;

    releases[0]();
    await waitFor(() => parameterCalls === 2, "second sender operation");
    holdParameters = false;
    releases[1]();
    await Promise.all([first, second]);
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
    await Promise.all([replacing, superseded, detaching]);
    const supersededSkipped = ownedTracks.length === clonesBeforeSuperseded;
    await runtime.replace_local_track("v0", secondTrack, {
      contentHint: "motion",
    });

    await runtime.replace_local_track("v0", null, { contentHint: "motion" });
    const publication = await runtime.resource({
      op: "claimPublication",
      spec: { kind: "video", label: "camera" },
    });
    releaseReplacement = undefined;
    RTCRtpSender.prototype.replaceTrack = function (track) {
      pendingTrack = track;
      return new Promise((resolve, reject) => {
        releaseReplacement = () =>
          originalReplaceTrack.call(this, track).then(resolve, reject);
      });
    };
    const ownedClonesBefore = ownedTracks.length;
    const ownedReplacing = runtime
      .replace_owned_track(publication.owner, firstTrack, {
        contentHint: "motion",
      })
      .then(
        () => false,
        (error) => error.pulsebeamCode === "released",
      );
    await waitFor(() => releaseReplacement, "owned blocked replacement");
    await runtime.set_owned_muted(publication.owner, true);
    const mutedOwnedClone = !pendingTrack.enabled && firstTrack.enabled;
    const staleReplacement = runtime
      .replace_owned_track(publication.owner, thirdTrack, {
        contentHint: "motion",
      })
      .then(
        () => false,
        (error) => error.pulsebeamCode === "released",
      );
    await runtime.resource({ op: "release", owner: publication.owner });
    const releasedImmediately =
      pendingTrack.readyState === "ended" && firstTrack.readyState === "live";
    const reused = await runtime.resource({
      op: "claimPublication",
      spec: { kind: "video", label: "screen" },
    });
    const reusedReplacing = runtime.replace_owned_track(
      reused.owner,
      secondTrack,
      { contentHint: "text" },
    );
    RTCRtpSender.prototype.replaceTrack = originalReplaceTrack;
    releaseReplacement();
    const [oldFenced, queuedFenced] = await Promise.all([
      ownedReplacing,
      staleReplacement,
      reusedReplacing,
    ]);
    const reusedClone = ownedTracks.at(-1);
    const ownerReuseFenced =
      oldFenced &&
      queuedFenced &&
      mutedOwnedClone &&
      releasedImmediately &&
      reused.slot === publication.slot &&
      reused.owner !== publication.owner &&
      ownedTracks.length === ownedClonesBefore + 2 &&
      cloneSources.get(reusedClone) === secondTrack &&
      reusedClone.enabled;
    await runtime.resource({ op: "release", owner: reused.owner });

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
    const closeFenced = await closing.then(
      () => false,
      (error) =>
        error instanceof Error && error.message === "browser runtime is closed",
    );
    const postCloseFenced = await runtime.set_local_muted("v0", true).then(
      () => false,
      (error) =>
        error instanceof Error && error.message === "browser runtime is closed",
    );

    return {
      serializedBeforeRelease,
      ownerReuseFenced: ownerReuseFenced && reusedClone.readyState === "ended",
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
    runtime.abort();
    firstTrack.stop();
    secondTrack.stop();
    thirdTrack.stop();
  }
}
