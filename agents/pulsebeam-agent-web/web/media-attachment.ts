import { nativeSourceTrack, watchSource } from "./capture-source.js";
import type { AudioSource, VideoSource } from "./capture-source.js";
import {
  bindVideoView,
  type AudioSubscription,
  type VideoSubscription,
  type MediaObservation,
} from "./reception.js";

export interface MediaReceiver {
  observe(signal?: AbortSignal): Promise<MediaObservation>;
  isClosed(): boolean;
  updates(): {
    next(): Promise<unknown | undefined>;
    close(): void;
  };
}
export type MediaInput =
  | AudioSource
  | VideoSource
  | AudioSubscription
  | VideoSubscription
  | MediaReceiver
  | MediaObservation;
export interface BorrowedMediaAttachment {
  setAutoPlay(value: boolean): void;
  retryPlayback(): Promise<void>;
  close(): Promise<void>;
}

export function attachMedia(
  source: MediaInput,
  kind: "video" | "audio",
  element: HTMLMediaElement,
  autoPlay: boolean,
  blocked: (error: unknown, retry: () => Promise<void>) => void,
  failed: (error: unknown) => void,
  ready?: Promise<void>,
): BorrowedMediaAttachment {
  let live = true;
  let playing = autoPlay;
  let attempt = 0;
  let revision = 0;
  const stream = new MediaStream();
  element.srcObject = stream;
  let observation: MediaObservation | undefined;
  let pending: Promise<void> | undefined;
  let controller: AbortController | undefined;
  let cursor: ReturnType<MediaReceiver["updates"]> | undefined;
  let unwatch = () => {};
  let unlisten = () => {};
  let unview = () => {};
  const releases = new Set<Promise<void>>();
  const trackRelease = (closing: Promise<void>) => {
    releases.add(closing);
    void closing.then(
      () => releases.delete(closing),
      () => releases.delete(closing),
    );
    return closing;
  };
  const detach = () => {
    unlisten();
    unlisten = () => {};
    ++attempt;
    for (const track of stream.getTracks()) stream.removeTrack(track);
    if (element.srcObject === stream) element.srcObject = null;
  };
  const play = async () => {
    const current = stream;
    if (!live || current.getTracks().length === 0) return;
    const ticket = ++attempt;
    try {
      await element.play();
    } catch (error) {
      if (
        !live ||
        stream !== current ||
        ticket !== attempt ||
        (error instanceof DOMException && error.name === "AbortError")
      )
        return;
      const retry = async () => {
        if (live && ticket === attempt) await play();
      };
      blocked(error, retry);
    }
  };
  const present = (track: MediaStreamTrack | null) => {
    detach();
    if (!live || !track || track.kind !== kind || track.readyState !== "live")
      return;
    const ended = () => {
      if ("observe" in source) reconcile();
      else detach();
    };
    track.addEventListener("ended", ended);
    unlisten = () => track.removeEventListener("ended", ended);
    stream.addTrack(track);
    element.srcObject = stream;
    if (playing) void play();
  };
  const retire = () => {
    detach();
    const old = observation;
    observation = undefined;
    if (old) void trackRelease(old.close()).catch(failed);
  };
  const reconcile = () => {
    if (!live || !("observe" in source)) return;
    if (source.isClosed()) {
      unview();
      retire();
      controller?.abort();
      return;
    }
    if (
      observation &&
      !observation.isClosed() &&
      observation.track()?.readyState === "live"
    )
      return;
    if (observation) retire();
    if (pending) return;
    const start = revision;
    const acquisition = new AbortController();
    controller = acquisition;
    pending = source
      .observe(acquisition.signal)
      .then(async (value) => {
        if (
          !live ||
          acquisition.signal.aborted ||
          source.isClosed() ||
          value.isClosed()
        ) {
          await value.close();
          return;
        }
        observation = value;
        present(value.track());
      })
      .catch((error: unknown) => {
        if (!live || acquisition.signal.aborted || source.isClosed()) return;
        const code =
          typeof error === "object" && error !== null && "code" in error
            ? error.code
            : undefined;
        if (code !== "closed") failed(error);
      })
      .finally(() => {
        pending = undefined;
        if (controller === acquisition) controller = undefined;
        if (live && revision !== start) reconcile();
      });
  };
  const initialize = () => {
    try {
      if (kind === "video")
        unview = bindVideoView(source, element as HTMLVideoElement, (error) => {
          if (live) failed(error);
        });
      if ("observe" in source) {
        cursor = source.updates();
        void (async () => {
          while (live) {
            const state = await cursor!.next();
            if (!live || state === undefined) return;
            ++revision;
            reconcile();
          }
        })().catch((error: unknown) => {
          if (live) failed(error);
        });
      } else if ("track" in source) {
        present(source.track());
      } else {
        unwatch = watchSource(source, () => present(null));
        present(nativeSourceTrack(source, kind));
      }
    } catch (error) {
      failed(error);
    }
  };
  if (ready) {
    void ready
      .then(() => {
        if (live) initialize();
      })
      .catch((error: unknown) => {
        if (live) failed(error);
      });
  } else initialize();
  let closing: Promise<void> | undefined;
  return {
    setAutoPlay(value) {
      if (!live || playing === value) return;
      playing = value;
      ++attempt;
      if (playing && stream) void play();
    },
    retryPlayback: play,
    close() {
      if (closing) return closing;
      live = false;
      controller?.abort();
      cursor?.close();
      unwatch();
      unview();
      detach();
      const old = observation;
      observation = undefined;
      return (closing = Promise.allSettled([
        old?.close(),
        ready,
        pending,
        ...releases,
      ]).then(() => {}));
    },
  };
}
