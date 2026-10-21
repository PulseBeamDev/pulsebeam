import assert from "node:assert/strict";
import { setImmediate } from "node:timers/promises";
import "./platform.mjs";

const encode = new TextEncoder();
const join = (...parts) => Uint8Array.from(parts.flatMap((part) => [...part]));
function varint(value) {
  let remaining = BigInt(value);
  const bytes = [];
  while (remaining >= 128n) {
    bytes.push(Number(remaining & 127n) | 128);
    remaining >>= 7n;
  }
  return Uint8Array.from([...bytes, Number(remaining)]);
}
const uint = (field, value) => join([field * 8], varint(value));
const bytes = (field, value) =>
  join([field * 8 + 2], varint(value.length), value);
const text = (field, value) => bytes(field, encode.encode(value));
function signalingWire(protobuf) {
  const lengths = [];
  if (protobuf.length >= 15) {
    let remaining = protobuf.length - 15;
    while (remaining >= 255) {
      lengths.push(255);
      remaining -= 255;
    }
    lengths.push(remaining);
  }
  return join([Math.min(protobuf.length, 15) * 16], lengths, protobuf);
}
const participant = (canonical, external) =>
  bytes(1, join(text(1, canonical), text(2, external)));
const track = (id, owner) =>
  bytes(
    2,
    join(
      text(1, id),
      text(2, owner),
      uint(3, 3),
      text(4, "events"),
      uint(5, 3),
    ),
  );
const catalog = (revision, bound) =>
  bytes(
    1,
    join(
      uint(1, revision),
      bytes(
        2,
        join(
          participant("canonical-self", "self"),
          participant("canonical-remote", "alice"),
          ...(bound
            ? [
                track("server-local-track", "canonical-self"),
                track("server-remote-track", "canonical-remote"),
              ]
            : []),
        ),
      ),
    ),
  );
const mapping = bytes(2, join(uint(1, 1), bytes(2, []), bytes(3, [])));
const frame = (kind, sequence, payload) =>
  bytes(
    kind,
    join(
      text(1, "server-remote-track"),
      uint(2, sequence),
      ...(payload ? [bytes(3, payload)] : []),
    ),
  );

