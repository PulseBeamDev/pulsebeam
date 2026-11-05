import {
  AudioSource,
  nativeSourceTrack,
  VideoSource,
  watchSource,
} from "./capture-source.js";
import {
  ResourceError,
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
  PublicationConfig,
  SenderConfig,
  VideoSenderConfig,
} from "./types.js";

export interface MediaRuntime extends ResourceRuntime {
  replace_owned_track(
    owner: bigint,
    source: MediaStreamTrack | null,
    config: SenderConfig,
    muted: boolean,
    replaceInput: boolean,
  ): Promise<void>;
  set_owned_muted(owner: bigint, muted: boolean): Promise<void>;
}

type Source = VideoSource | AudioSource;
type ConfigFor<S extends Source> = S extends VideoSource
  ? VideoSenderConfig
  : AudioSenderConfig;
type LocalHandle = LocalVideoHandle | LocalAudioHandle;
const owners = new WeakMap<LocalHandle, LocalMediaRegistry>();
const publications = new WeakMap<object, ResourceOwner>();

class LocalVideoHandle implements LocalVideoTrack {
  readonly kind = "video";
  constructor(readonly label: string) {}
  async publish(
    source: VideoSource,
    configuration: PublicationConfig,
  ): Promise<VideoPublication> {
    return registryFor(this).publish(this, source, configuration);
  }
}
class LocalAudioHandle implements LocalAudioTrack {
  readonly kind = "audio";
  constructor(readonly label: string) {}
  async publish(
    source: AudioSource,
    configuration: PublicationConfig,
  ): Promise<AudioPublication> {
    return registryFor(this).publish(this, source, configuration);
  }
}
function registryFor(handle: LocalHandle): LocalMediaRegistry {
  const registry = owners.get(handle);
  if (!registry) throw new ResourceError("closed", "agent is closed");
  return registry;
}
function validateSource(source: Source, kind: "video" | "audio"): void {
  nativeSourceTrack(source, kind);
  if (source.isClosed())
    throw new ResourceError("closed", "media source is closed");
}
function validateConfiguration(configuration: PublicationConfig): void {
  if (typeof configuration.enabled !== "boolean")
    throw new TypeError("enabled must be a boolean");
}

class Publication<S extends Source> {
  readonly #resource: ResourceOwner;
  #source: S | null;
  #sourceRevision = {};
  #enabled: boolean;
  #config: SenderConfig;
  #unwatch: (() => void) | undefined;
  readonly #pending = new Set<Promise<void>>();
  #closing: Promise<void> | undefined;

