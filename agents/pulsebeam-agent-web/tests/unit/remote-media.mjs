import assert from "node:assert/strict";
import { test } from "node:test";
import { attachRemoteMedia } from "../../unit-javascript/remote-media.js";
import { Video } from "./media-platform.mjs";

function videoTrack() {
  return Object.assign(new EventTarget(), {
    kind: "video",
    readyState: "live",
  });
}
async function settle() {
  for (let index = 0; index < 12; index++) await Promise.resolve();
}
function feed(initial) {
  let current = initial;
  const receivers = new Set();
  return {
    publish(track) {
      current = track;
      for (const receiver of receivers) receiver.update();
    },
    receiver() {
      let closed = false;
      const listeners = new Set();
      const pending = new Set();
      const observations = [];
      function observation() {
        const track = current;
        let retired = false;
        const value = {
          isClosed: () => retired || closed || track !== current,
          track: () => (value.isClosed() ? null : track),
          async close() {
            retired = true;
          },
        };
        observations.push(value);
        return value;
      }
      const source = {
        observations,
        isClosed: () => closed,
        async observe() {
          if (closed)
            throw Object.assign(new Error("closed"), { code: "closed" });
          if (current) return observation();
          return new Promise((resolve, reject) =>
            pending.add({ resolve, reject }),
          );
        },
        subscribe(listener) {
          listeners.add(listener);
          return () => listeners.delete(listener);
        },
        update() {
          if (closed) return;
          if (current) {
            for (const reader of pending) reader.resolve(observation());
            pending.clear();
          }
          for (const listener of listeners) listener();
        },
        async close() {
          closed = true;
          receivers.delete(source);
          for (const reader of pending)
            reader.reject(
              Object.assign(new Error("closed"), { code: "closed" }),
            );
          pending.clear();
        },
      };
      receivers.add(source);
      return source;
    },
  };
}
function attach(source, element, options = {}) {
  return attachRemoteMedia(source, element, {
    ready: Promise.resolve(),
    subscribe: source.subscribe,
    ...options,
  });
}

test("media presentation acquires exact-source observations and fences retired playback", async () => {
  const firstTrack = videoTrack();
  const replacement = videoTrack();
  const media = feed();
  const source = media.receiver();
  const element = new Video();
  let plays = 0;
  element.play = async () => {
    plays++;
  };
  const attachment = attach(source, element, { autoPlay: false });
  const stream = element.srcObject;
  await settle();
  assert.deepEqual(stream.getTracks(), []);
  media.publish(firstTrack);
  await settle();
  assert.deepEqual(stream.getTracks(), [firstTrack]);
  const original = source.observations[0];
  media.publish(firstTrack);
  await settle();
  assert.equal(
    source.observations.length,
    1,
    "unchanged source keeps its observation",
  );
  media.publish(replacement);
  await settle();
  assert.equal(original.isClosed(), true);
  assert.equal(
    original.track(),
    null,
    "retired observation cannot switch tracks",
  );
  assert.equal(
    source.observations.length,
    2,
    "presentation explicitly acquires a fresh observation",
  );
  assert.deepEqual(stream.getTracks(), [replacement]);
  media.publish(undefined);
  await settle();
  assert.deepEqual(stream.getTracks(), []);
  media.publish(firstTrack);
  await settle();
  assert.deepEqual(stream.getTracks(), [firstTrack]);
  assert.equal(plays, 0);
  attachment.setAutoPlay(true);
  assert.equal(plays, 1);
  attachment.setAutoPlay(true);
  assert.equal(plays, 1, "unchanged autoplay does not retry");
  attachment.setAutoPlay(false);
  media.publish(replacement);
  await settle();
  assert.equal(plays, 1, "disabled autoplay applies to replacement media");
  attachment.setAutoPlay(true);
  assert.equal(plays, 2);
  assert.equal(element.srcObject, stream);

  const siblingSource = media.receiver();
  const siblingElement = new Video();
  const sibling = attach(siblingSource, siblingElement);
  await settle();
  const external = new MediaStream();
  element.srcObject = external;
  const closing = attachment.close();
  assert.equal(attachment.close(), closing);
  await closing;
  assert.equal(
    element.srcObject,
    external,
    "cleanup preserves an external replacement",
  );
  assert.deepEqual(stream.getTracks(), []);
  assert.deepEqual(siblingElement.srcObject.getTracks(), [replacement]);
  assert.equal(siblingSource.isClosed(), false);
  assert.equal(
    replacement.readyState,
    "live",
    "closing a consumer does not stop receiver media",
  );
  await sibling.close();
  assert.equal(siblingElement.srcObject, null);

  const blockedSource = media.receiver();
  const blockedElement = new Video();
  let rejected;
  let retry;
  let blocked = 0;
  blockedElement.play = () =>
    new Promise((_, reject) => {
      rejected = reject;
    });
  const pending = attach(blockedSource, blockedElement, {
    onPlaybackBlocked(failure, nextRetry) {
      assert.equal(failure.message, "current receiver blocked");
      blocked++;
      retry = nextRetry;
    },
  });
  await settle();
  media.publish(undefined);
  rejected(new Error("retired receiver rejected late"));
  await settle();
  assert.equal(blocked, 0);
  media.publish(firstTrack);
  await settle();
  rejected(new Error("current receiver blocked"));
  await settle();
  assert.equal(blocked, 1);
  blockedElement.play = async () => {
    plays++;
  };
  await retry();
  assert.equal(plays, 3, "retry reaches the current attached element");
  blockedElement.play = () =>
    new Promise((_, reject) => {
      rejected = reject;
    });
  media.publish(replacement);
  await settle();
  await pending.close();
  rejected(new Error("closed receiver rejected late"));
  await settle();
  assert.equal(blocked, 1);
});

test("closing presentation awaits and retires a late physical observation", async () => {
  let resolve;
  let retired = false;
  let closed = false;
  const source = {
    isClosed: () => closed,
    observe: () =>
      new Promise((done) => {
        resolve = done;
      }),
    async close() {
      closed = true;
    },
    subscribe: () => () => {},
  };
  const element = new Video();
  const presentation = attach(source, element);
  await settle();
  let completed = false;
  const closing = presentation.close().then(() => {
    completed = true;
  });
  await settle();
  assert.equal(completed, false);
  resolve({
    isClosed: () => false,
    track: () => videoTrack(),
    async close() {
      retired = true;
    },
  });
  await closing;
  assert.equal(retired, true);
  assert.equal(element.srcObject, null);
});
