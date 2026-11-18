import { useLayoutEffect } from "react";
import type {
  LocalVideoTrack,
  LocalAudioTrack,
  VideoPublication,
  AudioPublication,
  VideoSenderConfig,
  AudioSenderConfig,
} from "@pulsebeam/web";
import { usePublication } from "../../src/hooks.js";
import type {
  PublishableVideoSource,
  PublishableAudioSource,
} from "../../src/sources.js";

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

export function Publication(props: PublishProps): null {
  const publication = usePublication(props.track, props.source, {
    enabled: props.enabled ?? true,
  });
  const owner = publication.value;
  useLayoutEffect(() => {
    if (!owner) return;
    const configuring =
      props.track.kind === "video"
        ? (owner as VideoPublication).configureSender({
            contentHint: (props.content ??
              "motion") as VideoSenderConfig["contentHint"],
          })
        : (owner as AudioPublication).configureSender({
            contentHint: (props.content ??
              "speech") as AudioSenderConfig["contentHint"],
          });
    void configuring.catch(publication.fail);
  }, [owner, props.content]);
  return null;
}
