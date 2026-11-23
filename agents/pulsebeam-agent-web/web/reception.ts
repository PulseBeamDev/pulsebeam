import { observeVideoGeometry, type VideoGeometry } from "./video-geometry.js";
import {
  ResourceError,
  ResourceOwner,
  ResourceOwners,
  resourceFailure,
  type ResourceBinding,
  type ResourceRuntime,
} from "./resources.js";
import {
  ResourceObservation,
  type NativeResourceObserver,
} from "./resource-observation.js";
import type {
  AudioSubscriptionState,
  VideoSubscriptionState,
} from "./schema.js";
import type {
  AudioSubscriptionUpdates,
  VideoSubscriptionUpdates,
  AudioSubscriptionConfig,
  TrackSelector,
  VideoObservation,
} from "./types.js";

export const defaultVideoObservation = Object.freeze<VideoObservation>({
  visible: true,
  height: 0,
  minHeight: 0,
  minFps: 0,
  priority: 0,
  playoutDelay: Object.freeze({ mode: "adaptive" }),
});
export const defaultAudioSubscription = Object.freeze<AudioSubscriptionConfig>({
  playoutDelay: Object.freeze({ mode: "adaptive" }),
});

interface NativeObservation {
  track(): MediaStreamTrack | undefined;
  is_closed(): boolean;
  close(): Promise<void>;
  free(): void;
}
interface ReceptionRuntime extends ResourceRuntime {
  video_subscription(owner: bigint): NativeResourceObserver;
  audio_subscription(owner: bigint): NativeResourceObserver;
  observe_media(
    owner: bigint,
    source?: string,
    signal?: AbortSignal,
  ): Promise<NativeObservation>;
}

export class MediaObservation {
  #closed = false;
  #closing: Promise<void> | undefined;
  constructor(
    private readonly native: NativeObservation,
    private readonly retired: () => void,
  ) {}
  isClosed(): boolean {
    return this.#closed || this.native.is_closed();
  }
  track(): MediaStreamTrack | null {
    return this.isClosed() ? null : (this.native.track() ?? null);
  }
  close(): Promise<void> {
    if (this.#closing) return this.#closing;
    this.#closed = true;
    return (this.#closing = this.native.close().finally(() => {
      this.native.free();
      this.retired();
    }));
  }
}

const initialized = new WeakMap<object, Promise<void>>();
type Configure<C> = (owner: bigint, configuration: C) => unknown;
class Subscription<C> {
  readonly #resource: ResourceOwner;
  readonly #pending = new Set<Promise<unknown>>();
  readonly #observations = new Set<MediaObservation>();
  #configuration: C;
  #closing: Promise<void> | undefined;

