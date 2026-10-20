import assert from "node:assert/strict";
import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { MeetApp } from "../components/MeetApp";
import {
  captures,
  flush,
  stream,
} from "../../../agents/react/tests/component/platform";
import { runtimes } from "../../../agents/react/tests/component/runtime";

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
assert.ok(password);
assert.equal(password.autocomplete, "off");
await input(password, token);
const endpoint = [...document.querySelectorAll("form input")].find(
  (element) => element !== password,
) as HTMLInputElement;
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
  runtimes.length,
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
assert.equal(button("Join room").disabled, false);
let plays = 0;
HTMLMediaElement.prototype.play = async () => {
  plays++;
  throw new DOMException("blocked", "NotAllowedError");
};
await click("Join room");
assert.equal(runtimes.filter((runtime) => !runtime.closed).length, 1);
const runtime = runtimes.at(-1)!;
assert.equal(runtime.desired.connected, true);
await flush(() =>
  runtime.emit({
    connection: "connected",
    participantId: "admitted-alice",
    roomId: "admitted-room",
    generation: 1n,
  }),
);
assert.match(document.querySelector("header")!.textContent!, /admitted-alice/);
assert.match(
  document.querySelector("header")!.textContent!,
  /Room admitted-room/,
);
const abandoned = stream();
await flush(() => captures[0].resolve(abandoned));
assert.ok(
  abandoned.getTracks().every((track) => track.readyState === "ended"),
  "StrictMode retired capture cannot resurrect in the room",
);
assert.ok(owned.getTracks().every((track) => track.readyState === "live"));
privateSoFar();

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
const payload = {
  id: "ignored-payload-id",
  sender: "forged-payload-sender",
  text: "delivered through production topics",
  ts: 1,
};
await flush(() =>
  runtime.event({
    type: "topic-message",
    topic: "chat",
    mode: "ordered",
    dataTrackId: "remote-chat",
    participantId: "bob",
    sequence: 0n,
    payload: new TextEncoder().encode(JSON.stringify(payload)),
  }),
);
assert.match(document.body.textContent!, /delivered through production topics/);
assert.doesNotMatch(document.body.textContent!, /forged-payload-sender/);
const remoteChat = (dataTrackId: string, sequence: bigint) =>
  runtime.event({
    type: "topic-message",
    topic: "chat",
    mode: "ordered",
    participantId: "bob",
    dataTrackId,
    sequence,
    payload: new TextEncoder().encode(JSON.stringify(payload)),
  });
await flush(() => remoteChat("remote-chat", 0n));
assert.equal(
  document.body.textContent!.match(/delivered through production topics/g)
    ?.length,
  1,
  "same track and sequence deduplicate",
);
await flush(() => remoteChat("remote-chat", 1n));
await flush(() => remoteChat("replacement-remote-chat", 1n));
assert.equal(
  document.body.textContent!.match(/delivered through production topics/g)
    ?.length,
  3,
  "payload IDs do not replace data-track and sequence identity",
);
await flush(() =>
  runtime.event({
    type: "topic-recovery-gap",
    topic: "chat",
    participantId: "bob",
    dataTrackId: "remote-chat",
    expectedSequence: 2n,
    availableSequence: 4n,
  }),
);
const transcript = document.querySelector(
  '[role="log"][aria-label="Chat transcript"]',
);
assert.ok(transcript, "chat owns an inline transcript");
assert.match(
  transcript.lastElementChild!.textContent!,
  /Some chat messages were skipped/,
);
assert.match(transcript.lastElementChild!.textContent!, /bob: 2 unavailable/);
await flush(() =>
  runtime.event({
    type: "topic-message",
    topic: "chat",
    mode: "ordered",
    participantId: "bob",
    dataTrackId: "remote-chat",
    sequence: 4n,
    payload: new TextEncoder().encode(
      JSON.stringify({ text: "after the gap", ts: 4 }),
    ),
  }),
);
const transcriptEntries = Array.from(transcript.children);
assert.equal(transcriptEntries.length, 5);
assert.match(
  transcriptEntries[2].textContent!,
  /delivered through production topics/,
);
assert.match(
  transcriptEntries[3].textContent!,
  /Some chat messages were skipped/,
);
assert.match(transcriptEntries[4].textContent!, /after the gap/);
assert.equal(transcript.querySelectorAll('[role="alert"]').length, 1);
await click("Close chat");
await click("Chat");
assert.match(
  document.querySelector('[aria-label="Chat transcript"]')!.textContent!,
  /Some chat messages were skipped.*after the gap/s,
  "chat history retains the gap before continuation when reopened",
);
const chat =
  document.querySelector<HTMLTextAreaElement>("textarea") ??
  document.querySelector<HTMLInputElement>("aside input");
