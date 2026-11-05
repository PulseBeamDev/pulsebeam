import {
  ResourceError,
  ResourceOwner,
  ResourceOwners,
  resourceFailure,
  type ResourceBinding,
  type ResourceRuntime,
} from "./resources.js";
import type {
  AudioDemand,
  AudioSubscriptionConfig,
  TrackSelector,
  VideoObservation,
} from "./types.js";

interface NativeObservation {
  track(): MediaStreamTrack | undefined;
  is_closed(): boolean;
  close(): Promise<void>;
  free(): void;
}
interface ReceptionRuntime extends ResourceRuntime {
  observe_media(owner: bigint, source?: string): Promise<NativeObservation>;
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
  #applied: Promise<void> = Promise.resolve();
  #closing: Promise<void> | undefined;

  constructor(
    resources: ResourceOwners,
    claim: (configuration: C) => unknown,
    configuration: C,
    private readonly command: Configure<C>,
  ) {
    this.#configuration = configuration;
    this.#resource = resources.acquire(() => claim(this.#configuration), {
      bind: (binding) => {
        this.#applied = this.#apply(binding, this.#configuration);
        void this.#applied.catch((error: unknown) => {
          if (this.#resource.current(binding)) this.#resource.fail(error);
        });
      },
      unbind: () => {},
    });
    const ready = this.#resource.ready.then(async () => {
      await this.#applied;
      this.#assertOpen();
    });
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
  protected async observeSource(source?: string): Promise<MediaObservation> {
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

async function acquire<T extends { close(): Promise<void> }>(
  value: T,
): Promise<T> {
  try {
    await initialized.get(value)!;
    return value;
  } catch (error) {
    await value.close();
    throw error;
  }
}

export function claimVideoSubscription(
  resources: ResourceOwners,
  selector: TrackSelector,
  configuration: VideoObservation,
) {
  const subscription = new VideoSubscription(
    resources,
    selector,
    configuration,
  );
  return { subscription, ready: initialized.get(subscription)! };
}

export async function subscribeVideo(
  resources: ResourceOwners,
  selector: TrackSelector,
  configuration: VideoObservation,
): Promise<VideoSubscription> {
  return acquire(
    claimVideoSubscription(resources, selector, configuration).subscription,
  );
}
export async function subscribeAudio(
  resources: ResourceOwners,
  selector: TrackSelector,
  configuration: AudioSubscriptionConfig,
): Promise<AudioSubscription> {
  return acquire(new AudioSubscription(resources, selector, configuration));
}
export async function selectAudio(
  resources: ResourceOwners,
  policy: AudioDemand,
): Promise<AudioSelection> {
  return acquire(new AudioSelection(resources, policy));
}

export class VideoSubscription extends Subscription<VideoObservation> {
  constructor(
    resources: ResourceOwners,
    selector: TrackSelector,
    configuration: VideoObservation,
  ) {
    super(
      resources,
      () => ({ op: "claimVideo", selector }),
      configuration,
      (owner, observation) => ({ op: "observeVideo", owner, observation }),
    );
  }
  observe(): Promise<MediaObservation> {
    return this.observeSource();
  }
}

export class AudioSubscription extends Subscription<AudioSubscriptionConfig> {
  constructor(
    resources: ResourceOwners,
    selector: TrackSelector,
    configuration: AudioSubscriptionConfig,
  ) {
    super(
      resources,
      () => ({ op: "claimAudioSubscription", selector }),
      configuration,
      (owner, value) => ({
        op: "configureAudio",
        owner,
        playoutDelay: value.playoutDelay,
      }),
    );
  }
  observe(): Promise<MediaObservation> {
    return this.observeSource();
  }
}

export class AudioSelection extends Subscription<AudioDemand> {
  constructor(resources: ResourceOwners, policy: AudioDemand) {
    super(
      resources,
      (value) => ({ op: "claimAudioPolicy", policy: value }),
      policy,
      (owner, value) => ({ op: "configureAudioPolicy", owner, policy: value }),
    );
  }
  observeSource(source: string): Promise<MediaObservation> {
    return super.observeSource(source);
  }
}