  constructor(
    resources: ResourceOwners,
    claim: (configuration: C) => unknown,
    configuration: C,
    private readonly command: Configure<C>,
    bind: (binding: ResourceBinding) => void = () => {},
  ) {
    this.#configuration = configuration;
    this.#resource = resources.acquire(() => claim(this.#configuration), {
      bind: (binding) => {
        binding.runtime.resource_now(
          this.command(binding.owner, this.#configuration),
        );
        bind(binding);
      },
      unbind: () => {},
    });
    const ready = this.#resource.ready.then(() => this.#assertOpen());
    void ready.catch(() => {});
    initialized.set(this, ready);
  }
  isClosed(): boolean {
    return this.#resource.isClosed();
  }
  #assertOpen(): void {
    if (this.isClosed())
      throw new ResourceError("closed", "subscription is closed");
  }
  #track<T>(promise: Promise<T>): Promise<T> {
    this.#pending.add(promise);
    void promise.then(
      () => this.#pending.delete(promise),
      () => this.#pending.delete(promise),
    );
    return promise;
  }
  #apply(binding: ResourceBinding, configuration: C): Promise<void> {
    return this.#track(
      (async () => {
        try {
          await binding.runtime.resource(
            this.command(binding.owner, configuration),
          );
        } catch (error) {
          throw resourceFailure(error);
        }
      })(),
    );
  }
  async configure(configuration: C): Promise<void> {
    this.#assertOpen();
    const prior = this.#configuration;
    this.#configuration = configuration;
    const binding = this.#resource.binding;
    if (!binding || !this.#resource.current(binding)) return;
    try {
      await this.#apply(binding, configuration);
    } catch (error) {
      if (
        this.#resource.current(binding) &&
        this.#configuration === configuration
      )
        this.#configuration = prior;
      throw error;
    }
  }
  protected async observeSource(
    source?: string,
    signal?: AbortSignal,
  ): Promise<MediaObservation> {
    this.#assertOpen();
    const binding = this.#resource.binding;
    if (!binding || !this.#resource.current(binding))
      throw new ResourceError(
        "unavailable",
        "subscription runtime is inactive",
      );
    const acquiring = (async () => {
      const native = await (binding.runtime as ReceptionRuntime).observe_media(
        binding.owner,
        source,
        signal,
      );
      const observation = new MediaObservation(native, () =>
        this.#observations.delete(observation),
      );
      if (!this.#resource.current(binding) || observation.isClosed()) {
        await observation.close();
        throw new ResourceError("closed", "media observation was retired");
      }
      this.#observations.add(observation);
      return observation;
    })().catch((error: unknown) => {
      throw resourceFailure(error);
    });
    return this.#track(acquiring);
  }
  close(): Promise<void> {
    if (this.#closing) return this.#closing;
    const release = this.#resource.close();
    return (this.#closing = Promise.allSettled([
      release,
      ...[...this.#observations].map((observation) => observation.close()),
      ...this.#pending,
    ]).then(() => {}));
  }
}

export function videoSubscriptionInitialized(
  subscription: VideoSubscription,
): Promise<void> {
  return initialized.get(subscription)!;
}

export function subscribeVideo(
  resources: ResourceOwners,
  selector: TrackSelector,
  configuration: VideoObservation,
): VideoSubscription {
  return new VideoSubscription(resources, selector, configuration);
}
export function subscribeAudio(
  resources: ResourceOwners,
  selector: TrackSelector,
  configuration: AudioSubscriptionConfig,
): AudioSubscription {
  return new AudioSubscription(resources, selector, configuration);
}
type ViewBinding = (
  element: HTMLVideoElement,
  failed: (error: unknown) => void,
) => () => void;
const videoViews = new WeakMap<object, ViewBinding>();

export function bindVideoView(
  source: object,
  element: HTMLVideoElement,
  failed: (error: unknown) => void,
): () => void {
  return videoViews.get(source)?.(element, failed) ?? (() => {});
}

export class VideoSubscription extends Subscription<VideoObservation> {
  readonly #observation: ResourceObservation<VideoSubscriptionState>;
  readonly #views = new Map<object, VideoGeometry>();
  readonly #viewClosers = new Set<() => void>();
  #requested: VideoObservation;
  #retiringViews = false;
  constructor(
    resources: ResourceOwners,
    selector: TrackSelector,
    configuration: VideoObservation,
  ) {
    const observation = new ResourceObservation<VideoSubscriptionState>();
    super(
      resources,
      () => ({ op: "claimVideo", selector }),
      configuration,
      (owner, observation) => ({ op: "observeVideo", owner, observation }),
      (binding) =>
        observation.bind(
          (binding.runtime as ReceptionRuntime).video_subscription(
            binding.owner,
          ),
        ),
    );
    this.#observation = observation;
    this.#requested = configuration;
    videoViews.set(this, (element, failed) => {
      const key = {};
      let closed = false;
      const apply = () => {
        if (this.isClosed() || this.#retiringViews) return;
        void this.#applyViews().catch((error: unknown) => {
          if (!closed && !this.isClosed()) failed(error);
        });
      };
      const geometry = observeVideoGeometry(element, (next) => {
        const previous = this.#views.get(key);
        if (
          previous?.visible === next.visible &&
          previous.height === next.height
        )
          return;
        this.#views.set(key, next);
        apply();
      });
      const close = () => {
        if (closed) return;
        closed = true;
        geometry.close();
        this.#views.delete(key);
        this.#viewClosers.delete(close);
        apply();
      };
      this.#viewClosers.add(close);
      return close;
    });
  }
  #applyViews(): Promise<void> {
    if (this.#views.size === 0) return super.configure(this.#requested);
    let visible = false;
    let height = 0;
    for (const view of this.#views.values()) {
      visible ||= view.visible;
      height = Math.max(height, view.height);
    }
    return super.configure({
      ...this.#requested,
      visible: this.#requested.visible && visible,
      height,
    });
  }
  state(): Readonly<VideoSubscriptionState> {
    return this.#observation.state();
  }
  updates(): VideoSubscriptionUpdates {
    return this.#observation.updates();
  }
  override async configure(
    configuration = defaultVideoObservation,
  ): Promise<void> {
    const previous = this.#requested;
    this.#requested = configuration;
    try {
      await this.#applyViews();
    } catch (error) {
      if (this.#requested === configuration) this.#requested = previous;
      throw error;
    }
  }
  observe(signal?: AbortSignal): Promise<MediaObservation> {
    return this.observeSource(undefined, signal);
  }
  override close(): Promise<void> {
    this.#retiringViews = true;
    for (const close of this.#viewClosers) close();
    return super.close();
  }
}

export class AudioSubscription extends Subscription<AudioSubscriptionConfig> {
  readonly #observation: ResourceObservation<AudioSubscriptionState>;
  constructor(
    resources: ResourceOwners,
    selector: TrackSelector,
    configuration: AudioSubscriptionConfig,
  ) {
    const observation = new ResourceObservation<AudioSubscriptionState>();
    super(
      resources,
      () => ({ op: "claimAudioSubscription", selector }),
      configuration,
      (owner, value) => ({
        op: "configureAudio",
        owner,
        playoutDelay: value.playoutDelay,
      }),
      (binding) =>
        observation.bind(
          (binding.runtime as ReceptionRuntime).audio_subscription(
            binding.owner,
          ),
        ),
    );
    this.#observation = observation;
  }
  state(): Readonly<AudioSubscriptionState> {
    return this.#observation.state();
  }
  updates(): AudioSubscriptionUpdates {
    return this.#observation.updates();
  }
  override configure(configuration = defaultAudioSubscription): Promise<void> {
    return super.configure(configuration);
  }
  observe(signal?: AbortSignal): Promise<MediaObservation> {
    return this.observeSource(undefined, signal);
  }
}
