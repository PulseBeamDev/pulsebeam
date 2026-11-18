import { useLayoutEffect, useState } from "react";
import type {
  AudioPublication,
  AudioSource,
  AudioSubscription,
  AudioSubscriptionConfig,
  DataPublication,
  DataSubscription,
  Delivery,
  LocalAudioTrack,
  LocalDataTrack,
  LocalVideoTrack,
  Pipe,
  PublicationConfig,
  RemoteAudioTrack,
  RemoteDataTrack,
  RemoteVideoTrack,
  VideoObservation,
  VideoPublication,
  VideoSource,
  VideoSubscription,
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

export function usePublication(
  track: Local | null,
  source?: Source | null,
  options?: PublicationConfig,
) {
  const committed = useCommittedRef({ track, source, options });
  const publication = useResource<Publication>(track, () => {
    const view = committed.current;
    return view.track!.kind === "data"
      ? view.track!.publish()
      : view.track!.publish(view.options);
  });
  const owner = publication.value;
  const enabled = options?.enabled ?? true;
  useLayoutEffect(() => {
    if (owner && track?.kind !== "data") {
      try {
        (owner as VideoPublication | AudioPublication).configure({ enabled });
      } catch (error) {
        publication.fail(error);
      }
    }
  }, [owner, enabled]);
  const [input, setInput] = useState<{
    owner: Publication;
    source: Source;
    pipe: Pipe;
  } | null>(null);
  useLayoutEffect(() => {
    if (!owner || !source) return;
    let pipe: Pipe;
    try {
      const input =
        "pipeTo" in source
          ? source
          : publicationTrack(source, track!.kind as "video" | "audio");
      pipe =
        track!.kind === "video"
          ? (input as VideoSource).pipeTo(owner as VideoPublication)
          : (input as AudioSource).pipeTo(owner as AudioPublication);
      setInput({ owner, source, pipe });
    } catch (error) {
      publication.fail(error);
      return;
    }
    return () => {
      void pipe.close().catch((error: unknown) => {
        console.error("PulseBeam pipe cleanup failed", error);
      });
    };
  }, [owner, source]);
  useObservation(
    input?.owner === owner && input.source === source ? input.pipe : null,
  );
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
export function usePublish<D extends Delivery>(
  track: LocalDataTrack<D> | null,
  source?: null,
): DataPublication | null;
export function usePublish(
  track: Local | null,
  source?: Source | null,
  options?: PublicationConfig,
): Publication | null {
  return usePublication(track, source, options).value;
}

export function useSubscribe(
  track: RemoteVideoTrack | null,
  options?: VideoObservation,
): VideoSubscription | null;
export function useSubscribe(
  track: RemoteAudioTrack | null,
  options?: AudioSubscriptionConfig,
): AudioSubscription | null;
export function useSubscribe<D extends Delivery>(
  track: RemoteDataTrack<D> | null,
): DataSubscription<D> | null;
export function useSubscribe(
  track: Remote | null,
  options?: VideoObservation | AudioSubscriptionConfig,
): Subscription | null {
  const committed = useCommittedRef({ track, options });
  const subscription = useResource<Subscription>(track, () => {
    const view = committed.current;
    if (view.track!.kind === "data") return view.track!.subscribe();
    return view.track!.kind === "video"
      ? view.track!.subscribe(view.options as VideoObservation | undefined)
      : view.track!.subscribe(
          view.options as AudioSubscriptionConfig | undefined,
        );
  });
  const owner = subscription.value;
  const key = JSON.stringify(options ?? null);
  useLayoutEffect(() => {
    if (!owner || track?.kind === "data") return;
    const updating =
      track?.kind === "video"
        ? (owner as VideoSubscription).configure(
            committed.current.options as VideoObservation | undefined,
          )
        : (owner as AudioSubscription).configure(
            committed.current.options as AudioSubscriptionConfig | undefined,
          );
    void updating.catch(subscription.fail);
  }, [owner, key]);
  return owner;
}
