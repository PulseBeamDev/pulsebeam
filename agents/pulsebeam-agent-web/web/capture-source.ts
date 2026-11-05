import { ResourceError } from "./resources.js";

type Source = VideoSource | AudioSource;
interface SourceState {
  readonly track: MediaStreamTrack;
  readonly listeners: Set<() => void>;
  readonly ended: () => void;
  closed: boolean;
}
const sources = new WeakMap<Source, SourceState>();

function initialize(source: Source, track: MediaStreamTrack): void {
  if (track.kind !== source.kind)
    throw new TypeError("source track kind mismatch");
  if (track.readyState !== "live")
    throw new ResourceError("closed", "source track is ended");
  const state: SourceState = {
    track,
    listeners: new Set(),
    ended: () => retire(state),
    closed: false,
  };
  sources.set(source, state);
  track.addEventListener("ended", state.ended, { once: true });
}

function stateOf(source: Source): SourceState {
  const state = sources.get(source);
  if (!state) throw new TypeError("invalid media source");
  return state;
}
function retire(state: SourceState): void {
  if (state.closed) return;
  state.closed = true;
  state.track.removeEventListener("ended", state.ended);
  const listeners = [...state.listeners];
  state.listeners.clear();
  for (const listener of listeners) listener();
}
function closed(source: Source): boolean {
  const state = stateOf(source);
  return state.closed || state.track.readyState !== "live";
}
async function close(source: Source): Promise<void> {
  const state = stateOf(source);
  if (state.closed) return;
  state.track.stop();
  retire(state);
}

export class VideoSource {
  readonly kind = "video";
  private constructor(track: MediaStreamTrack) {
    initialize(this, track);
  }
  static fromTrack(track: MediaStreamTrack): VideoSource {
    return new VideoSource(track);
  }
  isClosed(): boolean {
    return closed(this);
  }
  close(): Promise<void> {
    return close(this);
  }
}

export class AudioSource {
  readonly kind = "audio";
  private constructor(track: MediaStreamTrack) {
    initialize(this, track);
  }
  static fromTrack(track: MediaStreamTrack): AudioSource {
    return new AudioSource(track);
  }
  isClosed(): boolean {
    return closed(this);
  }
  close(): Promise<void> {
    return close(this);
  }
}

export function nativeSourceTrack(
  source: Source,
  kind: "video" | "audio",
): MediaStreamTrack {
  const state = stateOf(source);
  if (source.kind !== kind || state.track.kind !== kind)
    throw new TypeError("source track kind mismatch");
  return state.track;
}

export function watchSource(source: Source, listener: () => void): () => void {
  const state = stateOf(source);
  if (closed(source)) {
    listener();
    return () => {};
  }
  state.listeners.add(listener);
  return () => state.listeners.delete(listener);
}
