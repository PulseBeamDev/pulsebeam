import assert from "node:assert/strict";
import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { MeetApp } from "../components/MeetApp";
import { AgentStatus, WatchStatus, CatalogDelivery } from "@pulsebeam/react";
import { AgentError, ErrorCode, type CatalogParticipant } from "@pulsebeam/web";
import {
  captures,
  displays,
  flush,
  stream,
} from "../../../agents/react/tests/component/platform";
import { scriptSdk } from "./sdk";

const sdk = scriptSdk();

// Observe effects before entry, including writes removed before the final check.
const token = "component-secret-" + crypto.randomUUID();
const effects: string[] = [];
const record = (values: unknown) => effects.push(JSON.stringify(values));
for (const name of ["localStorage", "sessionStorage"] as const) {
  const original = window[name];
  const proxy = new Proxy(original, {
    get(target, key) {
      const value = Reflect.get(target, key, target);
      if (typeof value !== "function") return value;
      return (...args: unknown[]) => {
        if (key === "setItem") record(args);
        return Reflect.apply(value, target, args);
      };
    },
    set(target, key, value) {
      record([key, value]);
      return Reflect.set(target, key, value, target);
    },
  });
  for (const target of [window, globalThis])
    Object.defineProperty(target, name, { configurable: true, value: proxy });
}
for (const name of ["pushState", "replaceState"] as const) {
  const original = history[name];
  history[name] = function (...args) {
    record(args);
    return Reflect.apply(original, this, args);
  };
}
for (const name of [
  "log",
  "warn",
  "error",
  "info",
  "debug",
  "trace",
] as const) {
  const original = console[name];
  console[name] = (...args) => {
    record(args);
    Reflect.apply(original, console, args);
  };
}
let cookieOwner: object | null = document;
let cookie: PropertyDescriptor | undefined;
while (cookieOwner && !cookie) {
  cookie = Object.getOwnPropertyDescriptor(cookieOwner, "cookie");
  cookieOwner = Object.getPrototypeOf(cookieOwner);
}
assert.ok(cookie?.get && cookie?.set);
Object.defineProperty(document, "cookie", {
  configurable: true,
  get: () => cookie!.get!.call(document),
  set: (value) => {
    record(value);
    cookie!.set!.call(document, value);
  },
});
globalThis.fetch = async (input, init) => {
  record(input instanceof Request ? input.url : String(input));
  throw new Error("unexpected network request in component model");
};
const sentinel = crypto.randomUUID();
localStorage.setItem("transient", sentinel);
localStorage.removeItem("transient");
assert.ok(
  effects.some((effect) => effect.includes(sentinel)),
  "oracle sees transient storage exposure",
);
effects.length = 0;
const privateSoFar = () => {
  record([location.href, document.cookie]);
  assert.ok(
    !effects.some(
      (effect) =>
        effect.includes(token) || effect.includes(encodeURIComponent(token)),
    ),
    "credentials never reach observed storage, URLs or console",
  );
};
function button(label: string) {
  const found = [...document.querySelectorAll("button")].find(
    (element) =>
      element.textContent?.trim() === label ||
      element.getAttribute("aria-label") === label,
  );
  assert.ok(found, `button ${label}`);
  return found;
}
async function click(label: string) {
  await flush(() => button(label).click());
}
async function input(
  element: HTMLInputElement | HTMLTextAreaElement,
  value: string,
) {
  const prototype =
    element.tagName === "TEXTAREA"
      ? window.HTMLTextAreaElement.prototype
      : HTMLInputElement.prototype;
  await flush(() => {
    Object.getOwnPropertyDescriptor(prototype, "value")!.set!.call(
      element,
      value,
    );
    element.dispatchEvent(new Event("input", { bubbles: true }));
  });
}

