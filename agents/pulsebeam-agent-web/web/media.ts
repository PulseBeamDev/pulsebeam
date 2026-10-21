import {
  nativeCaptureTrack,
  type CapturedAudioTrack,
  type CapturedTrack,
  type CapturedVideoTrack,
} from "./capture-source.js";
import {
  ResourceOwner,
  ResourceOwners,
  type ResourceBinding,
  type ResourceRuntime,
} from "./resources.js";
import {
  track,
  trackKey,
  type AudioTrackSpec,
  type VideoTrackSpec,
} from "./track.js";
import type {
  AudioSenderConfig,
  LocalAudioTrack,
  LocalVideoTrack,
  SenderConfig,
  VideoSenderConfig,
} from "./types.js";

export interface MediaRuntime extends ResourceRuntime {
  replace_owned_track(
    owner: bigint,
    source: MediaStreamTrack | null,
    config: SenderConfig,
  ): Promise<void>;
  set_owned_muted(owner: bigint, muted: boolean): Promise<void>;
}

type ConfigFor<S extends CapturedTrack> = S extends CapturedVideoTrack
  ? VideoSenderConfig
  : S extends CapturedAudioTrack
    ? AudioSenderConfig
    : SenderConfig;

export interface LocalMediaAttachment<S extends CapturedTrack = CapturedTrack> {
  readonly ready: Promise<void>;
  setSource(source: S | null): void;
  setEnabled(enabled: boolean): void;
  setConfig(config: ConfigFor<S>): void;
  close(): Promise<void>;
}

type LocalHandle = LocalVideoHandle | LocalAudioHandle;
const owners = new WeakMap<LocalHandle, LocalMediaRegistry>();

abstract class LocalHandleBase<
  K extends "video" | "audio",
  S extends CapturedTrack,
> {
  readonly kind: K;
  readonly label: string;
  #source: S | null = null;
  #enabled = true;
  #headless: LocalMediaAttachment<S> | undefined;
  readonly #listeners = new Set<() => void>();

  constructor(kind: K, label: string) {
    this.kind = kind;
    this.label = label;
  }
  get source(): S | null {
    return this.#source;
  }
  get enabled(): boolean {
    return this.#enabled;
  }
  setSource(source: S | null): void {
    if (source === this.#source) return;
    const registry = owners.get(this as LocalHandle);
    if (!registry) throw new Error("agent is closed");
    if (source && source.kind !== this.kind)
      throw new TypeError("capture track kind mismatch");
    this.#source = source;
    if (source || this.#headless) {
      this.#headless ??= registry.attach(this as LocalHandle, {
        source: this.#source,
        enabled: this.#enabled,
      }) as LocalMediaAttachment<S>;
      this.#headless.setSource(source);
    }
    this.#emit();
  }
  setEnabled(enabled: boolean): void {
    if (typeof enabled !== "boolean")
      throw new TypeError("enabled must be a boolean");
    if (enabled === this.#enabled) return;
    if (!owners.has(this as LocalHandle)) throw new Error("agent is closed");
    this.#enabled = enabled;
    this.#headless?.setEnabled(enabled);
    this.#emit();
  }
  subscribe(listener: () => void): () => void {
    if (!owners.has(this as LocalHandle)) return () => {};
    this.#listeners.add(listener);
    return () => this.#listeners.delete(listener);
  }
  close(): void {
    owners.delete(this as LocalHandle);
    void this.#headless?.close();
    this.#headless = undefined;
    this.#source = null;
    this.#emit();
    this.#listeners.clear();
  }
  #emit(): void {
    for (const listener of this.#listeners) listener();
  }
}

class LocalVideoHandle
  extends LocalHandleBase<"video", CapturedVideoTrack>
  implements LocalVideoTrack
{
  constructor(label: string) {
    super("video", label);
  }
}
class LocalAudioHandle
  extends LocalHandleBase<"audio", CapturedAudioTrack>
  implements LocalAudioTrack
{
  constructor(label: string) {
    super("audio", label);
  }
}

export interface LocalMediaAttachmentOptions<S extends CapturedTrack> {
  readonly source?: S | null;
  readonly enabled?: boolean;
  readonly config?: ConfigFor<S>;
  readonly onError?: (error: unknown) => void;
}

class Attachment<S extends CapturedTrack> implements LocalMediaAttachment<S> {
  readonly ready: Promise<void>;
  #resource: ResourceOwner;
  #binding: ResourceBinding | undefined;
  #source: S | null;
  #enabled: boolean;
  #config: SenderConfig;
  #revision = 0;
  #closed = false;
  #ended: (() => void) | undefined;

