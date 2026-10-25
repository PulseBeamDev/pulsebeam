import { useLayoutEffect, useRef, useState } from "react";
import {
  attachLocalTrack,
  attachRemoteAudio,
  type AudioSenderConfig,
  type CapturedAudioTrack,
  type CapturedVideoTrack,
  type DataEvent,
  type Delivery,
  type LocalMediaAttachment,
  type PlaybackFailure,
  type VideoSenderConfig,
} from "@pulsebeam/web";
import { useCommittedRef } from "./committed-ref.js";
import {
  dataEvents,
  localMedia,
  trackIdentity,
  managedAgent,
  type Agent,
  type LocalDataTrack,
  type LocalVideoTrack,
  type LocalAudioTrack,
  type RemoteDataTrack,
} from "./agent.js";
import {
  publicationTrack,
  type PublishableAudioSource,
  type PublishableVideoSource,
} from "./sources.js";

export type PublicationSource = PublishableVideoSource | PublishableAudioSource;

export type PublishProps =
  | {
      readonly track: LocalVideoTrack;
      readonly source: PublishableVideoSource | null;
      readonly enabled?: boolean;
      readonly content?: VideoSenderConfig["contentHint"];
    }
  | {
      readonly track: LocalAudioTrack;
      readonly source: PublishableAudioSource | null;
      readonly enabled?: boolean;
      readonly content?: AudioSenderConfig["contentHint"];
    };

export function Publish(props: PublishProps): null {
  const committed = useCommittedRef(props);
  const target = trackIdentity(props.track);
  const attachment = useRef<LocalMediaAttachment | null>(null);
  const [failure, setFailure] = useState<unknown>(null);
  if (failure) throw failure;

  useLayoutEffect(() => {
    setFailure(null);
    const current = committed.current;
    const source = current.source
      ? publicationTrack(current.source, current.track.kind)
      : null;
    const options = {
      source,
      enabled: current.enabled ?? true,
      config:
        current.track.kind === "video"
          ? { contentHint: current.content ?? "motion" }
          : { contentHint: current.content ?? "speech" },
      onError: (error: unknown) => {
        if (attachment.current === owner) setFailure(error);
      },
    };
    const owner =
      current.track.kind === "video"
        ? attachLocalTrack(localMedia(current.track), {
            ...options,
            source: source as CapturedVideoTrack | null,
            config: options.config as VideoSenderConfig,
          })
        : attachLocalTrack(localMedia(current.track), {
            ...options,
            source: source as CapturedAudioTrack | null,
            config: options.config as AudioSenderConfig,
          });
    attachment.current = owner;
    void owner.ready.catch((error) => {
      if (attachment.current === owner) setFailure(error);
    });
    return () => {
      if (attachment.current === owner) attachment.current = null;
      void owner.close();
    };
  }, [target]);

  const configKey = `${props.track.kind}:${props.content ?? ""}`;
  useLayoutEffect(() => {
    const owner = attachment.current;
    if (!owner) return;
    const source = props.source
      ? publicationTrack(props.source, props.track.kind)
      : null;
    owner.setSource(source);
    owner.setEnabled(props.enabled ?? true);
    owner.setConfig(
      props.track.kind === "video"
        ? { contentHint: props.content ?? "motion" }
        : { contentHint: props.content ?? "speech" },
    );
  }, [props.source, props.enabled, configKey]);
  return null;
}

export interface DataProps<D extends Delivery = Delivery> {
  readonly track: LocalDataTrack<D> | RemoteDataTrack<D>;
  readonly onEvent: (event: DataEvent<D>) => void | Promise<void>;
  readonly onError?: (error: unknown) => void;
}

export function Data<D extends Delivery>({
  track,
  onEvent,
  onError,
}: DataProps<D>): null {
  const callbacks = useCommittedRef({ onEvent, onError });
  const target = trackIdentity(track);
  useLayoutEffect(() => {
    const controller = new AbortController();
    let events: AsyncIterableIterator<DataEvent<D>> | undefined;
    const release = () => {
      controller.abort();
      void events?.return?.();
    };
    void (async () => {
      try {
        events = dataEvents(track, { signal: controller.signal });
        while (!controller.signal.aborted) {
          const next = await events.next();
          if (next.done || controller.signal.aborted) break;
          await callbacks.current.onEvent(next.value);
        }
      } catch (error) {
        if (controller.signal.aborted) return;
        release();
        try {
          if (callbacks.current.onError) callbacks.current.onError(error);
          else console.error("PulseBeam data receive failed", error);
        } catch (reportingError) {
          console.error("PulseBeam data error handler failed", reportingError);
        }
      } finally {
        release();
      }
    })();
    return release;
  }, [target]);
  return null;
}

export interface AudioProps {
  readonly agent: Agent;
  readonly onPlaybackError?: (failure: PlaybackFailure) => void;
}

export function Audio({ agent, onPlaybackError }: AudioProps): null {
  const callback = useCommittedRef(onPlaybackError);
  const target = managedAgent(agent);
  const [failure, setFailure] = useState<unknown>(null);
  if (failure) throw failure;
  useLayoutEffect(() => {
    setFailure(null);
    let active = true;
    const owner = attachRemoteAudio(target, {
      onPlaybackError: (value) => {
        if (active) callback.current?.(value);
      },
    });
    void owner.ready.catch((error) => {
      if (active) setFailure(error);
    });
    return () => {
      active = false;
      void owner.close();
    };
  }, [target]);
  return null;
}