const host = document.createElement("div");
document.body.append(host);
const root = createRoot(host);
await flush(() =>
  root.render(
    <StrictMode>
      <MeetApp />
    </StrictMode>,
  ),
);
const password = document.querySelector<HTMLInputElement>(
  'input[type="password"]',
)!;
assert.ok(password, "token field is present");
assert.equal(password.autocomplete, "off");
await input(password, token);
const endpoint = document.querySelector<HTMLInputElement>(
  'input[name="endpoint"]',
)!;
await input(endpoint, "http://localhost:7070");
assert.equal(
  button("Join room").disabled,
  true,
  "token is insufficient before capture",
);
await flush(() =>
  document
    .querySelector("form")!
    .dispatchEvent(new Event("submit", { bubbles: true, cancelable: true })),
);
assert.equal(
  sdk.agents.length,
  0,
  "form submission cannot bypass required capture",
);
privateSoFar();

await flush(() =>
  captures.at(-1)!.reject(new DOMException("denied", "NotAllowedError")),
);
assert.equal(button("Join room").disabled, true);
await click("Retry");
const owned = stream();
await flush(() => captures.at(-1)!.resolve(owned));
await input(endpoint, "ftp://not-an-endpoint");
assert.equal(
  button("Join room").disabled,
  true,
  "invalid endpoint cannot join with capture",
);
await input(endpoint, "http://localhost:7070");
assert.equal(
  button("Join room").disabled,
  true,
  "a token never selects a room",
);
const roomField =
  document.querySelector<HTMLInputElement>('input[name="room"]')!;
await input(roomField, "é".repeat(129));
assert.equal(button("Join room").disabled, true);
assert.match(host.textContent!, /256 UTF-8 bytes/);
await flush(() =>
  roomField.form!.dispatchEvent(
    new Event("submit", { bubbles: true, cancelable: true }),
  ),
);
assert.equal(sdk.agents.length, 0, "invalid room never starts an Agent");
await input(roomField, "selected-room");
assert.equal(button("Join room").disabled, false);
await click("Mute microphone");
await click("Turn camera off");
assert.ok(
  owned.getTracks().every((track) => track.readyState === "live"),
  "lobby toggles retain capture for joining",
);
let plays = 0;
HTMLMediaElement.prototype.play = async () => {
  plays++;
  throw new DOMException("blocked", "NotAllowedError");
};
await click("Join room");
assert.equal(
  sdk.agents.filter((agent) => agent.state().status !== AgentStatus.Closed)
    .length,
  1,
);
const agent = sdk.agents.at(-1)!;
const deliver = (event: Parameters<typeof sdk.deliver>[1]) =>
  sdk.deliver("chat", event);
assert.deepEqual(sdk.joins, ["selected-room"]);
await flush(() =>
  sdk.changeAgent({
    status: AgentStatus.Joined,
    participantId: "admitted-alice",
    room: "admitted-room",
  }),
);
const activeWatch = () => sdk.watches().at(-1)!;
const firstParticipants = [
  {
    id: "admitted-alice",
    publications: [
      {
        id: "self-chat",
        trackId: "chat",
        kind: { tag: "Data", inner: { delivery: CatalogDelivery.Ordered } },
      },
      { id: "self-audio", trackId: "microphone", kind: { tag: "Audio" } },
      { id: "self-video", trackId: "camera", kind: { tag: "Video" } },
    ],
  },
  {
    id: "bob",
    publications: [
      {
        id: "remote-chat",
        trackId: "chat",
        kind: { tag: "Data", inner: { delivery: CatalogDelivery.Ordered } },
      },
      {
        id: "remote-reactions",
        trackId: "reactions",
        kind: { tag: "Data", inner: { delivery: CatalogDelivery.Lossy } },
      },
      { id: "remote-audio", trackId: "microphone", kind: { tag: "Audio" } },
      { id: "remote-camera", trackId: "camera", kind: { tag: "Video" } },
      { id: "remote-screen", trackId: "screen", kind: { tag: "Video" } },
    ],
  },
  { id: "quiet-observer", publications: [] },
];
const continuation = Object.freeze(
  Array.from({ length: 32 }, (_, index) => index),
);
const page = (participants = firstParticipants, hasMore = true) =>
  flush(() => {
    const watch = activeWatch();
    watch.change({
      status: WatchStatus.Live,
      participants: participants.map((participant) => ({
        id: () => participant.id,
        tracks: () => participant.publications,
      })) as unknown as CatalogParticipant[],
      hasMore,
      nextCursor: Uint8Array.from(hasMore ? continuation : []),
      cursorExpiresAtUnixSeconds: 1000n,
    });
  });
