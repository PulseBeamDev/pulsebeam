import { useCallback, useEffect, useRef, useState } from "react";

const stop = (stream: MediaStream | null) =>
  stream?.getTracks().forEach((track) => track.stop());
export function useMediaDevices() {
  const [stream, setStream] = useState<MediaStream | null>(null);
  const [devices, setDevices] = useState<MediaDeviceInfo[]>([]);
  const [videoDeviceId, setVideoDeviceIdState] = useState("");
  const [audioDeviceId, setAudioDeviceIdState] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [isMicOn, setMic] = useState(true);
  const [isCamOn, setCam] = useState(true);
  const live = useRef(true);
  const micOn = useRef(true);
  const camOn = useRef(true);
  const sequence = useRef(0);
  const streamRef = useRef<MediaStream | null>(null);
  const refresh = useCallback(async () => {
    setDevices(
      (await navigator.mediaDevices.enumerateDevices()).filter(
        (device) => device.deviceId && device.label,
      ),
    );
  }, []);
  const startMedia = useCallback(
    async (video = videoDeviceId, audio = audioDeviceId) => {
      const request = ++sequence.current;
      setError(null);
      try {
        const replacement = await navigator.mediaDevices.getUserMedia({
          video: {
            deviceId: video ? { exact: video } : undefined,
            width: { ideal: 1280 },
            height: { ideal: 720 },
            aspectRatio: { ideal: 16 / 9 },
            frameRate: { ideal: 30 },
          },
          audio: {
            deviceId: audio ? { exact: audio } : undefined,
            channelCount: 1,
            echoCancellation: true,
            noiseSuppression: true,
            autoGainControl: true,
          },
        });
        if (!live.current || request !== sequence.current)
          return stop(replacement);
        replacement.getVideoTracks().forEach((track) => {
          track.enabled = camOn.current;
        });
        replacement.getAudioTracks().forEach((track) => {
          track.enabled = micOn.current;
        });
        const previous = streamRef.current;
        streamRef.current = replacement;
        setStream(replacement);
        setVideoDeviceIdState(
          replacement.getVideoTracks()[0]?.getSettings().deviceId ?? video,
        );
        setAudioDeviceIdState(
          replacement.getAudioTracks()[0]?.getSettings().deviceId ?? audio,
        );
        stop(previous);
        await refresh();
      } catch (reason) {
        if (live.current && request === sequence.current)
          setError(
            reason instanceof Error
              ? reason.message
              : "Unable to access camera or microphone",
          );
      }
    },
    [audioDeviceId, refresh, videoDeviceId],
  );
  useEffect(() => {
    const requests = sequence;
    const activeStream = streamRef;
    live.current = true;
    const initial = setTimeout(() => void refresh(), 0);
    navigator.mediaDevices.addEventListener("devicechange", refresh);
    return () => {
      clearTimeout(initial);
      live.current = false;
      ++requests.current;
      navigator.mediaDevices.removeEventListener("devicechange", refresh);
      stop(activeStream.current);
    };
  }, [refresh]);
  const toggle = (kind: "audio" | "video", value: boolean) => {
    streamRef.current
      ?.getTracks()
      .filter((track) => track.kind === kind)
      .forEach((track) => {
        track.enabled = value;
      });
  };
  const takeStream = () => {
    const active = streamRef.current;
    streamRef.current = null;
    setStream(null);
    return active;
  };
  return {
    stream,
    devices,
    videoDeviceId,
    audioDeviceId,
    error,
    isMicOn,
    isCamOn,
    startMedia,
    takeStream,
    toggleAudio: () => {
      setMic((value) => {
        const next = !value;
        micOn.current = next;
        toggle("audio", next);
        return next;
      });
    },
    toggleVideo: () => {
      setCam((value) => {
        const next = !value;
        camOn.current = next;
        toggle("video", next);
        return next;
      });
    },
    setVideoDeviceId: (id: string) => {
      setVideoDeviceIdState(id);
      void startMedia(id, audioDeviceId);
    },
    setAudioDeviceId: (id: string) => {
      setAudioDeviceIdState(id);
      void startMedia(videoDeviceId, id);
    },
  };
}