assert.ok(chat, "chat composer is rendered");
await input(chat, "hello from component");
await click("Send message");
assert.equal(
  runtime.sent.length,
  0,
  "initial publish waits for authoritative binding",
);
const chatBinding = {
  name: "chat",
  mode: "ordered",
  connected: true,
  dataTrackId: "local-chat",
  nextSequence: 0n,
  acceptedHistory: 0,
  queued: 0,
  sendPending: false,
};
await flush(() =>
  runtime.emit({
    topics: { ...runtime.snapshot.topics, publishers: [chatBinding] },
  }),
);
assert.ok(
  runtime.sent.some(
    (message) =>
      message.name === "chat" &&
      new TextDecoder()
        .decode(message.payload)
        .includes("hello from component"),
  ),
);
const sentChat = runtime.sent.find((message) => message.name === "chat")!;
assert.equal(sentChat.mode, "ordered");
assert.deepEqual(
  Object.keys(JSON.parse(new TextDecoder().decode(sentChat.payload))).sort(),
  ["text", "ts"],
);
privateSoFar();
await flush(() =>
  runtime.event({
    type: "topic-message",
    topic: "reactions",
    mode: "lossy",
    dataTrackId: "remote-reactions",
    participantId: "bob",
    sequence: 0n,
    payload: new TextEncoder().encode(JSON.stringify({ emoji: "👍" })),
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
assert.ok(reaction);
await flush(() => reaction.click());
await flush(() =>
  runtime.emit({
    topics: {
      ...runtime.snapshot.topics,
      publishers: [
        chatBinding,
        {
          ...chatBinding,
          name: "reactions",
          mode: "lossy",
          dataTrackId: "local-reactions",
        },
      ],
    },
  }),
);
assert.ok(
  runtime.sent.some(
    (message) =>
      message.name === "reactions" &&
      new TextDecoder().decode(message.payload).includes("👍"),
  ),
  "reaction interaction publishes through production topic policy",
);
const sentReaction = runtime.sent.find(
  (message) => message.name === "reactions",
)!;
assert.equal(sentReaction.mode, "lossy");
assert.deepEqual(JSON.parse(new TextDecoder().decode(sentReaction.payload)), {
  emoji: "👍",
});
await click("Mute microphone");
await click("Turn camera off");
assert.ok(
  owned.getTracks().every((track) => track.readyState === "live"),
  "muting retains capture until Leave",
);

await click("Leave");
assert.equal(runtime.closed, true);
assert.ok(
  owned.getTracks().every((track) => track.readyState === "ended"),
  "Leave releases capture ownership",
);
assert.match(document.body.textContent!, /Ready to join/);
const pending = captures.at(-1)!;
await flush(() => root.unmount());
const late = stream();
await flush(() => pending.resolve(late));
assert.ok(late.getTracks().every((track) => track.readyState === "ended"));
assert.ok(runtimes.every((runtime) => runtime.closed));
privateSoFar();
host.remove();
console.log(
  "Meet capture-gated join, admission, topics, transient credential observation and Leave passed",
);