assert.match(document.body.textContent!, /Loading participants/);
assert.equal(activeWatch().state().limit, 100);
assert.deepEqual(Array.from(activeWatch().state().cursor), []);
await page();
assert.match(
  document.body.textContent!,
  /quiet-observer/,
  "presence-only entries render in the bounded roster",
);
assert.match(document.body.textContent!, /admitted-alice \(you\)/);
assert.match(document.querySelector("header")!.textContent!, /admitted-alice/);
assert.match(
  document.querySelector("header")!.textContent!,
  /Room admitted-room/,
);
await flush(() => sdk.changeAgent({ room: "selected-room" }));
assert.equal(
  sdk.dataOwners().length,
  2,
  "room-pinned chat and reaction tracks attach for each discovered endpoint",
);
assert.deepEqual(
  sdk.audios().map(({ selector }) => selector),
  [{ room: "selected-room", participantId: "bob", trackId: "microphone" }],
);
assert.deepEqual(
  sdk
    .videos()
    .map(({ selector, configuration }) => ({
      selector,
      minFps: configuration.minFps,
      priority: configuration.priority,
    }))
    .sort((a, b) => a.selector.trackId.localeCompare(b.selector.trackId)),
  [
    {
      selector: {
        room: "selected-room",
        participantId: "bob",
        trackId: "camera",
      },
      minFps: 15,
      priority: 200,
    },
    {
      selector: {
        room: "selected-room",
        participantId: "bob",
        trackId: "screen",
      },
      minFps: 15,
      priority: 10,
    },
  ],
  "page-derived remote videos preserve spotlight/FPS/playout policy and exclude self",
);
const initialWatch = activeWatch();
await click("Next page");
assert.equal(initialWatch.state().status, WatchStatus.Closed);
assert.deepEqual(Array.from(activeWatch().state().cursor), continuation);
assert.match(document.body.textContent!, /Loading participants/);
assert.equal(
  sdk.dataOwners().length,
  0,
  "page replacement releases UI-owned subscriptions",
);
assert.equal(
  sdk.videos().length,
  0,
  "page replacement releases UI-owned video reception",
);
assert.equal(
  sdk.audios().length,
  0,
  "page replacement releases UI-owned audio reception",
);
await page([{ id: "last-presence", publications: [] }], false);
assert.match(document.body.textContent!, /last-presence/);
assert.doesNotMatch(document.body.textContent!, /quiet-observer/);
assert.equal(button("Next page").disabled, true);
assert.equal(
  sdk.watches().length,
  1,
  "Meet retains one bounded page, not an all-page traversal",
);
await click("First page");
assert.deepEqual(Array.from(activeWatch().state().cursor), []);
await page();
await click("Next page");
await flush(() => {
  activeWatch().change({
    status: WatchStatus.Failed,
    error: { code: 0, message: "Catalog cursor expired" },
    participants: undefined,
    hasMore: undefined,
    nextCursor: undefined,
  });
});
assert.match(document.body.textContent!, /continuation is invalid or expired/);
assert.equal(button("Next page").disabled, true);
assert.equal(button("First page").disabled, false);
await click("First page");
assert.deepEqual(Array.from(activeWatch().state().cursor), []);
await page();
assert.equal(
  sdk.dataOwners().length,
  2,
  "First-page recovery restores only non-self UI subscriptions",
);
const abandoned = stream();
await flush(() => captures[0].resolve(abandoned));
assert.ok(
  abandoned.getTracks().every((track) => track.readyState === "ended"),
  "StrictMode retired capture cannot resurrect in the room",
);
assert.ok(
  owned.getTracks().every((track) => track.readyState === "live"),
  "joined capture remains live",
);
privateSoFar();

