import {
  AudioSource,
  nativeSourceTrack,
  pipeSource,
  VideoSource,
  watchSource,
} from "./capture-source.js";
import { OperationError, raise } from "./agent-observation.js";
import { PublicationObservation } from "./publication-observation.js";
import type { BrowserRuntime } from "./wasm.js";
import {
  ErrorCode,
  PipeStatus,
  type PipeState,
  type PublicationState,
} from "./schema.js";
import {
  ResourceError,
  ResourceOwner,
  ResourceOwners,
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
  PipeUpdates,
  PublicationConfig,
  PublicationUpdates,
  SenderConfig,
  VideoSenderConfig,
} from "./types.js";

type Runtime = InstanceType<typeof BrowserRuntime>;
type NativePipe = ReturnType<Runtime["create_owned_publication_pipe"]>;
export interface MediaRuntime extends ResourceRuntime {
  create_owned_publication_pipe(owner: bigint): NativePipe;
  configure_publication(owner: bigint, enabled: boolean): void;
}
type Source = VideoSource | AudioSource;
type ConfigFor<S extends Source> = S extends VideoSource
  ? VideoSenderConfig
  : AudioSenderConfig;
type LocalHandle = LocalVideoHandle | LocalAudioHandle;
const owners = new WeakMap<LocalHandle, LocalMediaRegistry>();
const configurePipe = Symbol("sender configuration");

class LocalVideoHandle implements LocalVideoTrack {
  readonly kind = "video";
  constructor(readonly label: string) {}
  publish(
    configuration: PublicationConfig = { enabled: true },
  ): VideoPublication {
    return registryFor(this).claim(this, configuration) as VideoPublication;
  }
}
class LocalAudioHandle implements LocalAudioTrack {
  readonly kind = "audio";
  constructor(readonly label: string) {}
  publish(
    configuration: PublicationConfig = { enabled: true },
  ): AudioPublication {
    return registryFor(this).claim(this, configuration) as AudioPublication;
  }
}
function registryFor(handle: LocalHandle): LocalMediaRegistry {
  const registry = owners.get(handle);
  if (!registry) throw new ResourceError("closed", "agent is closed");
  return registry;
}
function validateConfiguration(configuration: PublicationConfig): void {
  if (typeof configuration.enabled !== "boolean")
    throw new OperationError({
      code: ErrorCode.InvalidArgument,
      message: "enabled must be a boolean",
    });
}

