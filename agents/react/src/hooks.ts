import { useLayoutEffect, useMemo, useRef, useState } from "react";
import {
  AudioPublication,
  AudioSource,
  AudioSubscription,
  DataDelivery,
  LocalAudioTrack,
  LocalDataTrack,
  LocalVideoTrack,
  RemoteAudioTrack,
  RemoteDataTrack,
  RemoteVideoTrack,
  VideoPublication,
  VideoSource,
  VideoSubscription,
  type Catalog,
  type DataPublication,
  type DataPublishOptions,
  type DataSubscribeOptions,
  type DataSubscription,
  type Pipe,
  type PublicationConfig,
  type RoomRef,
  type VideoObservation,
  type WatchConfig,
} from "@pulsebeam/web";
import { useCommittedRef } from "./committed-ref.js";
import { useResource } from "./resource.js";
import { useObservation } from "./observation.js";
import {
  publicationTrack,
  type PublishableAudioSource,
  type PublishableVideoSource,
} from "./sources.js";

type Local = LocalVideoTrack | LocalAudioTrack | LocalDataTrack;
type Source =
  | VideoSource
  | AudioSource
  | PublishableVideoSource
  | PublishableAudioSource;
type Publication = VideoPublication | AudioPublication | DataPublication;
type Remote = RemoteVideoTrack | RemoteAudioTrack | RemoteDataTrack;
type Subscription = VideoSubscription | AudioSubscription | DataSubscription;

export function useWatch(
  room: RoomRef | null,
  options?: Partial<WatchConfig>,
): Catalog | null {
  const limit = options?.limit ?? 100;
  const cursor = Array.from(options?.cursor ?? []);
  const scope = JSON.stringify([limit, cursor]);
  const target = useMemo(
    () =>
      room
        ? { room, config: { limit, cursor: Uint8Array.from(cursor) } }
        : null,
    [room, scope],
  );
  return useResource(target, () => target!.room.watch(target!.config)).value;
}

export function usePublication(
  track: Local | null,
  source?: Source | null,
  options?: PublicationConfig | DataPublishOptions,
) {
  const mode =
    track instanceof LocalDataTrack
      ? ((options as DataPublishOptions | undefined)?.delivery ??
        DataDelivery.Reliable)
      : undefined;
  const target = useMemo(() => track && { track, mode }, [track, mode]);
  const publication = useResource<Publication>(target, () => {
    if (track instanceof LocalDataTrack)
      return track.publish(options as DataPublishOptions | undefined);
    if (track instanceof LocalVideoTrack || track instanceof LocalAudioTrack)
      return track.publish(options as PublicationConfig | undefined);
    throw new TypeError("Expected an SDK local track");
  });
  const owner = publication.value;
  const enabled = (options as PublicationConfig | undefined)?.enabled ?? true;
  useLayoutEffect(() => {
    if (
      !(owner instanceof VideoPublication) &&
      !(owner instanceof AudioPublication)
    )
      return;
    try {
      if (owner.state().configuration.enabled !== enabled)
        owner.configure({ enabled });
    } catch (error) {
      publication.fail(error);
    }
  }, [owner, enabled]);

  const cleanup = useRef(Promise.resolve());
  const [input, setInput] = useState<{
    owner: Publication;
    source: Source;
    pipe: Pipe;
  } | null>(null);
  useLayoutEffect(() => {
    if (!owner || !source) return;
    let active = true;
    let pipe: Pipe | undefined;
    void cleanup.current.then(() => {
      if (!active) return;
      try {
        const input =
          "pipeTo" in source ? source : publicationTrack(source, source.kind);
        if (input instanceof VideoSource && owner instanceof VideoPublication)
          pipe = input.pipeTo(owner);
        else if (
          input instanceof AudioSource &&
          owner instanceof AudioPublication
        )
          pipe = input.pipeTo(owner);
        else
          throw new TypeError(
            "Source and publication media kinds do not match",
          );
        setInput({ owner, source, pipe });
      } catch (error) {
        publication.fail(error);
      }
    });
    return () => {
      active = false;
      const prior = pipe;
      if (prior)
        cleanup.current = cleanup.current
          .then(() => prior.close())
          .catch((error: unknown) => {
            console.error("PulseBeam pipe cleanup failed", error);
          });
    };
  }, [owner, source]);
  const pipe =
    input?.owner === owner && input.source === source ? input.pipe : null;
  useObservation(pipe, (observed, error) => {
    if (observed === pipe) publication.fail(error);
  });
  return publication;
}

export function usePublish(
  track: LocalVideoTrack | null,
  source?: VideoSource | PublishableVideoSource | null,
  options?: PublicationConfig,
): VideoPublication | null;
export function usePublish(
  track: LocalAudioTrack | null,
  source?: AudioSource | PublishableAudioSource | null,
  options?: PublicationConfig,
): AudioPublication | null;
export function usePublish(
  track: LocalDataTrack | null,
  source?: null,
  options?: DataPublishOptions,
): DataPublication | null;
export function usePublish(
  track: Local | null,
  source?: Source | null,
  options?: PublicationConfig | DataPublishOptions,
): Publication | null {
  return usePublication(track, source, options).value;
}

export function useSubscribe(
  track: RemoteVideoTrack | null,
  options?: VideoObservation,
): VideoSubscription | null;
export function useSubscribe(
  track: RemoteAudioTrack | null,
): AudioSubscription | null;
export function useSubscribe(
  track: RemoteDataTrack | null,
  options?: DataSubscribeOptions,
): DataSubscription | null;
export function useSubscribe(
  track: Remote | null,
  options?: VideoObservation | DataSubscribeOptions,
): Subscription | null {
  const mode =
    track instanceof RemoteDataTrack
      ? ((options as DataSubscribeOptions | undefined)?.delivery ??
        DataDelivery.Reliable)
      : undefined;
  const target = useMemo(() => track && { track, mode }, [track, mode]);
  const committed = useCommittedRef(options);
  const applied = useRef(new WeakMap<Subscription, string>());
  const key = JSON.stringify(options ?? null);
  const subscription = useResource<Subscription>(target, () => {
    const owner =
      track instanceof RemoteDataTrack
        ? track.subscribe(options as DataSubscribeOptions | undefined)
        : track instanceof RemoteVideoTrack
          ? track.subscribe(options as VideoObservation | undefined)
          : track instanceof RemoteAudioTrack
            ? track.subscribe()
            : undefined;
    if (!owner) throw new TypeError("Expected an SDK remote track");
    applied.current.set(owner, key);
    return owner;
  });
  const owner = subscription.value;
  useLayoutEffect(() => {
    if (!owner || applied.current.get(owner) === key) return;
    try {
      if (owner instanceof VideoSubscription)
        owner.configure(committed.current as VideoObservation | undefined);
      applied.current.set(owner, key);
    } catch (error) {
      subscription.fail(error);
    }
  }, [owner, key]);
  return owner;
}
