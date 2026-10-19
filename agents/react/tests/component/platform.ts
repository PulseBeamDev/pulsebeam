import { act } from "react";

export function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (error: unknown) => void;
  const promise = new Promise<T>((yes, no) => {
    resolve = yes;
    reject = no;
  });
  return { promise, resolve, reject };
}

export class Track extends EventTarget {
  readonly id = crypto.randomUUID();
  readyState = "live";
  enabled = true;
  stopped = 0;
  constructor(readonly kind: "video" | "audio") {
    super();
  }
  stop() {
    this.readyState = "ended";
    this.stopped++;
  }
  clone() {
    return new Track(this.kind);
  }
  getSettings() {
    return {};
  }
}
export class Stream {
  constructor(readonly tracks: Track[] = []) {}
  getTracks() {
    return this.tracks;
  }
  getVideoTracks() {
    return this.tracks.filter((t) => t.kind === "video");
  }
  getAudioTracks() {
    return this.tracks.filter((t) => t.kind === "audio");
  }
}
export const captures: ReturnType<typeof deferred<Stream>>[] = [];
const devices = new EventTarget();
Object.assign(devices, {
  enumerateDevices: async () => [],
  getUserMedia: () => {
    const next = deferred<Stream>();
    captures.push(next);
    return next.promise;
  },
});
Object.defineProperty(navigator, "mediaDevices", {
  value: devices,
  configurable: true,
});
for (const target of [globalThis, window])
  Object.assign(target, { MediaStreamTrack: Track, MediaStream: Stream });
// DOM rendering is real; media transport and playback completion are external.
const attachments = new WeakMap<HTMLMediaElement, unknown>();
Object.defineProperty(HTMLMediaElement.prototype, "srcObject", {
  configurable: true,
  get() {
    return attachments.get(this) ?? null;
  },
  set(value: unknown) {
    attachments.set(this, value);
  },
});
HTMLMediaElement.prototype.play = async () => {};
HTMLMediaElement.prototype.pause = () => {};

class PlaybackContext {
  state = "suspended";
  async resume() {
    this.state = "running";
  }
  async close() {
    this.state = "closed";
  }
}
Object.assign(globalThis, { AudioContext: PlaybackContext });

export async function flush(work: () => unknown = () => {}) {
  await act(async () => {
    await work();
    // Flush promise-driven facade initialization and effect subscriptions, not time.
    for (let i = 0; i < 30; i++) await Promise.resolve();
  });
}
export function stream() {
  return new Stream([new Track("video"), new Track("audio")]);
}
