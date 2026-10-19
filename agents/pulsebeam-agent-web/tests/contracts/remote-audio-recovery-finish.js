(async () => {
  const state = window.__audioRecovery;
  const wait = async (condition, label) => {
    for (let i = 0; i < 500; i++) {
      if (condition()) return;
      await new Promise((resolve) => setTimeout(resolve, 10));
    }
    throw new Error(label);
  };
  try {
    await wait(
      () => state.ready || state.error,
      "late trusted gesture completion",
    );
    if (state.error || state.contexts[0].state !== "running")
      throw new Error("trusted gesture did not recover the SDK audio context");
    const frames = state.video.getVideoPlaybackQuality().totalVideoFrames;
    await wait(
      () =>
        !state.video.paused &&
        state.video.videoWidth > 0 &&
        state.video.getVideoPlaybackQuality().totalVideoFrames > frames,
      "trusted retry did not restore fresh native attachment frames",
    );
    if (state.video.srcObject.getVideoTracks()[0] !== state.videoTrack)
      throw new Error("recovery replaced the selected native track");
    return true;
  } finally {
    state.attachment.close();
    state.catalog.close();
    clearInterval(state.painting);
    state.track.stop();
    state.videoTrack.stop();
    state.video.remove();
    await state.source.close();
    window.AudioContext = state.RealContext;
  }
})();
