type MediaValue = MediaStreamTrack | MediaStream;
type MediaKind = "track" | "stream";

interface Entry {
  readonly kind: MediaKind;
  readonly value: MediaValue;
}

interface Registry {
  retain(value: MediaValue, kind: MediaKind): bigint;
  get(handle: bigint, kind: MediaKind): MediaValue | undefined;
  release(handle: bigint, kind: MediaKind): boolean;
  size(): bigint;
}

const MAX_HANDLE = (1n << 64n) - 1n;
const values = new Map<bigint, Entry>();
const handles = new WeakMap<MediaValue, bigint>();
let nextHandle = 1n;

function matches(value: unknown, kind: MediaKind): value is MediaValue {
  return kind === "track"
    ? value instanceof MediaStreamTrack
    : value instanceof MediaStream;
}

const registry: Registry = Object.freeze({
  retain(value: MediaValue, kind: MediaKind) {
    if (!matches(value, kind)) throw new TypeError(`expected a MediaStream${kind === "track" ? "Track" : ""}`);
    const current = handles.get(value);
    if (current !== undefined && values.has(current)) return current;
    if (nextHandle > MAX_HANDLE) throw new RangeError("browser media handle space exhausted");
    const handle = nextHandle;
    nextHandle += 1n;
    values.set(handle, { kind, value });
    handles.set(value, handle);
    return handle;
  },
  get(handle: bigint, kind: MediaKind) {
    const entry = values.get(handle);
    return entry !== undefined && entry.kind === kind ? entry.value : undefined;
  },
  release(handle: bigint, kind: MediaKind) {
    const entry = values.get(handle);
    if (entry === undefined || entry.kind !== kind) return false;
    values.delete(handle);
    handles.delete(entry.value);
    return true;
  },
  size() {
    return BigInt(values.size);
  },
});

Object.defineProperty(globalThis, "__pulsebeamMediaRegistry", {
  configurable: false,
  enumerable: false,
  writable: false,
  value: registry,
});

export function lowerMediaStreamTrack(value: MediaStreamTrack): bigint {
  return registry.retain(value, "track");
}

export function liftMediaStreamTrack(handle: bigint): MediaStreamTrack {
  const value = registry.get(handle, "track");
  if (!(value instanceof MediaStreamTrack)) throw new ReferenceError("stale browser media track handle");
  return value;
}

export function lowerMediaStream(value: MediaStream): bigint {
  return registry.retain(value, "stream");
}

export function liftMediaStream(handle: bigint): MediaStream {
  const value = registry.get(handle, "stream");
  if (!(value instanceof MediaStream)) throw new ReferenceError("stale browser media stream handle");
  return value;
}

export function invalidateMediaForTest(value: MediaValue): boolean {
  const handle = handles.get(value);
  const entry = handle === undefined ? undefined : values.get(handle);
  return handle !== undefined && entry !== undefined && registry.release(handle, entry.kind);
}

export function exhaustMediaHandlesForTest(): void {
  nextHandle = MAX_HANDLE + 1n;
}