// Control only platform and server outcomes. Production WASM owns admission,
// Catalog validation, identity projection, topic decoding and cancellation.
export async function topicContract(pulsebeam) {
  const previous = Object.fromEntries(
    [
      "RTCPeerConnection",
      "Window",
      "window",
      "fetch",
      "setTimeout",
      "clearTimeout",
    ].map((key) => [key, globalThis[key]]),
  );
  const peers = [];
  const timers = new Map();
  let timerId = 0;
  let rejectAdmission = false;
  class Channel {
    readyState = "connecting";
    sent = [];
    constructor(label) {
      this.label = label;
    }
    send(value) {
      this.sent.push(Uint8Array.from(value));
    }
    open() {
      this.readyState = "open";
      this.onopen?.(new Event("open"));
    }
    close() {
      this.readyState = "closed";
      this.onclose?.(new Event("close"));
    }
    receive(value) {
      this.onmessage?.(
        new MessageEvent("message", { data: Uint8Array.from(value).buffer }),
      );
    }
  }
  class Peer extends previous.RTCPeerConnection {
    channels = new Map();
    constructor(...args) {
      super(...args);
      peers.push(this);
    }
    createDataChannel(label) {
      const channel = new Channel(label);
      this.channels.set(label, channel);
      return channel;
    }
    async createOffer() {
      return { type: "offer", sdp: "v=0\r\n" };
    }
    async setLocalDescription(description) {
      this.localDescription = description;
    }
    async setRemoteDescription() {
      this.answerApplied = true;
    }
    open() {
      this.connectionState = "connected";
      this.onconnectionstatechange?.(new Event("connectionstatechange"));
      for (const channel of this.channels.values()) channel.open();
    }
  }
  class Window {
    static [Symbol.hasInstance](value) {
      return value === globalThis;
    }
  }
  Object.assign(globalThis, {
    RTCPeerConnection: Peer,
    Window,
    window: globalThis,
    fetch: async (request) =>
      request.method === "POST" && rejectAdmission
        ? new Response(null, { status: 401 })
        : request.method === "POST"
          ? new Response(
              JSON.stringify({
                room_external_id: "room",
                room_id: "canonical-room",
                participant_external_id: "self",
                participant_id: "canonical-self",
                connection_id: "connection",
                answer: "v=0\r\n",
              }),
              {
                status: 201,
                headers: { Location: "http://localhost/connection" },
              },
            )
          : new Response(null, { status: 204 }),
    setTimeout: (callback, delay) => {
      const id = ++timerId;
      timers.set(id, { callback, delay });
      return id;
    },
    clearTimeout: (id) => timers.delete(id),
  });
  async function until(condition, label) {
    for (let step = 0; step < 1000; step++) {
      if (condition()) return;
      await setImmediate();
    }
    assert.fail(
      `${label}: ${JSON.stringify({ snapshot: agent.getSnapshot(), peers: peers.length }, (_, value) => (typeof value === "bigint" ? value.toString() : value))}`,
    );
  }
  const agent = pulsebeam.createAgent({
    endpoint: "http://localhost",
    token: "controlled-admission",
    logging: { level: "off" },
    topology: {
      localVideos: 0,
      localAudios: 0,
      remoteVideos: 0,
      remoteAudios: 0,
    },
  });
  const topic = agent.topic("events", { mode: "ordered" });
  const iterator = topic.subscribe()[Symbol.asyncIterator]();
  let lastGeneration = null;
  let nextPeer = 0;
  async function connectPeer() {
    const index = nextPeer++;
    await until(
      () => peers[index]?.answerApplied,
      "controlled answer was not applied",
    );
    const peer = peers[index];
    await until(
      () =>
        agent.getSnapshot().generation !== null &&
        agent.connection !== "connected",
      "fresh generation was not observed",
    );
    peer.open();
    await until(
      () =>
        agent.connection === "connected" &&
        agent.getSnapshot().generation !== lastGeneration,
      "production runtime did not connect",
    );
    lastGeneration = agent.getSnapshot().generation;
    const signaling = peer.channels.get("v1/sys/signaling");
    signaling.receive(signalingWire(catalog(1, false)));
    signaling.receive(signalingWire(mapping));
    await until(
      () =>
        agent.getSnapshot().catalog.revision === 1 &&
        agent.getSnapshot().mapping.acceptedIntentRevision === 1,
      "initial signaling state was not applied",
    );
    return peer;
  }
  try {
    agent.setState({
      connected: true,
      topics: [{ name: "events", mode: "ordered", subscribe: true }],
    });
    const initialPeer = await connectPeer();
    assert.equal(initialPeer.channels.has("v2/ordered/pub/events"), false);
    const pending = topic.publish({ text: "held for binding" });
    let admitted = false;
    void pending.then(
      () => (admitted = true),
      () => {},
    );
    const peer = await connectPeer();
    const publisher = peer.channels.get("v2/ordered/pub/events");
    assert.equal(admitted, false);
    assert.deepEqual(publisher.sent, []);
    peer.channels
      .get("v1/sys/signaling")
      .receive(signalingWire(catalog(2, true)));
    await pending;
    assert.equal(publisher.sent.length, 1);
    assert.deepEqual(
      publisher.sent[0],
      bytes(
        1,
        join(
          text(1, "server-local-track"),
          bytes(3, encode.encode('{"text":"held for binding"}')),
        ),
      ),
    );
    assert.equal(agent.participantId, "self");
    assert.equal(agent.roomId, "room");
    assert.equal("participantExternalId" in agent, false);
    assert.equal("roomExternalId" in agent, false);
    const snapshot = agent.getSnapshot();
    assert.equal(
      snapshot.topics.publishers[0].dataTrackId,
      "server-local-track",
    );
    assert.equal(snapshot.topics.publishers[0].nextSequence, 1n);
    assert.deepEqual(snapshot.catalog.participants, [
      { participantId: "alice" },
    ]);
    for (const value of [
      snapshot.version,
      snapshot.desiredRevision,
      snapshot.generation,
      snapshot.catalog.revision,
      snapshot.mapping.acceptedIntentRevision,
      snapshot.topics.acceptedSends,
    ])
      assert.equal(typeof value, "number");
    assert.deepEqual(snapshot.catalog.publications, []);
    assert.deepEqual(snapshot.catalog.tracks, [
      {
        id: "server-remote-track",
        participantId: "alice",
        kind: "data",
        label: "events",
        delivery: "ordered",
      },
    ]);
    assert.equal(Object.isFrozen(snapshot.catalog.tracks[0]), true);
    assert.deepEqual(
      agent.remote.participants.map((entry) => entry.participantId),
      ["alice"],
    );

    const expected = 2n ** 64n - 5n;
    const available = 2n ** 64n - 2n;
    const gap = iterator.next();
    const message = iterator.next();
    const subscriber = peer.channels.get("v2/ordered/sub/events");
    subscriber.receive(frame(4, expected));
    subscriber.receive(
      frame(1, available, encode.encode('{"text":"lossless"}')),
    );
    subscriber.receive(frame(5, available));
    const identity = {
      dataTrackId: "server-remote-track",
      participantId: "alice",
      topic: "events",
    };
    assert.deepEqual((await gap).value, {
      type: "gap",
      ...identity,
      expectedSequence: expected,
      availableSequence: available,
    });
    assert.deepEqual((await message).value, {
      type: "message",
      ...identity,
      sequence: available,
      value: { text: "lossless" },
    });

    agent.reconnect();
    await assert.rejects(
      topic.publish({ text: "old generation" }),
      /generation|binding|closed/,
    );
    await connectPeer();
    assert.equal(
      publisher.sent.length,
      1,
      "stale publish never reached the old channel",
    );
    const disconnected = assert.rejects(
      topic.publish({ text: "disconnect" }),
      /closed|unavailable|generation/,
    );
    agent.disconnect();
    await disconnected;
    await until(
      () => agent.connection === "disconnected",
      "disconnect did not finish",
    );
    rejectAdmission = true;
    const rejectedPeer = nextPeer++;
    agent.connect();
    await until(
      () => agent.connection === "terminal-failure",
      "authorization did not fail",
    );
    assert.equal(peers.length, rejectedPeer + 1);
    assert.equal(agent.error.class, "authorization");
    rejectAdmission = false;
    agent.connect();
    const last = await connectPeer();
    assert.equal(
      agent.error,
      null,
      "explicit connect clears terminal failure without withdrawing intent",
    );
    const timedOut = topic.publish({ text: "deadline" });
    const deadline = timerId;
    assert.equal(timers.get(deadline).delay, 10_000);
    const timeoutRejection = assert.rejects(timedOut, /binding timed out/);
    timers.get(deadline).callback();
    await timeoutRejection;
    const channelClosed = assert.rejects(
      topic.publish({ text: "channel close" }),
      /closed|channel/,
    );
    last.channels.get("v2/ordered/pub/events").close();
    await channelClosed;
    const closed = assert.rejects(
      topic.publish({ text: "agent close" }),
      /closed|unavailable/,
    );
    agent.close();
    await closed;
    assert.deepEqual(await iterator.next(), { done: true, value: undefined });
    for (const peer of peers)
      for (const channel of peer.channels.values())
        assert.equal(
          channel.onmessage ?? null,
          null,
          "teardown released native callback ownership",
        );
  } finally {
    agent.close();
    Object.assign(globalThis, previous);
  }
}
