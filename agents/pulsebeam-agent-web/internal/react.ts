import {
  AudioSubscriptionUpdates,
  VideoSubscriptionUpdates,
  WebAudioOutput,
  WebVideoOutput,
  WebAgentError,
  type AudioTrack,
  type VideoTrack,
  type AudioSource,
  type AudioSubscription,
  type VideoSource,
  type VideoSubscription,
} from "../generated/bindings/pulsebeam_agent_web.js";
import { ErrorCode } from "../generated/bindings/pulsebeam_agent_core.js";
import {
  attachPlayback,
  type BorrowedMediaAttachment,
  type PlaybackReceiver,
} from "../web/platform-playback.js";
import { watchTrackLease } from "../web/media-registry.js";

export type { BorrowedMediaAttachment } from "../web/platform-playback.js";

export function attachMedia(
  input:
    | AudioSource
    | VideoSource
    | AudioSubscription
    | VideoSubscription
    | AudioTrack
    | VideoTrack,
  kind: "video" | "audio",
  element: HTMLMediaElement,
  autoPlay: boolean,
  blocked: (error: unknown, retry: () => Promise<void>) => void,
  failed: (error: unknown) => void,
): BorrowedMediaAttachment {
  const target = { kind, element, autoPlay, blocked, failed };
  if ("previewTrack" in input) {
    return attachPlayback({
      ...target,
      source: {
        track: () => input.previewTrack() ?? null,
        watch(listener) {
          const track = input.previewTrack();
          return track
            ? watchTrackLease(track, () => {
                if (input.previewTrack() !== track) listener();
              })
            : () => {};
        },
      },
    });
  }
  const observe: PlaybackReceiver["observe"] = async (signal) => {
    let output: WebAudioOutput | WebVideoOutput;
    try {
      output = await input.playbackOutput({ signal });
    } catch (error) {
      if (
        "sameAdmission" in input &&
        WebAgentError.Operation.instanceOf(error) &&
        (error.inner.info.code === ErrorCode.Replaced ||
          error.inner.info.code === ErrorCode.Closed)
      ) {
        // The parent snapshot supplies the successor capability.
        return undefined;
      }
      throw error;
    }
    let closing: Promise<void> | undefined;
    return {
      isClosed: () => output.track() === undefined,
      track: () => output.track() ?? null,
      close: () =>
        (closing ??= output.close().finally(() => {
          if (
            WebAudioOutput.instanceOf(output) ||
            WebVideoOutput.instanceOf(output)
          )
            output.uniffiDestroy();
        })),
    };
  };
  if ("sameAdmission" in input)
    return attachPlayback({ ...target, source: { observe, isClosed: () => false } });

  const source: PlaybackReceiver = {
    observe,
    isClosed: () => input.state().closed,
    updates() {
      const cursor = input.updates();
      return {
        next: () => cursor.next(),
        close() {
          try {
            cursor.close();
          } finally {
            if (
              AudioSubscriptionUpdates.instanceOf(cursor) ||
              VideoSubscriptionUpdates.instanceOf(cursor)
            )
              cursor.uniffiDestroy();
          }
        },
      };
    },
  };
  return attachPlayback({ ...target, source });
}
