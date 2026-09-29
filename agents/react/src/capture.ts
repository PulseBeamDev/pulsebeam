import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useCommittedRef } from "./committed-ref.js";
import {
  createCaptureSource,
  type CapturedAudioTrack,
  type CapturedVideoTrack,
} from "@pulsebeam/web";

export type MediaCaptureErrorCode =
  | "permission-denied"
  | "device-not-found"
  | "device-busy"
  | "constraint-unsatisfied"
  | "request-cancelled"
  | "not-supported"
  | "unknown";

export class MediaCaptureError extends Error {
  constructor(
    readonly code: MediaCaptureErrorCode,
    message: string,
    readonly kind?: "video" | "audio",
    readonly cause?: unknown,
  ) {
    super(message);
    this.name = "MediaCaptureError";
  }
}

function captureError(error: unknown, display: boolean): MediaCaptureError {
  if (error instanceof MediaCaptureError) return error;
  const name = error instanceof Error ? error.name : "";
  const message = error instanceof Error ? error.message : String(error);
  const code: MediaCaptureErrorCode =
    display && name === "NotAllowedError" && /cancel/i.test(message)
      ? "request-cancelled"
      : name === "NotAllowedError" || name === "SecurityError"
        ? "permission-denied"
        : name === "NotFoundError" || name === "DevicesNotFoundError"
          ? "device-not-found"
          : name === "NotReadableError" || name === "TrackStartError"
            ? "device-busy"
            : name === "OverconstrainedError" ||
                name === "ConstraintNotSatisfiedError"
              ? "constraint-unsatisfied"
              : name === "AbortError"
                ? "request-cancelled"
                : name === "NotSupportedError"
                  ? "not-supported"
                  : "unknown";
  return new MediaCaptureError(code, message, undefined, error);
}

export interface MediaDevice {
  readonly id: string;
  readonly label: string;
}

export interface MediaDevicesResult {
  readonly cameras: readonly MediaDevice[];
  readonly microphones: readonly MediaDevice[];
  readonly speakers: readonly MediaDevice[];
  readonly state: "loading" | "ready" | "error";
  readonly error: MediaCaptureError | null;
}

const refreshers = new Set<() => void>();
const EMPTY_DEVICES: MediaDevicesResult = {
  cameras: [],
  microphones: [],
  speakers: [],
  state: "loading",
  error: null,
};

export function useMediaDevices(): MediaDevicesResult {
  const [result, setResult] = useState<MediaDevicesResult>(EMPTY_DEVICES);
  useEffect(() => {
    let live = true;
    let revision = 0;
    const refresh = async () => {
      const requested = ++revision;
      try {
        const devices = await navigator.mediaDevices?.enumerateDevices();
        if (!devices)
          throw new MediaCaptureError(
            "not-supported",
            "device enumeration is unavailable",
          );
        if (!live || requested !== revision) return;
        const normalize = (kind: MediaDeviceKind) =>
          devices
            .filter((device) => device.kind === kind)
            .map((device) => ({ id: device.deviceId, label: device.label }));
        setResult({
          cameras: normalize("videoinput"),
          microphones: normalize("audioinput"),
          speakers: normalize("audiooutput"),
          state: "ready",
          error: null,
        });
      } catch (error) {
        if (live && requested === revision)
          setResult({
            ...EMPTY_DEVICES,
            state: "error",
            error: captureError(error, false),
          });
      }
    };
    const onChange = () => {
      void refresh();
    };
    refreshers.add(onChange);
    navigator.mediaDevices?.addEventListener?.("devicechange", onChange);
    onChange();
    return () => {
      live = false;
      refreshers.delete(onChange);
      navigator.mediaDevices?.removeEventListener?.("devicechange", onChange);
    };
  }, []);
  return result;
}

export interface CaptureResult {
  readonly videoTrack: CapturedVideoTrack | null;
  readonly audioTrack: CapturedAudioTrack | null;
  readonly state: "idle" | "requesting" | "active" | "error";
  readonly error: MediaCaptureError | null;
  request(): Promise<void>;
  stop(): void;
}

export type UserMediaOptions = Readonly<{
  video: boolean | MediaTrackConstraints;
  audio: boolean | MediaTrackConstraints;
}>;
export type DisplayMediaOptions = Readonly<{
  video: boolean | MediaTrackConstraints;
  audio: boolean | MediaTrackConstraints;
}>;

type Session = {
  stream: MediaStream;
  videoTrack: CapturedVideoTrack | null;
  audioTrack: CapturedAudioTrack | null;
  onEnded: () => void;
};

