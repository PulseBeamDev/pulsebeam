import type { MediaObservation } from "./reception.js";
import type { AgentSnapshot, PlaybackFailure } from "./types.js";

export interface MediaHost {
  getSnapshot(): AgentSnapshot;
  subscribe(listener: () => void): () => void;
}
interface VideoReceiver {
  observe(): Promise<MediaObservation>;
  isClosed(): boolean;
  close(): Promise<void>;
}
export interface MediaAttachment {
  setAutoPlay(autoPlay: boolean): void;
  retryPlayback(): Promise<void>;
  close(): Promise<void>;
}
interface AttachmentOptions {
  readonly ready: Promise<void>;
  readonly subscribe: (listener: () => void) => () => void;
  readonly autoPlay?: boolean;
  readonly onPlaybackBlocked?: (
    failure: PlaybackFailure,
    retry: () => Promise<void>,
  ) => void;
}
function failure(error: unknown, retry: () => Promise<void>): PlaybackFailure {
  return Object.freeze({
    message: error instanceof Error ? error.message : String(error),
    cause: error,
    retry,
  });
}

class Attachment implements MediaAttachment {
  #closed = false;
  #closing: Promise<void> | undefined;
  #ready = false;
  #autoPlay: boolean;
  #playAttempt = 0;
  #observation: MediaObservation | undefined;
  #track: MediaStreamTrack | undefined;
  #acquiring: Promise<void> | undefined;
  #invalidated = false;
  readonly #pending = new Set<Promise<unknown>>();
  readonly #stream = new MediaStream();
  readonly #unsubscribe: () => void;

  constructor(
    readonly source: VideoReceiver,
    readonly element: HTMLVideoElement,
    readonly options: AttachmentOptions,
  ) {
    if (!(element instanceof HTMLVideoElement))
      throw new TypeError("remote attachment requires a video element");
    this.#autoPlay = options.autoPlay ?? true;
    element.srcObject = this.#stream;
    this.#unsubscribe = options.subscribe(this.#reconcile);
    void options.ready.then(
      () => {
        if (this.#closed) return;
        this.#ready = true;
        this.#reconcile();
      },
      () => {},
    );
  }
  #trackPending<T>(pending: Promise<T>): Promise<T> {
    this.#pending.add(pending);
    void pending.then(
      () => this.#pending.delete(pending),
      () => this.#pending.delete(pending),
    );
    return pending;
  }
  #detach(): void {
    this.#playAttempt++;
    if (this.#track) {
      this.#track.removeEventListener("ended", this.#reconcile);
      this.#stream.removeTrack(this.#track);
      this.#track = undefined;
    }
  }
  readonly #reconcile = (): void => {
    if (this.#closed || !this.#ready) return;
    if (this.source.isClosed()) {
      void this.close();
      return;
    }
    if (this.#observation) {
      if (!this.#observation.isClosed() && this.#track?.readyState !== "ended")
        return;
      const retired = this.#observation;
      this.#observation = undefined;
      this.#detach();
      void this.#trackPending(retired.close());
    }
    if (this.#acquiring) {
      this.#invalidated = true;
      return;
    }
    let retired = false;
    const acquiring = this.source
      .observe()
      .then(async (observation) => {
        if (this.#closed || this.source.isClosed() || observation.isClosed()) {
          await observation.close();
          retired = !this.#closed && !this.source.isClosed();
          return;
        }
        const track = observation.track();
        if (!track || track.kind !== "video") {
          await observation.close();
          return;
        }
        this.#observation = observation;
        this.#track = track;
        track.addEventListener("ended", this.#reconcile);
        this.#stream.addTrack(track);
        this.#playAttempt++;
        if (this.#autoPlay) void this.#play();
      })
      .catch((error: unknown) => {
        if (this.#closed || this.source.isClosed()) return;
        if (
          typeof error === "object" &&
          error !== null &&
          "code" in error &&
          error.code === "closed"
        )
          retired = true;
        else this.#report(error);
      })
      .finally(() => {
        this.#acquiring = undefined;
        const invalidated = this.#invalidated;
        this.#invalidated = false;
        if ((retired || invalidated) && !this.#closed) this.#reconcile();
      });
    this.#acquiring = this.#trackPending(acquiring);
  };
  #report(error: unknown): void {
    const retry = () => this.retryPlayback();
    this.options.onPlaybackBlocked?.(failure(error, retry), retry);
  }
  setAutoPlay(autoPlay: boolean): void {
    if (this.#closed || autoPlay === this.#autoPlay) return;
    this.#autoPlay = autoPlay;
    this.#playAttempt++;
    if (autoPlay) void this.#play();
  }
  retryPlayback(): Promise<void> {
    return this.#play();
  }
  #play(): Promise<void> {
    if (this.#closed || !this.#track) return Promise.resolve();
    const attempt = ++this.#playAttempt;
    return this.element.play().catch((error: unknown) => {
      if (
        this.#closed ||
        attempt !== this.#playAttempt ||
        (error instanceof DOMException && error.name === "AbortError")
      )
        return;
      this.#report(error);
    });
  }
  close(): Promise<void> {
    if (this.#closing) return this.#closing;
    this.#closed = true;
    this.#unsubscribe();
    this.#detach();
    if (this.element.srcObject === this.#stream) this.element.srcObject = null;
    const observation = this.#observation;
    this.#observation = undefined;
    return (this.#closing = Promise.allSettled([
      this.source.close(),
      observation?.close(),
      this.options.ready,
      ...this.#pending,
    ]).then(() => {}));
  }
}

export function attachRemoteMedia(
  source: VideoReceiver,
  element: HTMLVideoElement,
  options: AttachmentOptions,
): MediaAttachment {
  return new Attachment(source, element, options);
}
