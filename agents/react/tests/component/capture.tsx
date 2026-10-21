import assert from "node:assert/strict";
import { StrictMode, useState } from "react";
import { createRoot } from "react-dom/client";
import {
  useUserMedia,
  useDisplayMedia,
  useMediaDevices,
  type UserMediaOptions,
  type DisplayMediaOptions,
} from "../../src/capture.js";
import {
  captures,
  captureOptions,
  devices,
  displays,
  displayOptions,
  deferred,
  flush,
  Stream,
  Track,
} from "./platform.js";

export async function captureLifecycle() {
  let capture!: ReturnType<typeof useUserMedia>;
  let display!: ReturnType<typeof useDisplayMedia>;
  let inventory!: ReturnType<typeof useMediaDevices>;
  let change!: (options: UserMediaOptions) => void;
  let changeDisplay!: (options: DisplayMediaOptions) => void;
  const enumerations: ReturnType<typeof deferred<MediaDeviceInfo[]>>[] = [];
  const originalEnumeration = (devices as any).enumerateDevices;
  Object.assign(devices, {
    enumerateDevices: () => {
      const result = deferred<MediaDeviceInfo[]>();
      enumerations.push(result);
      return result.promise;
    },
  });
  const host = document.createElement("div");
  const root = createRoot(host);
  function Capture() {
    const [options, setOptions] = useState<UserMediaOptions>({
      video: true,
      audio: false,
    });
    const [displayOptions, setDisplayOptions] = useState<DisplayMediaOptions>({
      video: true,
      audio: true,
    });
    change = setOptions;
    changeDisplay = setDisplayOptions;
    capture = useUserMedia(options);
    display = useDisplayMedia(displayOptions);
    inventory = useMediaDevices();
    return <output>{capture.state}</output>;
  }
  const permission = new EventTarget();
  const previousPermissions = navigator.permissions;
  Object.defineProperty(navigator, "permissions", {
    configurable: true,
    value: { query: async () => permission },
  });
  const before = captures.length;
  await flush(() =>
    root.render(
      <StrictMode>
        <Capture />
      </StrictMode>,
    ),
  );
  assert.equal(captures.length, before, "render and commit do not acquire");
  const firstEnumeration = enumerations.at(-1)!;
  const beforePermission = enumerations.length;
  await flush(() => permission.dispatchEvent(new Event("change")));
  assert.equal(
    enumerations.length,
    beforePermission + 1,
    "supported permission changes refresh inventory",
  );
  await flush(() => devices.dispatchEvent(new Event("devicechange")));
  await flush(() =>
    enumerations
      .at(-1)!
      .resolve([
        { kind: "videoinput", deviceId: "cam", label: "Camera" },
      ] as MediaDeviceInfo[]),
  );
  await flush(() => firstEnumeration.resolve([]));
  assert.deepEqual(
    inventory.cameras,
    [{ kind: "camera", id: "cam", label: "Camera" }],
    "stale enumeration cannot erase current inventory",
  );
  let pending!: Promise<void>;
  await flush(() =>
    change({
      video: { device: { kind: "camera", id: "", label: "" } },
      audio: false,
    }),
  );
  let invalid!: Promise<unknown>;
  await flush(() => {
    invalid = capture.request().catch((error) => error);
  });
  assert.equal((await invalid).code, "device-not-found");
  assert.equal(
    captures.length,
    before,
    "invalid selection fails before acquisition",
  );
  await flush(() => change({ video: true, audio: false }));
  assert.equal(
    captures.length,
    before,
    "failed first acquisition needs a new explicit request",
  );
  await flush(() => {
    pending = capture.request();
  });
  assert.deepEqual(
    captureOptions.at(-1),
    { video: true, audio: false },
    "default capture needs no inventory device",
  );
  const initial = new Track("video");
  initial.settings = { deviceId: "cam", width: 640, frameRate: 24 };
  initial.label = "Camera";
  await flush(() => captures.at(-1)!.resolve(new Stream([initial])));
  await pending;
  assert.equal(
    host.textContent,
    "active",
    "request resolves after committed result",
  );
  assert.deepEqual(capture.video, {
    kind: "video",
    device: { kind: "camera", id: "cam", label: "Camera" },
    settings: { width: 640, frameRate: 24 },
  });
  const source = capture.video;
  const equalCount = captures.length;
  await flush(() => change({ video: { preferences: {} }, audio: false }));
  assert.equal(
    captures.length,
    equalCount,
    "true and default empty options are semantically equal",
  );
  const replacement: UserMediaOptions = {
    video: {
      device: { kind: "camera", id: "next", label: "Next" },
      preferences: { width: 1280, height: 720 },
    },
    audio: false,
  };
  await flush(() => change(replacement));
  assert.deepEqual(captureOptions.at(-1), {
    video: {
      deviceId: { exact: "next" },
      width: { ideal: 1280 },
      height: { ideal: 720 },
    },
    audio: false,
  });
  assert.equal(initial.readyState, "live", "keep previous during replacement");
  const wrong = new Track("video");
  wrong.settings = { deviceId: "wrong" };
  await flush(() => captures.at(-1)!.resolve(new Stream([wrong])));
  assert.equal(capture.error?.code, "device-not-found");
  assert.equal(capture.error?.kind, "video");
  assert.equal(wrong.readyState, "ended");
  assert.equal(capture.video, source);
  assert.equal(initial.readyState, "live");
  const failedCount = captures.length;
  await flush(() =>
    change({ ...replacement, video: { ...(replacement.video as object) } }),
  );
  assert.equal(
    captures.length,
    failedCount,
    "unchanged failed options do not retry",
  );
  const stopInitial = initial.stop.bind(initial);
  initial.stop = () => {
    assert.notEqual(
      capture.video,
      source,
      "superseded originals are released after the replacement source commits",
    );
    stopInitial();
  };
  await flush(() => {
    pending = capture.request();
  });
  const accepted = new Track("video");
  accepted.settings = { width: 960 };
  await flush(() => captures.at(-1)!.resolve(new Stream([accepted])));
  await pending;
  assert.deepEqual(
    capture.video,
    { kind: "video", device: null, settings: { width: 960 } },
    "unknown identity and settings are not fabricated",
  );
  assert.equal(initial.readyState, "ended");
  const priorCount = captures.length;
  await flush(() =>
    change({
      video: {
        device: { kind: "camera", id: "next", label: "Next" },
        preferences: { width: 1280, height: 720, frameRate: Number.NaN },
      },
      audio: false,
    }),
  );
  assert.equal(capture.error?.code, "constraint-unsatisfied");
  assert.equal(
    captures.length,
    priorCount,
    "invalid preferences fail before acquisition",
  );
  await flush(() => accepted.end());
  assert.equal(capture.state, "idle");
  await flush(() => change({ video: true, audio: false }));
  assert.equal(captures.length, priorCount, "ending cannot restart capture");
  await flush(() => {
    pending = display.request();
  });
  assert.deepEqual(displayOptions.at(-1), { video: true, audio: true });
  const shared = new Track("video");
  shared.settings = { deviceId: "screen-not-camera", height: 1080 };
  await flush(() => displays.at(-1)!.resolve(new Stream([shared])));
  await pending;
  assert.equal(display.audio, null, "display audio is optional");
  assert.deepEqual(display.video, {
    kind: "video",
    device: null,
    settings: { height: 1080 },
  });
  const displayCount = displays.length;
  await flush(() =>
    changeDisplay({ video: { preferences: { height: 720 } }, audio: false }),
  );
  assert.equal(
    displays.length,
    displayCount,
    "option changes never reopen chooser",
  );
  let failed!: Promise<unknown>;
  await flush(() => {
    failed = display.request().catch((e) => e);
  });
  await flush(() =>
    displays
      .at(-1)!
      .reject(new DOMException("cancel diagnostic", "NotAllowedError")),
  );
  assert.equal(
    (await failed).code,
    "permission-denied",
    "diagnostic text cannot determine error category",
  );
  assert.equal(shared.readyState, "live");
  await flush(() => shared.end());
  assert.equal(display.state, "idle");
  let stale!: Promise<unknown>;
  await flush(() => {
    stale = capture.request().catch((e) => e);
  });
  const older = captures.at(-1)!;
  await flush(() =>
    change({
      video: false,
      audio: {
        preferences: {
          echoCancellation: false,
          noiseSuppression: true,
          autoGainControl: false,
        },
      },
    }),
  );
  assert.deepEqual(captureOptions.at(-1), {
    video: false,
    audio: {
      echoCancellation: { ideal: false },
      noiseSuppression: { ideal: true },
      autoGainControl: { ideal: false },
    },
  });
  const microphone = new Track("audio");
  microphone.settings = { echoCancellation: true, noiseSuppression: false };
  await flush(() => captures.at(-1)!.resolve(new Stream([microphone])));
  const late = new Track("video");
  await flush(() => older.resolve(new Stream([late])));
  assert.equal((await stale).code, "request-cancelled");
  assert.equal(late.readyState, "ended");
  assert.deepEqual(capture.audio?.settings, {
    echoCancellation: true,
    noiseSuppression: false,
  });
  await flush(() => change({ video: false, audio: false }));
  assert.equal(capture.state, "idle");
  assert.equal(microphone.readyState, "ended");
  const retained = capture.request;
  await flush(() => root.unmount());
  await assert.rejects(retained(), { code: "request-cancelled" });
  const afterUnmount = enumerations.length;
  await flush(() => {
    permission.dispatchEvent(new Event("change"));
    devices.dispatchEvent(new Event("devicechange"));
  });
  assert.equal(
    enumerations.length,
    afterUnmount,
    "inventory listeners are released",
  );
  Object.defineProperty(navigator, "permissions", {
    configurable: true,
    value: previousPermissions,
  });
  Object.assign(devices, { enumerateDevices: originalEnumeration });
}