type CaptureSnapshot = Pick<
  CaptureResult,
  "videoTrack" | "audioTrack" | "state" | "error"
>;
const EMPTY_CAPTURE: CaptureSnapshot = {
  videoTrack: null,
  audioTrack: null,
  state: "idle",
  error: null,
};

function useCapture(
  options: UserMediaOptions | DisplayMediaOptions,
  display: boolean,
): CaptureResult {
  const [snapshot, setSnapshot] = useState<CaptureSnapshot>(EMPTY_CAPTURE);
  const current = useRef<Session | null>(null);
  const generation = useRef(0);
  const mounted = useRef(false);
  const optionsRef = useCommittedRef(options);
  const signature = JSON.stringify(options);
  const requestedSignature = useRef<string | null>(null);

  const release = useCallback((session: Session) => {
    for (const track of session.stream.getTracks()) {
      track.removeEventListener("ended", session.onEnded);
      track.stop();
    }
  }, []);

  const stop = useCallback(() => {
    generation.current += 1;
    const previous = current.current;
    current.current = null;
    requestedSignature.current = null;
    if (previous) release(previous);
    if (mounted.current) setSnapshot(EMPTY_CAPTURE);
  }, [release]);

  const request = useCallback(async () => {
    const id = ++generation.current;
    const chosen = { ...optionsRef.current };
    const chosenSignature = JSON.stringify(chosen);
    requestedSignature.current = chosenSignature;
    setSnapshot((before) => ({ ...before, state: "requesting", error: null }));
    let stream: MediaStream;
    try {
      const devices = navigator.mediaDevices;
      if (
        !devices ||
        !(display ? devices.getDisplayMedia : devices.getUserMedia)
      ) {
        throw new MediaCaptureError(
          "not-supported",
          "media capture is unavailable",
        );
      }
      stream = display
        ? await devices.getDisplayMedia(chosen)
        : await devices.getUserMedia(chosen);
    } catch (error) {
      const failure = captureError(error, display);
      const superseded =
        id !== generation.current ||
        (!display && chosenSignature !== JSON.stringify(optionsRef.current));
      if (mounted.current && !superseded) {
        setSnapshot((before) => ({
          ...before,
          state: current.current ? "active" : "error",
          error: failure,
        }));
      }
      throw superseded
        ? new MediaCaptureError(
            "request-cancelled",
            "capture request was superseded",
          )
        : failure;
    }
    if (
      !mounted.current ||
      id !== generation.current ||
      (!display && chosenSignature !== JSON.stringify(optionsRef.current))
    ) {
      for (const track of stream.getTracks()) track.stop();
      throw new MediaCaptureError(
        "request-cancelled",
        "capture request was superseded",
      );
    }
    const video = stream.getVideoTracks()[0];
    const audio = stream.getAudioTracks()[0];
    const onEnded = () => {
      if (current.current?.stream !== stream) return;
      if (
        (display && video?.readyState === "ended") ||
        stream.getTracks().every((track) => track.readyState === "ended")
      ) {
        stop();
      } else {
        setSnapshot((before) => ({
          ...before,
          videoTrack: video?.readyState === "live" ? before.videoTrack : null,
          audioTrack: audio?.readyState === "live" ? before.audioTrack : null,
        }));
      }
    };
    const session: Session = {
      stream,
      videoTrack: video ? createCaptureSource(video, "video") : null,
      audioTrack: audio ? createCaptureSource(audio, "audio") : null,
      onEnded,
    };
    for (const track of stream.getTracks())
      track.addEventListener("ended", onEnded);
    const previous = current.current;
    current.current = session;
    setSnapshot({
      videoTrack: session.videoTrack,
      audioTrack: session.audioTrack,
      state: "active",
      error: null,
    });
    if (previous) release(previous);
    for (const refresh of refreshers) refresh();
  }, [display, release, stop]);

  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
      stop();
    };
  }, [stop]);

  useEffect(() => {
    if (
      !display &&
      requestedSignature.current !== null &&
      requestedSignature.current !== signature
    ) {
      void request().catch(() => {});
    }
  }, [display, request, signature]);

  return useMemo(
    () => ({ ...snapshot, request, stop }),
    [snapshot, request, stop],
  );
}

export function useUserMedia(options: UserMediaOptions): CaptureResult {
  return useCapture(options, false);
}

export function useDisplayMedia(options: DisplayMediaOptions): CaptureResult {
  return useCapture(options, true);
}