  constructor(
    resources: ResourceOwners,
    readonly handle: LocalHandle,
    options: LocalMediaAttachmentOptions<S>,
  ) {
    this.#source = options.source ?? null;
    this.#enabled = options.enabled ?? true;
    this.#config =
      options.config ??
      (handle.kind === "video"
        ? { contentHint: "motion" }
        : { contentHint: "speech" });
    this.#resource = resources.acquire(
      {
        op: "claimPublication",
        spec: { kind: handle.kind, label: handle.label },
      },
      {
        bind: (binding) => {
          this.#binding = binding;
          this.#reconcile();
        },
        unbind: () => {
          this.#binding = undefined;
        },
        error: options.onError,
      },
    );
    this.ready = this.#resource.ready;
    this.#watchSource();
  }
  setSource(source: S | null): void {
    if (this.#closed || source === this.#source) return;
    if (source && source.kind !== this.handle.kind)
      throw new TypeError("capture track kind mismatch");
    this.#unwatchSource();
    this.#source = source;
    this.#watchSource();
    this.#reconcile();
  }
  setEnabled(enabled: boolean): void {
    if (this.#closed || enabled === this.#enabled) return;
    if (typeof enabled !== "boolean")
      throw new TypeError("enabled must be a boolean");
    this.#enabled = enabled;
    const binding = this.#binding;
    if (binding && this.#resource.current(binding)) {
      void (binding.runtime as MediaRuntime)
        .set_owned_muted(binding.owner, !enabled)
        .catch((error) => this.#fail(binding, error));
    }
  }
  setConfig(config: SenderConfig): void {
    if (this.#closed) return;
    this.#config = config;
    this.#reconcile();
  }
  #reconcile(): void {
    const binding = this.#binding;
    if (!binding || !this.#resource.current(binding)) return;
    const revision = ++this.#revision;
    const runtime = binding.runtime as MediaRuntime;
    const source = this.#source;
    const present =
      source !== null &&
      nativeCaptureTrack(source, this.handle.kind).readyState === "live";
    const apply = async () => {
      if (!present)
        await runtime.resource({
          op: "updatePublication",
          owner: binding.owner,
          present: false,
        });
      await runtime.replace_owned_track(
        binding.owner,
        present ? nativeCaptureTrack(source, this.handle.kind) : null,
        this.#config,
      );
      if (!this.#resource.current(binding) || revision !== this.#revision)
        return;
      await runtime.set_owned_muted(binding.owner, !this.#enabled);
      if (
        present &&
        this.#resource.current(binding) &&
        revision === this.#revision
      ) {
        await runtime.resource({
          op: "updatePublication",
          owner: binding.owner,
          present: true,
        });
      }
    };
    void apply().catch((error) => this.#fail(binding, error));
  }
  #fail(binding: ResourceBinding, error: unknown): void {
    if (this.#resource.current(binding)) this.#resource.fail(error);
  }
  #watchSource(): void {
    const source = this.#source;
    if (!source) return;
    const track = nativeCaptureTrack(source, this.handle.kind);
    const ended = () => {
      if (source === this.#source) this.setSource(null);
    };
    track.addEventListener("ended", ended, { once: true });
    this.#ended = () => track.removeEventListener("ended", ended);
  }
  #unwatchSource(): void {
    this.#ended?.();
    this.#ended = undefined;
  }
  close(): Promise<void> {
    if (this.#closed) return this.#resource.close();
    this.#closed = true;
    ++this.#revision;
    this.#unwatchSource();
    this.#binding = undefined;
    return this.#resource.close();
  }
}

export class LocalMediaRegistry {
  readonly #handles = new Map<string, LocalHandle>();
  #closed = false;
  constructor(readonly resources: ResourceOwners) {}

  track(spec: VideoTrackSpec): LocalVideoTrack;
  track(spec: AudioTrackSpec): LocalAudioTrack;
  track(
    spec: VideoTrackSpec | AudioTrackSpec,
  ): LocalVideoTrack | LocalAudioTrack {
    if (this.#closed) throw new Error("agent is closed");
    const normalized =
      spec.kind === "video" ? track.video(spec.label) : track.audio(spec.label);
    const key = trackKey(normalized);
    let handle = this.#handles.get(key);
    if (!handle) {
      handle =
        normalized.kind === "video"
          ? new LocalVideoHandle(normalized.label)
          : new LocalAudioHandle(normalized.label);
      this.#handles.set(key, handle);
      owners.set(handle, this);
    }
    return handle;
  }
  attach<S extends CapturedTrack>(
    handle: LocalHandle,
    options: LocalMediaAttachmentOptions<S> = {},
  ): LocalMediaAttachment<S> {
    if (this.#closed || owners.get(handle) !== this)
      throw new TypeError("invalid local track handle");
    return new Attachment(this.resources, handle, options);
  }
  close(): void {
    if (this.#closed) return;
    this.#closed = true;
    for (const handle of this.#handles.values()) handle.close();
    this.#handles.clear();
  }
}

export function attachLocalTrack(
  handle: LocalVideoTrack,
  options?: LocalMediaAttachmentOptions<CapturedVideoTrack>,
): LocalMediaAttachment<CapturedVideoTrack>;
export function attachLocalTrack(
  handle: LocalAudioTrack,
  options?: LocalMediaAttachmentOptions<CapturedAudioTrack>,
): LocalMediaAttachment<CapturedAudioTrack>;
export function attachLocalTrack(
  handle: LocalVideoTrack | LocalAudioTrack,
  options: LocalMediaAttachmentOptions<CapturedTrack> = {},
): LocalMediaAttachment<CapturedTrack> {
  const registry = owners.get(handle as LocalHandle);
  if (!registry) throw new TypeError("invalid local track handle");
  return registry.attach(handle as LocalHandle, options);
}