  constructor(
    resources: ResourceOwners,
    readonly handle: LocalHandle,
    source: S,
    configuration: PublicationConfig,
  ) {
    validateSource(source, handle.kind);
    validateConfiguration(configuration);
    this.#source = source;
    this.#enabled = configuration.enabled;
    this.#config =
      handle.kind === "video"
        ? { contentHint: "motion" }
        : { contentHint: "speech" };
    this.#resource = resources.acquire(
      {
        op: "claimPublication",
        spec: { kind: handle.kind, label: handle.label },
      },
      {
        bind: () => {
          void this.#replace(true).catch(() => {});
        },
        unbind: () => {
          if (this.#resource?.isClosed()) {
            this.#unwatch?.();
            this.#unwatch = undefined;
            this.#source = null;
          }
        },
      },
    );
    publications.set(this, this.#resource);
    if (!this.#resource.isClosed()) this.#watchSource();
  }
  isClosed(): boolean {
    return this.#resource.isClosed();
  }
  #assertOpen(): void {
    if (this.isClosed())
      throw new ResourceError("closed", "publication is closed");
  }
  async setSource(source: S | null): Promise<void> {
    this.#assertOpen();
    if (source) validateSource(source, this.handle.kind);
    if (source === this.#source) return;
    this.#unwatch?.();
    this.#unwatch = undefined;
    this.#source = source;
    this.#sourceRevision = {};
    this.#watchSource();
    await this.#replace(true);
  }
  async configure(configuration: PublicationConfig): Promise<void> {
    this.#assertOpen();
    validateConfiguration(configuration);
    this.#enabled = configuration.enabled;
    const binding = this.#resource.binding;
    if (binding && this.#resource.current(binding)) {
      await this.#operation(binding, () =>
        (binding.runtime as MediaRuntime).set_owned_muted(
          binding.owner,
          !configuration.enabled,
        ),
      );
    }
  }
  async configureSender(configuration: ConfigFor<S>): Promise<void> {
    this.#assertOpen();
    this.#config = configuration;
    await this.#replace(false);
  }
  #replace(replaceInput: boolean): Promise<void> {
    const binding = this.#resource.binding;
    if (!binding || !this.#resource.current(binding)) return Promise.resolve();
    const source = this.#source;
    const track =
      source && !source.isClosed()
        ? nativeSourceTrack(source, this.handle.kind)
        : null;
    const configuration = this.#config;
    const muted = !this.#enabled;
    return this.#operation(binding, () =>
      (binding.runtime as MediaRuntime).replace_owned_track(
        binding.owner,
        track,
        configuration,
        muted,
        replaceInput,
      ),
    );
  }
  #operation(
    binding: ResourceBinding,
    operation: () => Promise<void>,
  ): Promise<void> {
    // Host validation can reject ended input before Core observes supersession.
    const sourceRevision = this.#sourceRevision;
    const current = () =>
      sourceRevision === this.#sourceRevision &&
      this.#resource.current(binding);
    const pending = Promise.resolve()
      .then(() => {
        if (current()) return operation();
      })
      .catch((error: unknown) => {
        if (!current()) return;
        this.#resource.fail(error);
        throw error;
      });
    this.#pending.add(pending);
    void pending.then(
      () => this.#pending.delete(pending),
      () => this.#pending.delete(pending),
    );
    return pending;
  }
  #watchSource(): void {
    const source = this.#source;
    if (!source) return;
    this.#unwatch = watchSource(source, () => {
      if (source === this.#source) void this.setSource(null).catch(() => {});
    });
  }
  close(): Promise<void> {
    if (this.#closing) return this.#closing;
    this.#unwatch?.();
    this.#unwatch = undefined;
    this.#source = null;
    const release = this.#resource.close();
    return (this.#closing = Promise.allSettled([
      release,
      ...this.#pending,
    ]).then(() => {}));
  }
}

export class VideoPublication extends Publication<VideoSource> {}
export class AudioPublication extends Publication<AudioSource> {}

// React must retain the owner before readiness to retire a pending claim on unmount.
export function claimPublication(
  handle: LocalVideoTrack,
  source: VideoSource,
  configuration: PublicationConfig,
): { publication: VideoPublication; ready: Promise<void> };
export function claimPublication(
  handle: LocalAudioTrack,
  source: AudioSource,
  configuration: PublicationConfig,
): { publication: AudioPublication; ready: Promise<void> };
export function claimPublication(
  handle: LocalHandle,
  source: Source,
  configuration: PublicationConfig,
): { publication: VideoPublication | AudioPublication; ready: Promise<void> } {
  const publication = registryFor(handle).claim(handle, source, configuration);
  return { publication, ready: publications.get(publication)!.ready };
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
    if (this.#closed) throw new ResourceError("closed", "agent is closed");
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
  publish(
    handle: LocalVideoHandle,
    source: VideoSource,
    configuration: PublicationConfig,
  ): Promise<VideoPublication>;
  publish(
    handle: LocalAudioHandle,
    source: AudioSource,
    configuration: PublicationConfig,
  ): Promise<AudioPublication>;
  async publish(
    handle: LocalHandle,
    source: Source,
    configuration: PublicationConfig,
  ): Promise<VideoPublication | AudioPublication> {
    const publication = this.claim(handle, source, configuration);
    try {
      await publications.get(publication)!.ready;
      return publication;
    } catch (error) {
      await publication.close();
      throw error;
    }
  }
  claim(
    handle: LocalHandle,
    source: Source,
    configuration: PublicationConfig,
  ): VideoPublication | AudioPublication {
    if (this.#closed || owners.get(handle) !== this)
      throw new ResourceError("closed", "agent is closed");
    return handle.kind === "video"
      ? new VideoPublication(
          this.resources,
          handle,
          source as VideoSource,
          configuration,
        )
      : new AudioPublication(
          this.resources,
          handle,
          source as AudioSource,
          configuration,
        );
  }
  close(): void {
    if (this.#closed) return;
    this.#closed = true;
    for (const handle of this.#handles.values()) owners.delete(handle);
    this.#handles.clear();
  }
}
