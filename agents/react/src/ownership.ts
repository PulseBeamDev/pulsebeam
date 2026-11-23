import { useLayoutEffect } from "react";
import type {
  DataEvent,
  DataSubscribeOptions,
  RemoteDataTrack,
} from "@pulsebeam/web";
import { useCommittedRef } from "./committed-ref.js";
import { useSubscribe } from "./hooks.js";
import type {
  PublishableAudioSource,
  PublishableVideoSource,
} from "./sources.js";
export type PublicationSource = PublishableVideoSource | PublishableAudioSource;

export interface DataProps {
  readonly track: RemoteDataTrack;
  readonly options?: DataSubscribeOptions;
  readonly onEvent: (event: DataEvent) => void | Promise<void>;
  readonly onError?: (error: unknown) => void;
}
export function Data({ track, options, onEvent, onError }: DataProps): null {
  const callbacks = useCommittedRef({ onEvent, onError });
  const receiver = useSubscribe(track, options);
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
