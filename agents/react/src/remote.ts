import type {
  RemoteMedia as WebRemoteMedia,
  RemoteParticipant as WebRemoteParticipant,
  RemoteVideoTrack as WebRemoteVideoTrack,
  RemoteAudioTrack as WebRemoteAudioTrack,
} from "@pulsebeam/web";
import { attachRemoteVideo } from "@pulsebeam/web";
import {
  videoSource,
  audioSource,
  type VideoSource,
  type AudioSource,
} from "./sources.js";

export interface RemoteVideoTrack
  extends Omit<WebRemoteVideoTrack, "setReceiveOptions">,
    VideoSource {}
export interface RemoteAudioTrack extends WebRemoteAudioTrack, AudioSource {}
export interface RemoteParticipant
  extends Omit<
    WebRemoteParticipant,
    "video" | "videoTracks" | "audio" | "audioTracks"
  > {
  video(label: string): RemoteVideoTrack;
  audio(label: string): RemoteAudioTrack;
  readonly videoTracks: readonly RemoteVideoTrack[];
  readonly audioTracks: readonly RemoteAudioTrack[];
}
export interface RemoteMedia
  extends Omit<
    WebRemoteMedia,
    "participant" | "participants" | "videoTracks" | "audioTracks"
  > {
  participant(externalId: string): RemoteParticipant;
  readonly participants: readonly RemoteParticipant[];
  readonly videoTracks: readonly RemoteVideoTrack[];
  readonly audioTracks: readonly RemoteAudioTrack[];
}

function collections<T, U>(
  map: (source: T) => U,
): (source: readonly T[]) => readonly U[] {
  const cache = new WeakMap<readonly T[], readonly U[]>();
  return (source) => {
    let result = cache.get(source);
    if (!result) {
      result = Object.freeze(source.map(map));
      cache.set(source, result);
    }
    return result;
  };
}

export function createRemoteMedia(raw: WebRemoteMedia): RemoteMedia {
  const videos = new WeakMap<WebRemoteVideoTrack, RemoteVideoTrack>();
  const audios = new WeakMap<WebRemoteAudioTrack, RemoteAudioTrack>();
  const participants = new WeakMap<WebRemoteParticipant, RemoteParticipant>();
  const video = (source: WebRemoteVideoTrack): RemoteVideoTrack => {
    let handle = videos.get(source);
    if (!handle) {
      handle = Object.freeze(
        videoSource(
          {
            kind: source.kind,
            participantId: source.participantId,
            label: source.label,
            get active() {
              return source.active;
            },
            subscribe: (listener: () => void) => source.subscribe(listener),
          },
          (element, onError, options) =>
            attachRemoteVideo(
              source,
              element,
              (failure, retry) =>
                onError(failure.cause ?? new Error(failure.message), retry),
              options,
            ),
        ),
      );
      videos.set(source, handle);
    }
    return handle;
  };
  const audio = (source: WebRemoteAudioTrack): RemoteAudioTrack => {
    let handle = audios.get(source);
    if (!handle) {
      handle = Object.freeze(
        audioSource({
          kind: source.kind,
          participantId: source.participantId,
          label: source.label,
          get receiving() {
            return source.receiving;
          },
          subscribe: (listener: () => void) => source.subscribe(listener),
          readWaveform: (buffer: Float32Array) => source.readWaveform(buffer),
          readSpectrum: (buffer: Float32Array) => source.readSpectrum(buffer),
        }),
      );
      audios.set(source, handle);
    }
    return handle;
  };
  const videoTracks = collections(video);
  const audioTracks = collections(audio);
  const participant = (source: WebRemoteParticipant): RemoteParticipant => {
    let handle = participants.get(source);
    if (!handle) {
      handle = Object.freeze({
        externalId: source.externalId,
        video: (label: string) => video(source.video(label)),
        audio: (label: string) => audio(source.audio(label)),
        get videoTracks() {
          return videoTracks(source.videoTracks);
        },
        get audioTracks() {
          return audioTracks(source.audioTracks);
        },
      });
      participants.set(source, handle);
    }
    return handle;
  };
  const participantList = collections(participant);
  return Object.freeze({
    get participants() {
      return participantList(raw.participants);
    },
    get videoTracks() {
      return videoTracks(raw.videoTracks);
    },
    get audioTracks() {
      return audioTracks(raw.audioTracks);
    },
    participant: (externalId: string) =>
      participant(raw.participant(externalId)),
    resumeAudio: () => raw.resumeAudio(),
  });
}
