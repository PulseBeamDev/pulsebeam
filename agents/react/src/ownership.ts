import { useLayoutEffect } from "react";
import type { DataEvent, DataSetRef, RemoteDataTrack } from "@pulsebeam/web";
import { useCommittedRef } from "./committed-ref.js";
import { useSubscribe } from "./hooks.js";
import type {
  PublishableAudioSource,
  PublishableVideoSource,
} from "./sources.js";
export type PublicationSource = PublishableVideoSource | PublishableAudioSource;

export type DataProps = {
  readonly track: RemoteDataTrack | DataSetRef;
  readonly onEvent: (event: DataEvent) => void | Promise<void>;
  readonly onError?: (error: unknown) => void;
};
export function Data({ track, onEvent, onError }: DataProps): null {
  const callbacks = useCommittedRef({ onEvent, onError });
  const receiver = useSubscribe(track);
  useLayoutEffect(() => {
    if (!receiver) return;
    let active = true;
    void (async () => {
      try {
        while (active) {
          const event = await receiver.next();
          if (!active || event === undefined) break;
          await callbacks.current.onEvent(event);
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
