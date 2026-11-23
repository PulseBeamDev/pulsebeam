(async () => {
  const early = "__GESTURE_ORDER__" === "before-media";
  const { Agent } = pulsebeam;
  const { attachMedia } = await import("/sdk/dist/internal/react.js");
  const agent = new Agent({
    endpoint: "http://localhost",
    token: "audio-gesture",
  });
  const subscription = agent
    .room("room")
    .participant("sender")
    .audio("microphone")
    .subscribe();
  const context = new AudioContext();
  const destination = context.createMediaStreamDestination();
  const oscillator = context.createOscillator();
  oscillator.connect(destination);
  oscillator.start();
  const capture = destination.stream.getAudioTracks()[0];
  const audio = document.createElement("audio");
  document.body.append(audio);
  let complete;
  const acquisition = new Promise((resolve) => {
    complete = resolve;
  });
  let requested = false;
  subscription.playbackOutput = () => {
    requested = true;
    return acquisition;
  };
  let owned;
  const releaseOutput = () => {
    owned = capture.clone();
    complete({
      track: () => (owned.readyState === "live" ? owned : undefined),
      close: async () => owned.stop(),
    });
  };
  let blocked;
  let retry;
  let failure;
  let clicked;
  const attachment = attachMedia(
    subscription,
    "audio",
    audio,
    true,
    (error, resume) => {
      blocked = error;
      retry = resume;
    },
    (error) => {
      failure = error;
    },
  );
  const wait = async (predicate, label) => {
    const end = performance.now() + 4000;
    while (!predicate()) {
      if (failure) throw failure;
      if (performance.now() > end) throw new Error(label);
      await new Promise((resolve) => setTimeout(resolve, 10));
    }
  };
  await wait(() => requested, "audio output was not requested");
  if (navigator.userActivation.hasBeenActive)
    throw new Error("autoplay fixture started activated");
  if (early) {
    if (audio.srcObject.getTracks().length || blocked)
      throw new Error(
        "early gesture must precede output and autoplay attempts",
      );
  } else {
    releaseOutput();
    await wait(
      () => blocked,
      "real audio element did not report blocked autoplay",
    );
    if (blocked.name !== "NotAllowedError" || !audio.paused)
      throw new Error(`unexpected autoplay result: ${blocked}`);
  }
  const button = document.createElement("button");
  button.id = "enable-audio";
  button.textContent = "Enable audio";
  button.onclick = () => {
    clicked = Promise.all([
      context.resume(),
      retry ? retry() : attachment.retryPlayback(),
    ]);
  };
  document.body.append(button);
  globalThis.__finishAudioGesture = async () => {
    try {
      if (!clicked || !navigator.userActivation.hasBeenActive)
        throw new Error("browser did not activate the document");
      await clicked;
      if (early) releaseOutput();
      await wait(
        () => !audio.paused && audio.srcObject.getAudioTracks()[0] === owned,
        "audio did not play after the real user gesture",
      );
      await attachment.close();
      if (
        audio.srcObject !== null ||
        owned.readyState !== "ended" ||
        capture.readyState !== "live" ||
        subscription.state().closed
      ) {
        throw new Error("closing the view must retire only its owned output");
      }
      return true;
    } finally {
      await attachment.close();
      await subscription.close();
      await agent.close();
      capture.stop();
      await context.close();
      audio.remove();
      button.remove();
      delete globalThis.__finishAudioGesture;
    }
  };
  return true;
})();
