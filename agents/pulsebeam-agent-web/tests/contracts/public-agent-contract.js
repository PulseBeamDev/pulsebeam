(async () => {
  const exports = Object.keys(window.pulsebeam).sort();
  const first = window.pulsebeam.createAgent();
  const second = window.pulsebeam.createAgent();
  const initial = first.getSnapshot();
  const initialStable = initial === first.getSnapshot();
  const initialFrozen =
    Object.isFrozen(initial) && Object.isFrozen(initial.tracks);
  second.setState({ connection: { roomId: "closed", token: "closed" } });
  second.close();
  const calls = [];
  let removeFirst;
  removeFirst = first.subscribe(() => {
    calls.push("first");
    removeFirst();
    first.subscribe(() => calls.push("late"));
  });
  const removeSecond = first.subscribe(() => calls.push("second"));
  let roomReads = 0;
  let tokenReads = 0;
  const connection = {
    get roomId() {
      roomReads += 1;
      return "first";
    },
    get token() {
      tokenReads += 1;
      return "first";
    },
  };
  const publish = [{ kind: "audio", label: "microphone", media: null }];
  const subscribe = [
    { participantId: "ignored", kind: "video", label: "camera" },
  ];
  first.setState({
    connection,
    publish,
    subscribe,
  });
  publish.push({ kind: "audio", label: "mutated", media: null });
  subscribe.push({ participantId: "mutated", kind: "video", label: "mutated" });
  const connecting = first.getSnapshot();
  const connectingStable = connecting === first.getSnapshot();
  first.setState({ connection: null });
  await new Promise((resolve) => setTimeout(resolve, 20));
  const nullCancelled = first.getSnapshot().connection === "disconnected";
  first.setState({ connection: { roomId: "last", token: "last" } });
  await new Promise((resolve, reject) => {
    const timeout = setTimeout(
      () => reject(new Error("initialization did not settle")),
      5000,
    );
    const remove = first.subscribe(() => {
      if (first.getSnapshot().connection !== "failed") return;
      clearTimeout(timeout);
      remove();
      resolve();
    });
  });
  const failed = first.getSnapshot();
  const defensiveCopies = roomReads === 1 && tokenReads === 1;
  removeSecond();
  removeSecond();
  first.close();
  const closed = first.getSnapshot();
  first.close();
  first.setState({ connection: { roomId: "ignored", token: "ignored" } });
  return {
    exports,
    independent: first !== second,
    initialStable,
    initialFrozen,
    initial: initial.connection,
    connecting: connecting.connection,
    connectingStable,
    nullCancelled,
    defensiveCopies,
    closeBeforeSettlement: second.getSnapshot().connection === "disconnected",
    failed: failed.connection,
    calls,
    closed: closed.connection,
    postClose: first.getSnapshot() === closed,
  };
})()
