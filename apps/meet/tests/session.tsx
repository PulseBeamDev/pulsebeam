import assert from "node:assert/strict";
import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { MeetApp } from "../components/MeetApp";
import {
  captures,
  displays,
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
assert.equal(runtimes.filter((runtime) => !runtime.closed).length, 1);
const runtime = runtimes.at(-1)!;
const deliver = (event: unknown) => runtime.deliverData("chat", event);
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
    type: "message",
    trackId: "remote-chat",
    participantId: "bob",
    sequence: 0n,
    data: new TextEncoder().encode(JSON.stringify(payload)),
  }),
);
assert.match(
  document.body.textContent!,
  /delivered through production data tracks/,
);
const remoteChat = (trackId: string, sequence: bigint) =>
  deliver({
    type: "message",
    participantId: "bob",
    trackId,
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
await flush(() =>
  deliver({
    type: "gap",
    participantId: "bob",
    trackId: "remote-chat",
    from: 2n,
    to: 4n,
  }),
);
const transcript = document.querySelector(
  '[role="log"][aria-label="Chat transcript"]',
);
assert.ok(transcript, "chat owns an inline transcript");
assert.equal(
  transcript.lastElementChild!.textContent,
  "Some messages could not be recovered.",
);
await flush(() =>
  deliver({
    type: "message",
    participantId: "bob",
    trackId: "remote-chat",
    sequence: 4n,
    data: new TextEncoder().encode(JSON.stringify("after the gap")),
  }),
);
await flush(() => {
  for (const data of [
    new Uint8Array([255]),
    new TextEncoder().encode("{"),
    new TextEncoder().encode("42"),
  ]) {
    deliver({
      type: "message",
      trackId: "remote-chat",
      participantId: "bob",
      sequence: 5n,
      data,
    });
    runtime.deliverData("reactions", {
      type: "message",
      trackId: "remote-reactions",
      participantId: "bob",
      sequence: 5n,
      data,
    });
  }
});
const transcriptEntries = Array.from(transcript.children);
assert.equal(transcriptEntries.length, 5);
assert.match(
  transcriptEntries[2].textContent!,
  /delivered through production data tracks/,
);
assert.equal(
  transcriptEntries[3].textContent,
  "Some messages could not be recovered.",
);
assert.match(transcriptEntries[4].textContent!, /after the gap/);
assert.equal(transcript.querySelectorAll('[role="alert"]').length, 1);
await click("Close chat");
await click("Chat");
assert.match(
  document.querySelector('[aria-label="Chat transcript"]')!.textContent!,
  /Some messages could not be recovered\..*after the gap/s,
  "chat history retains the gap before continuation when reopened",
);
const chat =
  document.querySelector<HTMLTextAreaElement>("textarea") ??
  document.querySelector<HTMLInputElement>("aside input");
assert.ok(chat, "chat composer is rendered");
await input(chat, "hello from component");
let completeSend!: () => void;
runtime.sendCompletion = new Promise((resolve) => (completeSend = resolve));
await click("Send message");
assert.equal(runtime.sent.length, 0, "send waits for runtime completion");
assert.doesNotMatch(
  transcript.textContent!,
  /hello from component/,
  "local chat appears only after the send succeeds",
);
await flush(() => completeSend());
runtime.sendCompletion = Promise.resolve();
assert.ok(
  runtime.sent.some(
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
const sentChat = runtime.sent.find((message) => message.name === "chat")!;
assert.equal(sentChat.mode, "ordered");
assert.equal(
  JSON.parse(new TextDecoder().decode(sentChat.payload)),
  "hello from component",
);
privateSoFar();
await flush(() =>
  runtime.deliverData("reactions", {
    type: "message",
    trackId: "remote-reactions",
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
assert.ok(reaction);
await flush(() => reaction.click());
assert.ok(
  runtime.sent.some(
    (message) =>
      message.name === "reactions" &&
      new TextDecoder().decode(message.payload).includes("👍"),
  ),
  "reaction interaction sends through the production data handle",
);
const sentReaction = runtime.sent.find(
  (message) => message.name === "reactions",
)!;
assert.equal(sentReaction.mode, "lossy");
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
assert.ok(shared.getTracks().every((track) => track.readyState === "ended"));
await click("Share screen");
const sharedOnLeave = stream();
await flush(() => displays.at(-1)!.resolve(sharedOnLeave));

await click("Leave");
assert.equal(runtime.closed, true);
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
await click("Join room");
await click("Leave");
assert.ok(
  nextCapture.getTracks().every((track) => track.readyState === "ended"),
);
const pending = captures.at(-1)!;
await flush(() => root.unmount());
const late = stream();
await flush(() => pending.resolve(late));
assert.ok(late.getTracks().every((track) => track.readyState === "ended"));
assert.ok(runtimes.every((runtime) => runtime.closed));
privateSoFar();
host.remove();
console.log(
  "Meet capture-gated join, admission, binary data codecs, transient credential observation and Leave passed",
);
