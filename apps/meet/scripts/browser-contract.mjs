(async () => {
  const errors = [];
  const onError = (event) =>
    errors.push(String(event.error?.stack ?? event.message ?? event.reason));
  globalThis.addEventListener("error", onError);
  globalThis.addEventListener("unhandledrejection", onError);
  const originalConsoleError = console.error;
  console.error = (...args) => {
    errors.push(args.map((value) => String(value?.stack ?? value)).join(" "));
    originalConsoleError.apply(console, args);
  };
  const waitFor = async (predicate, label) => {
    const deadline = performance.now() + 20000;
    while (!predicate()) {
      if (performance.now() > deadline)
        throw new Error(
          `Meet timed out: ${label}; ${document.body.innerText}; ${errors.join("\n")}`,
        );
      await new Promise((resolve) => setTimeout(resolve, 25));
    }
  };
  const check = (value, label) => {
    if (!value) throw new Error(`Meet contract: ${label}`);
  };
  const button = (label) =>
    [...document.querySelectorAll("button")].find(
      (element) =>
        element.getAttribute("aria-label") === label ||
        element.textContent.trim() === label,
    );
  const fill = (input, value) => {
    Object.getOwnPropertyDescriptor(
      HTMLInputElement.prototype,
      "value",
    ).set.call(input, value);
    input.dispatchEvent(new Event("input", { bubbles: true }));
  };
  const peers = [];
  const OriginalPeer = globalThis.RTCPeerConnection;
  globalThis.RTCPeerConnection = new Proxy(OriginalPeer, {
    construct(target, args) {
      const peer = new target(...args);
      peers.push(peer);
      return peer;
    },
  });
  try {
    await waitFor(
      () =>
        button("Join room") &&
        !button("Join room").disabled &&
        document.querySelector("video")?.videoWidth > 0,
      "captured lobby preview",
    );
    const preview = document.querySelector("video");
    const capturedCamera = preview.srcObject.getVideoTracks()[0];
    fill(document.querySelector('input[type="password"]'), "__TOKEN__");
    fill(
      document.querySelector('input[inputmode="url"]'),
      "http://127.0.0.1:7070",
    );
    await new Promise((resolve) => setTimeout(resolve, 0));
    button("Join room").click();
    await waitFor(
      () =>
        document
          .querySelector(".meet-room-name")
          ?.textContent.includes(", connected") &&
        peers.some((peer) => peer.connectionState === "connected") &&
        document.querySelector(".meet-spotlight video")?.videoWidth > 0,
      "connected room with local video",
    );
    const peer = peers.find(
      (candidate) => candidate.connectionState === "connected",
    );
    const sending = (kind) =>
      peer.getSenders().find((sender) => sender.track?.kind === kind)?.track;
    await waitFor(
      () => sending("audio") && sending("video"),
      "published capture sources",
    );
    const capturedAudio = sending("audio");
    check(sending("video") === capturedCamera, "room borrows lobby camera");
    check(!document.querySelector('[role="alert"]'), "no room failure");

    button("End").click();
    await waitFor(
      () =>
        button("Join room") &&
        !button("Join room").disabled &&
        peers.every((connection) => connection.connectionState === "closed") &&
        capturedCamera.readyState === "ended" &&
        capturedAudio.readyState === "ended" &&
        document.querySelector("video")?.videoWidth > 0,
      "closed room, released old capture, and fresh lobby preview",
    );
    check(
      document.querySelector("video").srcObject.getVideoTracks()[0] !==
        capturedCamera,
      "lobby reacquires its own camera",
    );
    check(!document.querySelector('[role="alert"]'), "no lobby failure");
    return true;
  } finally {
    globalThis.RTCPeerConnection = OriginalPeer;
    globalThis.removeEventListener("error", onError);
    globalThis.removeEventListener("unhandledrejection", onError);
    console.error = originalConsoleError;
    for (const peer of peers) peer.close();
  }
})();
