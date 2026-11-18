import { useLayoutEffect, useRef, useState } from "react";
import type {
  Agent,
  AudioDemand,
  AudioSelection,
  DataEvent,
  Delivery,
  RemoteDataTrack,
} from "@pulsebeam/web";
import { useCommittedRef } from "./committed-ref.js";
import { useSubscribe } from "./hooks.js";
import type {
  PublishableAudioSource,
  PublishableVideoSource,
} from "./sources.js";
export type PublicationSource = PublishableVideoSource | PublishableAudioSource;

export interface DataProps<D extends Delivery = Delivery> {
  readonly track: RemoteDataTrack<D>;
  readonly onEvent: (event: DataEvent<D>) => void | Promise<void>;
  readonly onError?: (error: unknown) => void;
}
export function Data<D extends Delivery>({
  track,
  onEvent,
  onError,
}: DataProps<D>): null {
  const callbacks = useCommittedRef({ onEvent, onError });
  const receiver = useSubscribe(track);
  useLayoutEffect(() => {
    if (!receiver) return;
    let active = true;
    void (async () => {
      try {
        while (active) {
          const next = await receiver.next();
          if (!active || next.type === "closed") break;
          await callbacks.current.onEvent(next.event);
        }
      } catch (error) {
        if (!active) return;
        try {
          if (callbacks.current.onError) callbacks.current.onError(error);
          else console.error("PulseBeam data receive failed", error);
        } catch (reportingError) {
          console.error("PulseBeam data error handler failed", reportingError);
        }
      } finally {
        void receiver.close().catch((error: unknown) => {
          console.error("PulseBeam data cleanup failed", error);
        });
      }
    })();
    return () => {
      active = false;
    };
  }, [receiver]);
  return null;
}

export interface SelectAudioProps {
  readonly agent: Agent;
  readonly policy: AudioDemand;
}
export function SelectAudio({ agent: target, policy }: SelectAudioProps): null {
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
      void selection?.close().catch((error: unknown) => {
        console.error("PulseBeam audio selection cleanup failed", error);
      });
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
