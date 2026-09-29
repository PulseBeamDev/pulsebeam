(async () => {
  const { TopicRegistry } = await import("/dist/topic.js");
  const listeners = new Set();
  const agent = {
    subscribeEvents(listener) {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
  };
  const registrations = [];
  const sends = [];
  const registry = new TopicRegistry(
    agent,
    async (name, mode, bytes) => sends.push({ name, mode, bytes }),
    (value) => registrations.push(value),
  );
  const reliable = registry.topic("chat", { mode: "reliable" });
  const stable = reliable === registry.topic("chat", { mode: "reliable" });
  const inert = registrations.length === 0;
  const abort = new AbortController();
  const first = reliable
    .subscribe({ signal: abort.signal })
    [Symbol.asyncIterator]();
  const second = reliable.subscribe()[Symbol.asyncIterator]();
  const interest = registrations.at(-1)?.[0];
  const subscribed =
    interest?.mode === "ordered" &&
    interest.subscribe &&
    !interest.publish &&
    listeners.size === 2;
  const message = { value: 42 };
  const delivery = second.next();
  for (const listener of listeners)
    listener({
      type: "topic-message",
      topic: "chat",
      mode: "ordered",
      payload: new TextEncoder().encode(JSON.stringify(message)),
    });
  const received =
    JSON.stringify((await delivery).value) === JSON.stringify(message);
  await reliable.publish({ value: 7 });
  const published =
    registrations.at(-1)?.[0]?.publish &&
    sends[0]?.name === "chat" &&
    sends[0]?.mode === "ordered" &&
    new TextDecoder().decode(sends[0]?.bytes) === '{"value":7}';
  await first.next();
  const cancelled = first.next();
  abort.abort();
  const aborted =
    (await cancelled).done &&
    listeners.size === 1 &&
    registrations.at(-1)?.[0]?.subscribe;
  await second.return();
  const released =
    listeners.size === 0 &&
    registrations.at(-1)?.[0]?.publish &&
    !registrations.at(-1)?.[0]?.subscribe;

  const latest = registry.topic("presence", { mode: "unreliable" });
  const stream = latest.subscribe()[Symbol.asyncIterator]();
  for (const value of [1, 2, 3])
    for (const listener of listeners)
      listener({
        type: "topic-message",
        topic: "presence",
        mode: "latest",
        payload: new TextEncoder().encode(JSON.stringify(value)),
      });
  const coalesced = (await stream.next()).value === 3;
  const failure = stream.next();
  for (const listener of listeners)
    listener({
      type: "topic-message",
      topic: "presence",
      mode: "latest",
      payload: new Uint8Array([0xff]),
    });
  let invalidRejected = false;
  try {
    await failure;
  } catch {
    invalidRejected = true;
  }
  const malformedFenced =
    invalidRejected &&
    listeners.size === 0 &&
    registrations.at(-1)?.some((topic) => topic.name === "presence") === false;
  let rejects = 0;
  try {
    registry.topic("bad name", { mode: "reliable" });
  } catch {
    rejects++;
  }
  try {
    await reliable.publish(undefined);
  } catch {
    rejects++;
  }
  try {
    await reliable.publish("x".repeat(65_537));
  } catch {
    rejects++;
  }
  const validation = rejects === 3 && sends.length === 1;
  registry.close();
  let postClose = false;
  try {
    await reliable.publish({ value: 9 });
  } catch {
    postClose = true;
  }
  return {
    stable,
    inert,
    subscribed,
    received,
    published,
    aborted,
    released,
    coalesced,
    malformedFenced,
    validation,
    postClose,
  };
})();