assert.ok(button("Unmute microphone"), "lobby microphone intent survives join");
assert.ok(button("Turn camera on"), "lobby camera intent survives join");
await click("Unmute microphone");
await click("Turn camera on");
const beforeRetry = plays;
await click("Chat");
assert.ok(plays > beforeRetry, "ordinary interaction retries blocked playback");
const afterRejection = plays;
HTMLMediaElement.prototype.play = async () => {
  plays++;
};
await flush(() =>
  document
    .querySelector("header")!
    .dispatchEvent(new KeyboardEvent("keydown", { key: "x", bubbles: true })),
);
assert.ok(
  plays > afterRejection,
  "failed retry remains available to later keyboard interaction",
);
const payload = "delivered through production data tracks";
await flush(() =>
  deliver({
    publicationId: "remote-chat",
    participantId: "bob",
    sequence: 0n,
    data: new TextEncoder().encode(JSON.stringify(payload)),
  }),
);
assert.match(
  document.body.textContent!,
  /delivered through production data tracks/,
);
const remoteChat = (publicationId: string, sequence: bigint) =>
  deliver({
    participantId: "bob",
    publicationId,
    sequence,
    data: new TextEncoder().encode(JSON.stringify(payload)),
  });
await flush(() => remoteChat("remote-chat", 1n));
await flush(() => remoteChat("replacement-remote-chat", 1n));
assert.equal(
  document.body.textContent!.match(/delivered through production data tracks/g)
    ?.length,
  3,
  "payload IDs do not replace data-track and sequence identity",
);
const transcript = document.querySelector(
  '[role="log"][aria-label="Chat transcript"]',
);
assert.ok(transcript, "chat owns an inline transcript");
await flush(() =>
  deliver({
    participantId: "bob",
    publicationId: "remote-chat",
    sequence: 4n,
    data: new TextEncoder().encode(JSON.stringify("final remote message")),
  }),
);
await flush(() => {
  for (const data of [
    new Uint8Array([255]),
    new TextEncoder().encode("{"),
    new TextEncoder().encode("42"),
  ]) {
    deliver({
      publicationId: "remote-chat",
      participantId: "bob",
      sequence: 5n,
      data,
    });
    sdk.deliver("reactions", {
      publicationId: "remote-reactions",
      participantId: "bob",
      sequence: 5n,
      data,
    });
  }
});
const transcriptEntries = Array.from(transcript.children);
assert.equal(transcriptEntries.length, 4);
assert.match(
  transcriptEntries[2].textContent!,
  /delivered through production data tracks/,
);
assert.match(transcriptEntries[3].textContent!, /final remote message/);
await click("Close chat");
await click("Chat");
assert.match(
  document.querySelector('[aria-label="Chat transcript"]')!.textContent!,
  /final remote message/,
  "chat history remains visible when reopened",
);
const chat =
  document.querySelector<HTMLTextAreaElement>("textarea") ??
  document.querySelector<HTMLInputElement>("aside input");
assert.ok(chat, "chat composer is rendered");
await input(chat, "hello from component");
let completeSend!: () => void;
sdk.setSendCompletion(new Promise((resolve) => (completeSend = resolve)));
await click("Send message");
assert.equal(sdk.sent.length, 0, "send waits for runtime completion");
assert.doesNotMatch(
  transcript.textContent!,
  /hello from component/,
  "local chat appears only after the send succeeds",
);
await flush(() => completeSend());
sdk.setSendCompletion(Promise.resolve());
assert.ok(
  sdk.sent.some(
    (message) =>
      message.name === "chat" &&
      new TextDecoder()
        .decode(message.payload)
        .includes("hello from component"),
  ),
);
await flush();
const updatedTranscript = document.querySelector(
  '[role="log"][aria-label="Chat transcript"]',
)!;
assert.match(updatedTranscript.textContent!, /hello from component/);
assert.doesNotMatch(updatedTranscript.textContent!, /Sending…/);
const sentChat = sdk.sent.find((message) => message.name === "chat")!;
assert.equal(sentChat.mode, CatalogDelivery.Ordered);
assert.equal(
  JSON.parse(new TextDecoder().decode(sentChat.payload)),
  "hello from component",
);
const chatOwners = sdk
  .dataOwners()
  .filter(([owner]) => owner.state().selector.trackId === "chat");
