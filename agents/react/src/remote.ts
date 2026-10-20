import type {
  RemoteMedia as WebRemoteMedia,
  RemoteParticipant as WebRemoteParticipant,
  RemoteVideoTrack as WebRemoteVideoTrack,
  RemoteAudioTrack,
} from "@pulsebeam/web";

export type RemoteVideoTrack = Omit<WebRemoteVideoTrack, "setReceiveOptions">;
export interface RemoteParticipant
  extends Omit<WebRemoteParticipant, "video" | "videoTracks"> {
  video(label: string): RemoteVideoTrack;
  readonly videoTracks: readonly RemoteVideoTrack[];
}
export interface RemoteMedia
  extends Omit<WebRemoteMedia, "participant" | "participants" | "videoTracks"> {
  participant(externalId: string): RemoteParticipant;
  readonly participants: readonly RemoteParticipant[];
  readonly videoTracks: readonly RemoteVideoTrack[];
}

const videoSources = new WeakMap<RemoteVideoTrack, WebRemoteVideoTrack>();
export function remoteVideoSource(
  source: RemoteVideoTrack,
): WebRemoteVideoTrack {
  return videoSources.get(source) ?? (source as WebRemoteVideoTrack);
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
  const audios = new WeakMap<RemoteAudioTrack, RemoteAudioTrack>();
  const participants = new WeakMap<WebRemoteParticipant, RemoteParticipant>();
  const video = (source: WebRemoteVideoTrack): RemoteVideoTrack => {
    let handle = videos.get(source);
    if (!handle) {
      handle = Object.freeze({
        kind: source.kind,
        participantId: source.participantId,
        label: source.label,
        get active() {
          return source.active;
        },
        subscribe: (listener: () => void) => source.subscribe(listener),
      });
      videos.set(source, handle);
      videoSources.set(handle, source);
    }
    return handle;
  };
  const audio = (source: RemoteAudioTrack): RemoteAudioTrack => {
    let handle = audios.get(source);
    if (!handle) {
      handle = Object.freeze({
        kind: source.kind,
        participantId: source.participantId,
        label: source.label,
        get receiving() {
          return source.receiving;
        },
        subscribe: (listener: () => void) => source.subscribe(listener),
        readWaveform: (buffer: Float32Array) => source.readWaveform(buffer),
        readSpectrum: (buffer: Float32Array) => source.readSpectrum(buffer),
      });
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