export class Pipe {
  #value: Readonly<PipeState> | undefined;
  #unwatch: (() => void) | undefined;
  #closing: Promise<void> | undefined;
  readonly #pending = new Set<Promise<void>>();
  constructor(
    private readonly native: NativePipe,
    private readonly source: Source,
    private readonly retired: (closing: Promise<void>) => void,
    configuration: SenderConfig,
  ) {
    this.#unwatch = watchSource(
      source,
      () => void this.close().catch(() => {}),
    );
    void this[configurePipe](configuration).catch(() => {});
  }
  isClosed(): boolean {
    return this.state().status === PipeStatus.Closed;
  }
  #accept(value: PipeState): Readonly<PipeState> {
    if (this.#value?.observationRevision === value.observationRevision)
      return this.#value;
    const next = Object.freeze({
      ...value,
      error: value.error ? Object.freeze({ ...value.error }) : undefined,
    });
    if (
      !this.#value ||
      value.observationRevision > this.#value.observationRevision
    )
      this.#value = next;
    return next;
  }
  state(): Readonly<PipeState> {
    return this.#accept(this.native.state() as PipeState);
  }
  updates(): PipeUpdates {
    this.state();
    try {
      const cursor = this.native.updates();
      return {
        next: async () => {
          try {
            const value = (await cursor.next()) as PipeState | undefined;
            return value === undefined ? undefined : this.#accept(value);
          } catch (error) {
            return raise(error);
          }
        },
        close: () => cursor.close(),
      };
    } catch (error) {
      return raise(error);
    }
  }
  [configurePipe](configuration: SenderConfig): Promise<void> {
    const pending = this.native
      .install_track(
        nativeSourceTrack(this.source, this.source.kind),
        configuration,
      )
      .catch((error: unknown) => raise(error));
    this.#pending.add(pending);
    void pending.then(
      () => this.#pending.delete(pending),
      () => this.#pending.delete(pending),
    );
    return pending;
  }
  close(): Promise<void> {
    if (this.#closing) return this.#closing;
    this.#unwatch?.();
    this.#unwatch = undefined;
    try {
      const closing = this.native.close();
      this.#closing = Promise.all([
        closing,
        Promise.allSettled([...this.#pending]),
      ]).then(
        () => {},
        (error: unknown) => raise(error),
      );
      this.retired(this.#closing);
      return this.#closing;
    } catch (error) {
      return raise(error);
    }
  }
}

class Publication<S extends Source> {
  readonly #resource: ResourceOwner;
  readonly #observation = new PublicationObservation();
  #pipe: Pipe | undefined;
  #config: SenderConfig;
  #configuration: PublicationConfig;
  readonly #pending = new Set<Promise<void>>();
  #closing: Promise<void> | undefined;
  constructor(
    resources: ResourceOwners,
    readonly handle: LocalHandle,
    configuration: PublicationConfig,
  ) {
    validateConfiguration(configuration);
    this.#configuration = { ...configuration };
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
        bind: (binding) => {
          const runtime = binding.runtime as MediaRuntime;
          runtime.configure_publication(
            binding.owner,
            this.#configuration.enabled,
          );
          this.#observation.bind(runtime as Runtime, binding.owner);
        },
        unbind: () => {
          void this.#pipe?.close().catch(() => {});
        },
      },
    );
  }
  isClosed(): boolean {
    return this.#resource.isClosed();
  }
  state(): Readonly<PublicationState> {
    return this.#observation.state();
  }
  updates(): PublicationUpdates {
    return this.#observation.updates();
  }
  #binding() {
    if (this.isClosed())
      throw new ResourceError("closed", "publication is closed");
    const binding = this.#resource.binding;
    if (!binding || !this.#resource.current(binding))
      throw new ResourceError("unavailable", "publication runtime is inactive");
    return binding;
  }
  [pipeSource](source: S): Pipe {
    const native = nativeSourceTrack(source, this.handle.kind);
    if (source.isClosed() || native.readyState !== "live")
      throw new ResourceError("closed", "media source is closed");
    const binding = this.#binding();
    try {
      const input = (
        binding.runtime as MediaRuntime
      ).create_owned_publication_pipe(binding.owner);
      const pipe = new Pipe(
        input,
        source,
        (closing) => {
          if (this.#pipe === pipe) this.#pipe = undefined;
          this.#pending.add(closing);
          void closing.then(
            () => this.#pending.delete(closing),
            () => this.#pending.delete(closing),
          );
        },
        this.#config,
      );
      const previous = this.#pipe;
      this.#pipe = pipe;
      void previous?.close().catch(() => {});
      return pipe;
    } catch (error) {
      return raise(error);
    }
  }
  configure(configuration: PublicationConfig): void {
    validateConfiguration(configuration);
    const binding = this.#binding();
    try {
      (binding.runtime as MediaRuntime).configure_publication(
        binding.owner,
        configuration.enabled,
      );
      this.#configuration = { ...configuration };
    } catch (error) {
      return raise(error);
    }
  }
  async configureSender(configuration: ConfigFor<S>): Promise<void> {
    this.#binding();
    this.#config = { ...configuration };
    await this.#pipe?.[configurePipe](this.#config);
  }
  close(): Promise<void> {
    if (this.#closing) return this.#closing;
    let pipe: Promise<void> | undefined;
    try {
      pipe = this.#pipe?.close();
    } catch (error) {
      pipe = Promise.reject(error);
    }
    const release = this.#resource.close();
    return (this.#closing = Promise.all([pipe, release, ...this.#pending]).then(
      () => {},
    ));
  }
}
export class VideoPublication extends Publication<VideoSource> {}
export class AudioPublication extends Publication<AudioSource> {}

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
  claim(
    handle: LocalHandle,
    configuration: PublicationConfig,
  ): VideoPublication | AudioPublication {
    if (this.#closed || owners.get(handle) !== this)
      throw new ResourceError("closed", "agent is closed");
    return handle.kind === "video"
      ? new VideoPublication(this.resources, handle, configuration)
      : new AudioPublication(this.resources, handle, configuration);
  }
  close(): void {
    this.#closed = true;
  }
}
