import type { MediaObservation } from "./reception.js";
import type { PlaybackFailure } from "./types.js";
import type { ReconciliationSnapshot } from "./reconciliation-snapshot.js";
import {
  attachMedia,
  type BorrowedMediaAttachment,
} from "./media-attachment.js";

export interface MediaHost {
  getSnapshot(): ReconciliationSnapshot;
  subscribe(listener: () => void): () => void;
}
interface VideoReceiver {
  observe(signal?: AbortSignal): Promise<MediaObservation>;
  isClosed(): boolean;
  close(): Promise<void>;
}
export type MediaAttachment = BorrowedMediaAttachment;
interface AttachmentOptions {
  readonly ready: Promise<void>;
  readonly subscribe: (listener: () => void) => () => void;
  readonly autoPlay?: boolean;
  readonly onPlaybackBlocked?: (
    failure: PlaybackFailure,
    retry: () => Promise<void>,
  ) => void;
}

export function attachRemoteMedia(
  source: VideoReceiver,
  element: HTMLVideoElement,
  options: AttachmentOptions,
): MediaAttachment {
  const receiver = {
    observe: (signal?: AbortSignal) => source.observe(signal),
    isClosed: () => source.isClosed(),
    updates() {
      let changed = true;
      let closed = false;
      let pending: ((value: object | undefined) => void) | undefined;
      const unsubscribe = options.subscribe(() => {
        if (closed) return;
        changed = true;
        if (pending) {
          const done = pending;
          pending = undefined;
          changed = false;
          done({});
        }
      });
      return {
        next(): Promise<object | undefined> {
          if (closed) return Promise.resolve(undefined);
          if (changed) {
            changed = false;
            return Promise.resolve({});
          }
          return new Promise((resolve) => {
            pending = resolve;
          });
        },
        close() {
          if (closed) return;
          closed = true;
          unsubscribe();
          pending?.(undefined);
          pending = undefined;
        },
      };
    },
  };
  const report = (error: unknown, retry: () => Promise<void>) =>
    options.onPlaybackBlocked?.(
      Object.freeze({
        message: error instanceof Error ? error.message : String(error),
        cause: error,
        retry,
      }),
      retry,
    );
  const attachment = attachMedia(
    receiver,
    "video",
    element,
    options.autoPlay ?? true,
    report,
    (error) => report(error, () => options.ready),
    options.ready,
  );
  let closing: Promise<void> | undefined;
  return {
    setAutoPlay: attachment.setAutoPlay,
    retryPlayback: attachment.retryPlayback,
    close: () =>
      (closing ??= Promise.allSettled([
        attachment.close(),
        source.close(),
      ]).then(() => {})),
  };
}
