import { useLayoutEffect, useMemo, useRef, useState } from "react";
import {
  AudioCollectionRef,
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
  VideoCodec,
  VideoCollectionRef,
  VideoPublication,
  VideoSource,
  VideoSubscription,
  type AudioCollection,
  type Catalog,
  type VideoCollection,
  type DataPublication,
  type DataPublishOptions,
  type DataSubscribeOptions,
  type DataSubscription,
  type Pipe,
  type PublicationConfig,
  type RoomRef,
  type VideoObservation,
  type VideoPublishOptions,
  type WatchConfig,
} from "@pulsebeam/web";
import { useCommittedRef } from "./committed-ref.js";
import { useResource } from "./resource.js";
import { useObservation } from "./observation.js";
import { useStableTarget } from "./stable-target.js";
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
type AudioPublishOptions = PublicationConfig & { codec?: never };
type TopicPublishOptions = DataPublishOptions & { codec?: never };
type PublishOptions =
  | VideoPublishOptions
  | AudioPublishOptions
  | TopicPublishOptions;
type Remote =
  | RemoteVideoTrack
  | RemoteAudioTrack
  | RemoteDataTrack
  | VideoCollectionRef
  | AudioCollectionRef;
type Subscription =
  | VideoSubscription
  | AudioSubscription
  | DataSubscription
  | VideoCollection
  | AudioCollection;

function sameSelection(previous: Remote | null, current: Remote | null) {
  if (previous instanceof VideoCollectionRef && current instanceof VideoCollectionRef)
    return previous.sameSelection(current);
  if (previous instanceof AudioCollectionRef && current instanceof AudioCollectionRef)
    return previous.sameSelection(current);
  return false;
}

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
  options?: PublishOptions,
) {
  const mode =
    track instanceof LocalDataTrack
      ? ((options as DataPublishOptions | undefined)?.delivery ??
        DataDelivery.Reliable)
      : undefined;
  const codec =
    track instanceof LocalVideoTrack
      ? (options?.codec ?? VideoCodec.H264)
      : undefined;
  const enabled = (options as PublicationConfig | undefined)?.enabled ?? true;
  const target = useMemo(
    () => track && { track, mode, codec },
    [track, mode, codec],
  );
  const publication = useResource<Publication>(target, () => {
    if (track instanceof LocalDataTrack)
      return track.publish(options as DataPublishOptions | undefined);
    if (track instanceof LocalVideoTrack)
      return track.publish({ enabled, codec: codec ?? VideoCodec.H264 });
    if (track instanceof LocalAudioTrack) return track.publish({ enabled });
    throw new TypeError("Expected an SDK local track");
  });
  const owner = publication.value;
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
  options?: VideoPublishOptions,
): VideoPublication | null;
export function usePublish(
  track: LocalAudioTrack | null,
  source?: AudioSource | PublishableAudioSource | null,
  options?: AudioPublishOptions,
): AudioPublication | null;
export function usePublish(
  track: LocalDataTrack | null,
  source?: null,
  options?: TopicPublishOptions,
): DataPublication | null;
export function usePublish(
  track: Local | null,
  source?: Source | null,
  options?: PublishOptions,
): Publication | null {
  return usePublication(track, source, options).value;
}

export function useSubscribe(track: VideoCollectionRef | null): VideoCollection | null;
export function useSubscribe(track: AudioCollectionRef | null): AudioCollection | null;
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
  selection: Remote | null,
  options?: VideoObservation | DataSubscribeOptions,
): Subscription | null {
  const track = useStableTarget(selection, sameSelection);
  const mode =
    track instanceof RemoteDataTrack
      ? (options as DataSubscribeOptions | undefined)?.delivery
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
          : track instanceof RemoteAudioTrack ||
              track instanceof AudioCollectionRef ||
              track instanceof VideoCollectionRef
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
