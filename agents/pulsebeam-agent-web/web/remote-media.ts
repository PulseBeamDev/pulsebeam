import type {
  Agent,
  PlaybackFailure,
  RemoteMediaAttachment,
  RemoteMediaAttachmentOptions,
} from "./types.js";

function normalizePublicationIds(
  publicationIds: readonly string[],
): ReadonlySet<string> {
  return new Set(publicationIds);
}

function samePublicationIds(
  left: ReadonlySet<string>,
  right: ReadonlySet<string>,
): boolean {
  return left.size === right.size && [...left].every((id) => right.has(id));
}

function failure(error: unknown): PlaybackFailure {
  return Object.freeze({
    message: error instanceof Error ? error.message : String(error),
    cause: error,
  });
}

function isAbortError(error: unknown): boolean {
  return error instanceof DOMException && error.name === "AbortError";
}

class Attachment implements RemoteMediaAttachment {
  #publicationIds: ReadonlySet<string>;
  #tracks = new Set<MediaStreamTrack>();
  #closed = false;
  #playAttempt = 0;
  readonly #agent: Agent;
  readonly #element: HTMLMediaElement;
  readonly #onPlaybackBlocked: RemoteMediaAttachmentOptions["onPlaybackBlocked"];
  readonly #stream = new MediaStream();
  readonly #unsubscribe: () => void;

  constructor(
    agent: Agent,
    element: HTMLMediaElement,
    onPlaybackBlocked: RemoteMediaAttachmentOptions["onPlaybackBlocked"],
    publicationIds: readonly string[],
  ) {
    this.#agent = agent;
    this.#element = element;
    this.#onPlaybackBlocked = onPlaybackBlocked;
    this.#publicationIds = normalizePublicationIds(publicationIds);
    this.#element.srcObject = this.#stream;
    this.#unsubscribe = this.#agent.subscribe(this.#reconcile);
    this.#reconcile();
  }

  setPublicationIds(publicationIds: readonly string[]): void {
    if (this.#closed) return;
    const normalized = normalizePublicationIds(publicationIds);
    if (samePublicationIds(this.#publicationIds, normalized)) return;
    this.#publicationIds = normalized;
    this.#reconcile();
  }

  retryPlayback(): Promise<void> {
    return this.#play();
  }

  close(): void {
    if (this.#closed) return;
    this.#closed = true;
    this.#playAttempt += 1;
    this.#unsubscribe();
    for (const track of this.#tracks) this.#stream.removeTrack(track);
    this.#tracks.clear();
    if (this.#element.srcObject === this.#stream)
      this.#element.srcObject = null;
  }

  readonly #reconcile = (): void => {
    if (this.#closed) return;
    const next = new Set<MediaStreamTrack>();
    const tracks = this.#agent.getSnapshot().tracks;
    for (const publicationId of this.#publicationIds) {
      const track = tracks[publicationId]?.media;
      if (track) next.add(track);
    }
    let changed = false;
    for (const track of this.#tracks) {
      if (!next.has(track)) {
        this.#stream.removeTrack(track);
        changed = true;
      }
    }
    for (const track of next) {
      if (!this.#tracks.has(track)) {
        this.#stream.addTrack(track);
        changed = true;
      }
    }
    this.#tracks = next;
    if (changed && next.size > 0) void this.#play();
  };

  #play(): Promise<void> {
    if (this.#closed || this.#tracks.size === 0) return Promise.resolve();
    const attempt = this.#playAttempt + 1;
    this.#playAttempt = attempt;
    return this.#element.play().catch((error: unknown) => {
      if (
        this.#closed ||
        attempt !== this.#playAttempt ||
        isAbortError(error)
      ) {
        return;
      }
      this.#onPlaybackBlocked?.(failure(error), () => this.#play());
    });
  }
}

export function attachRemoteMedia(
  agent: Agent,
  element: HTMLMediaElement,
  options: RemoteMediaAttachmentOptions,
): RemoteMediaAttachment {
  return new Attachment(
    agent,
    element,
    options.onPlaybackBlocked,
    options.publicationIds,
  );
}
