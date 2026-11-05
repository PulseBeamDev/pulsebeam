import { useLayoutEffect, useRef, useState } from "react";
import type {
  AudioDemand,
  AudioPublication,
  AudioSelection,
  AudioSenderConfig,
  AudioSource,
  DataEvent,
  DataReceiver,
  Delivery,
  PlaybackFailure,
  VideoPublication,
  VideoSenderConfig,
  VideoSource,
} from "@pulsebeam/web";
import {
  attachRemoteAudio,
  claimPublication,
} from "@pulsebeam/web/internal/react";
import { useCommittedRef } from "./committed-ref.js";
import {
  receiveData,
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

type Publication =
  | { kind: "video"; value: VideoPublication }
  | { kind: "audio"; value: AudioPublication };

export function Publish(props: PublishProps): null {
  const committed = useCommittedRef(props);
  const target = trackIdentity(props.track);
  const synchronize = useRef<(() => void) | null>(null);
  const [failure, setFailure] = useState<unknown>(null);
  if (failure) throw failure;
  useLayoutEffect(() => {
    setFailure(null);
    let active = true;
    let owner: Publication | undefined;
    const report = (error: unknown) => {
      if (active) setFailure(error);
    };
    const sync = () => {
      if (!active) return;
      const view = committed.current;
      try {
        if (!owner) {
          if (!view.source) return;
          const configuration = { enabled: view.enabled ?? true };
          let ready: Promise<void>;
          if (view.track.kind === "video") {
            const claim = claimPublication(
              localMedia(view.track),
              publicationTrack(view.source, "video") as VideoSource,
              configuration,
            );
            owner = { kind: "video", value: claim.publication };
            ready = claim.ready;
          } else {
            const claim = claimPublication(
              localMedia(view.track),
              publicationTrack(view.source, "audio") as AudioSource,
              configuration,
            );
            owner = { kind: "audio", value: claim.publication };
            ready = claim.ready;
          }
          void ready.then(sync).catch(report);
          return;
        }
        const source = view.source
          ? publicationTrack(view.source, owner.kind)
          : null;
        const configuration = { enabled: view.enabled ?? true };
        const updates =
          owner.kind === "video"
            ? [
                owner.value.setSource(source as VideoSource | null),
                owner.value.configure(configuration),
                owner.value.configureSender({
                  contentHint: (view.content ??
                    "motion") as VideoSenderConfig["contentHint"],
                }),
              ]
            : [
                owner.value.setSource(source as AudioSource | null),
                owner.value.configure(configuration),
                owner.value.configureSender({
                  contentHint: (view.content ??
                    "speech") as AudioSenderConfig["contentHint"],
                }),
              ];
        void Promise.all(updates).catch(report);
      } catch (error) {
        report(error);
      }
    };
    synchronize.current = sync;
    sync();
    return () => {
      active = false;
      if (synchronize.current === sync) synchronize.current = null;
      void owner?.value.close();
    };
  }, [target]);
  useLayoutEffect(() => {
    synchronize.current?.();
  }, [props.source, props.enabled, props.content]);
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
    let active = true;
    let receiver: DataReceiver<D> | undefined;
    const release = () => {
      active = false;
      void receiver?.close();
    };
    void (async () => {
      try {
        receiver = await receiveData(track);
        if (!active) return;
        while (active) {
          const next = await receiver.next();
          if (next.type === "closed" || !active) break;
          await callbacks.current.onEvent(next.event);
        }
      } catch (error) {
        if (!active) return;
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

export interface SelectAudioProps {
  readonly agent: Agent;
  readonly policy: AudioDemand;
}
export function SelectAudio({ agent, policy }: SelectAudioProps): null {
  const target = managedAgent(agent);
  const committed = useCommittedRef(policy);
  const owner = useRef<AudioSelection | null>(null);
  const [failure, setFailure] = useState<unknown>(null);
  const policyKey = JSON.stringify(policy);
  if (failure) throw failure;
  useLayoutEffect(() => {
    setFailure(null);
    let active = true;
    void target
      .selectAudio(committed.current)
      .then(async (selection) => {
        if (!active) {
          await selection.close();
          return;
        }
        owner.current = selection;
        await selection.configure(committed.current);
      })
      .catch((error: unknown) => {
        if (active) setFailure(error);
      });
    return () => {
      active = false;
      const selection = owner.current;
      owner.current = null;
      void selection?.close();
    };
  }, [target]);
  useLayoutEffect(() => {
    const selection = owner.current;
    if (!selection) return;
    void selection.configure(committed.current).catch((error: unknown) => {
      if (owner.current === selection) setFailure(error);
    });
  }, [target, policyKey]);
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