assert.equal(chatOwners.length, 1);
const [[chatOwner, chatReads]] = chatOwners;
await flush(() =>
  chatReads.fail(
    AgentError.Operation.new({
      info: {
        code: ErrorCode.ContinuityLost,
        message: "Reliable data continuity was lost",
      },
    }),
  ),
);
assert.ok(chatOwner.state().closed, "the broken Reliable owner is released");
assert.match(
  updatedTranscript.closest("aside")!.textContent!,
  /Chat is unavailable/,
);
privateSoFar();
await flush(() =>
  sdk.deliver("reactions", {
    publicationId: "remote-reactions",
    participantId: "bob",
    sequence: 0n,
    data: new TextEncoder().encode(JSON.stringify("👍")),
  }),
);
assert.ok(
  document.querySelector('[aria-label="👍"]'),
  "incoming reaction renders",
);
await click("Send a reaction");
const reaction = document.querySelector<HTMLElement>(
  '[aria-label="React with 👍"]',
);
assert.ok(reaction, "incoming reaction is rendered");
await flush(() => reaction.click());
assert.ok(
  sdk.sent.some(
    (message) =>
      message.name === "reactions" &&
      new TextDecoder().decode(message.payload).includes("👍"),
  ),
  "reaction interaction sends through the production data handle",
);
const sentReaction = sdk.sent.find((message) => message.name === "reactions")!;
assert.equal(sentReaction.mode, CatalogDelivery.Lossy);
assert.equal(JSON.parse(new TextDecoder().decode(sentReaction.payload)), "👍");
const visibleReaction =
  document.querySelector<HTMLElement>('[aria-label="👍"]')!;
await flush(() =>
  visibleReaction.dispatchEvent(new Event("animationend", { bubbles: true })),
);
assert.ok(!visibleReaction.isConnected, "reaction expires with its animation");
await click("Mute microphone");
await click("Turn camera off");
assert.ok(
  owned.getTracks().every((track) => track.readyState === "live"),
  "muting retains capture until Leave",
);

await click("Share screen");
await flush(() => displays.at(-1)!.reject(new Error("display denied")));
assert.match(document.body.textContent!, /Screen share: display denied/);
await click("Share screen");
const shared = stream();
await flush(() => displays.at(-1)!.resolve(shared));
await click("Stop sharing");
assert.ok(
  shared.getTracks().every((track) => track.readyState === "ended"),
  "screen capture stops on release",
);
await click("Share screen");
const sharedOnLeave = stream();
await flush(() => displays.at(-1)!.resolve(sharedOnLeave));

await click("Leave");
assert.equal(agent.state().status, AgentStatus.Closed);
assert.ok(
  sharedOnLeave.getTracks().every((track) => track.readyState === "ended"),
  "Leave also releases display capture",
);
assert.ok(
  owned.getTracks().every((track) => track.readyState === "ended"),
  "Leave releases capture ownership",
);
assert.match(document.body.textContent!, /Ready to join/);
const nextCapture = stream();
await flush(() => captures.at(-1)!.resolve(nextCapture));
assert.ok(button("Mute microphone"), "Leave resets lobby microphone intent");
assert.ok(button("Turn camera off"), "Leave resets lobby camera intent");
await input(
  document.querySelector<HTMLInputElement>('input[type="password"]')!,
  token,
);
await input(
  document.querySelector<HTMLInputElement>('input[name="room"]')!,
  "selected-room",
);
await click("Join room");
await click("Leave");
assert.ok(
  nextCapture.getTracks().every((track) => track.readyState === "ended"),
);
const pending = captures.at(-1)!;
await flush(() => root.unmount());
const late = stream();
await flush(() => pending.resolve(late));
assert.ok(
  late.getTracks().every((track) => track.readyState === "ended"),
  "late capture is retired",
);
assert.ok(
  sdk.agents.every((agent) => agent.state().status === AgentStatus.Closed),
  "all runtimes close on unmount",
);
privateSoFar();
host.remove();
console.log(
  "Meet capture-gated join, admission, binary data codecs, transient credential observation and Leave passed",
);
