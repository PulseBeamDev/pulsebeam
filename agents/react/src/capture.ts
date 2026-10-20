import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
} from "react";
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

function captureError(error: unknown): MediaCaptureError {
  if (error instanceof MediaCaptureError) return error;
  const name = error instanceof Error ? error.name : "";
  const message = error instanceof Error ? error.message : String(error);
  const code: MediaCaptureErrorCode =
    name === "NotAllowedError" || name === "SecurityError"
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

export interface CameraDevice {
  readonly kind: "camera";
  readonly id: string;
  readonly label: string;
}
export interface MicrophoneDevice {
  readonly kind: "microphone";
  readonly id: string;
  readonly label: string;
}
export interface SpeakerDevice {
  readonly kind: "speaker";
  readonly id: string;
  readonly label: string;
}

export interface MediaDevicesResult {
  readonly cameras: readonly CameraDevice[];
  readonly microphones: readonly MicrophoneDevice[];
  readonly speakers: readonly SpeakerDevice[];
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
    const permissions: PermissionStatus[] = [];
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
        setResult({
          cameras: devices
            .filter((d) => d.kind === "videoinput")
            .map((d) => ({ kind: "camera", id: d.deviceId, label: d.label })),
          microphones: devices
            .filter((d) => d.kind === "audioinput")
            .map((d) => ({
              kind: "microphone",
              id: d.deviceId,
              label: d.label,
            })),
          speakers: devices
            .filter((d) => d.kind === "audiooutput")
            .map((d) => ({ kind: "speaker", id: d.deviceId, label: d.label })),
          state: "ready",
          error: null,
        });
      } catch (error) {
        if (live && requested === revision)
          setResult({
            ...EMPTY_DEVICES,
            state: "error",
            error: captureError(error),
          });
      }
    };
    const onChange = () => {
      void refresh();
    };
    refreshers.add(onChange);
    navigator.mediaDevices?.addEventListener?.("devicechange", onChange);
    for (const name of ["camera", "microphone"] as const) {
      void navigator.permissions
        ?.query({ name: name as PermissionName })
        .then((status) => {
          if (!live) return;
          permissions.push(status);
          status.addEventListener("change", onChange);
        })
        .catch(() => {});
    }
    onChange();
    return () => {
      live = false;
      refreshers.delete(onChange);
      navigator.mediaDevices?.removeEventListener?.("devicechange", onChange);
      for (const status of permissions)
        status.removeEventListener("change", onChange);
    };
  }, []);
  return result;
}

export interface VideoCapturePreferences {
  readonly width?: number;
  readonly height?: number;
  readonly frameRate?: number;
}
export interface AudioCapturePreferences {
  readonly echoCancellation?: boolean;
  readonly noiseSuppression?: boolean;
  readonly autoGainControl?: boolean;
}
export interface VideoCaptureOptions {
  readonly device?: CameraDevice;
  readonly preferences?: VideoCapturePreferences;
}
export interface AudioCaptureOptions {
  readonly device?: MicrophoneDevice;
  readonly preferences?: AudioCapturePreferences;
}
export type UserMediaOptions = Readonly<{
  video: boolean | VideoCaptureOptions;
  audio: boolean | AudioCaptureOptions;
}>;
export type DisplayMediaOptions = Readonly<{
  video: boolean | { readonly preferences?: VideoCapturePreferences };
  audio: boolean | { readonly preferences?: AudioCapturePreferences };
}>;
export interface CapturedVideo {
  readonly device: CameraDevice | null;
  readonly settings: VideoCapturePreferences;
}
export interface CapturedAudio {
  readonly device: MicrophoneDevice | null;
  readonly settings: AudioCapturePreferences;
}
export interface CaptureResult {
  readonly videoTrack: CapturedVideoTrack | null;
  readonly audioTrack: CapturedAudioTrack | null;
  readonly video: CapturedVideo | null;
  readonly audio: CapturedAudio | null;
  readonly state: "idle" | "requesting" | "active" | "error";
  readonly error: MediaCaptureError | null;
  request(): Promise<void>;
  stop(): void;
}

type CaptureSnapshot = Omit<CaptureResult, "request" | "stop">;
const EMPTY_CAPTURE: CaptureSnapshot = {
  videoTrack: null,
  audioTrack: null,
  video: null,
  audio: null,
  state: "idle",
  error: null,
};
type Session = {
  stream: MediaStream;
  snapshot: CaptureSnapshot;
  onEnded: () => void;
};
const cancelled = () =>
  new MediaCaptureError(
    "request-cancelled",
    "capture request was superseded or released",
  );

function signature(options: UserMediaOptions | DisplayMediaOptions): string {
  return JSON.stringify(
    ["video", "audio"].map((kind) => {
      const input = options[kind as "video" | "audio"];
      if (input === false) return false;
      const value: VideoCaptureOptions | AudioCaptureOptions =
        input === true ? {} : input;
      const device =
        "device" in value
          ? (value.device as CameraDevice | MicrophoneDevice | undefined)
          : undefined;
      const p = value.preferences;
      return [
        "device" in value && value.device !== undefined,
        device?.kind,
        device?.id,
        ...[
          "width",
          "height",
          "frameRate",
          "echoCancellation",
          "noiseSuppression",
          "autoGainControl",
        ].map((k) => p?.[k as keyof typeof p]),
      ];
    }),
    (_, value) =>
      typeof value === "number" && !Number.isFinite(value)
        ? String(value)
        : value,
  );
}

function constraints(
  options: UserMediaOptions | DisplayMediaOptions,
  display: boolean,
): MediaStreamConstraints {
  if (display && !options.video)
    throw new MediaCaptureError(
      "not-supported",
      "display capture requires video",
      "video",
    );
  const translate = (
    kind: "video" | "audio",
  ): boolean | MediaTrackConstraints => {
    const value = options[kind];
    if (typeof value === "boolean") return value;
    const device =
      "device" in value
        ? (value.device as CameraDevice | MicrophoneDevice | undefined)
        : undefined;
    if (
      Object.keys(value).some(
        (key) => key !== "preferences" && (display || key !== "device"),
      )
    )
      throw new MediaCaptureError(
        "constraint-unsatisfied",
        "unsupported capture option",
        kind,
      );
    if (
      device !== undefined &&
      (!device ||
        display ||
        device.kind !== (kind === "video" ? "camera" : "microphone") ||
        typeof device.id !== "string" ||
        !device.id.trim())
    ) {
      throw new MediaCaptureError(
        "device-not-found",
        "invalid exact capture device",
        kind,
      );
    }
    const result: MediaTrackConstraints = {};
    if (device) result.deviceId = { exact: device.id };
    const preferences = value.preferences ?? {};
    for (const [key, preference] of Object.entries(preferences)) {
      if (preference === undefined) continue;
      const valid =
        kind === "video"
          ? ["width", "height", "frameRate"].includes(key) &&
            typeof preference === "number" &&
            Number.isFinite(preference) &&
            preference > 0
          : [
              "echoCancellation",
              "noiseSuppression",
              "autoGainControl",
            ].includes(key) && typeof preference === "boolean";
      if (!valid)
        throw new MediaCaptureError(
          "constraint-unsatisfied",
          `invalid ${key} preference`,
          kind,
        );
      Object.assign(result, { [key]: { ideal: preference } });
    }
    return result;
  };
  return { video: translate("video"), audio: translate("audio") };
}

function observe(
  track: MediaStreamTrack,
  kind: "video" | "audio",
  options: UserMediaOptions | DisplayMediaOptions,
  display: boolean,
): CapturedVideo | CapturedAudio {
  const actual = track.getSettings();
  const chosen = options[kind];
  const selected =
    typeof chosen === "object" && "device" in chosen
      ? (chosen.device as CameraDevice | MicrophoneDevice | undefined)
      : undefined;
  if (selected && actual.deviceId && actual.deviceId !== selected.id)
    throw new MediaCaptureError(
      "device-not-found",
      "capture returned a different device",
      kind,
    );
  const device =
    !display && actual.deviceId
      ? {
          kind:
            kind === "video" ? ("camera" as const) : ("microphone" as const),
          id: actual.deviceId,
          label: track.label ?? "",
        }
      : null;
  const settings: Record<string, number | boolean> = {};
  for (const key of kind === "video"
    ? (["width", "height", "frameRate"] as const)
    : (["echoCancellation", "noiseSuppression", "autoGainControl"] as const)) {
    const value = actual[key];
    if (
      kind === "video"
        ? typeof value === "number" && Number.isFinite(value)
        : typeof value === "boolean"
    )
      settings[key] = value!;
  }
  return { device, settings } as CapturedVideo | CapturedAudio;
}

function useCapture(
  options: UserMediaOptions | DisplayMediaOptions,
  display: boolean,
): CaptureResult {
  const [snapshot, setSnapshot] = useState<CaptureSnapshot>(EMPTY_CAPTURE);
  const current = useRef<Session | null>(null);
  const retired = useRef(new Set<Session>());
  const generation = useRef(0);
  const mounted = useRef(false);
  const optionsRef = useCommittedRef(options);
  const key = signature(options);
  const requested = useRef<string | null>(null);
  const cancelPending = useRef<(() => void) | null>(null);
  const committed = useRef<{
    snapshot: CaptureSnapshot;
    resolve(): void;
    reject(error: unknown): void;
  } | null>(null);

  const release = useCallback((session: Session) => {
    for (const track of session.stream.getTracks()) {
      track.removeEventListener("ended", session.onEnded);
      track.stop();
    }
  }, []);
  const stop = useCallback(() => {
    generation.current++;
    requested.current = null;
    cancelPending.current?.();
    cancelPending.current = null;
    committed.current?.reject(cancelled());
    committed.current = null;
    const previous = current.current;
    current.current = null;
    if (previous) release(previous);
    for (const session of retired.current) release(session);
    retired.current.clear();
    if (mounted.current) setSnapshot(EMPTY_CAPTURE);
  }, [release]);

  const request = useCallback(async () => {
    if (!mounted.current) throw cancelled();
    const id = ++generation.current;
    cancelPending.current?.();
    cancelPending.current = null;
    committed.current?.reject(cancelled());
    committed.current = null;
    const chosen = optionsRef.current;
    const chosenKey = signature(chosen);
    requested.current = chosenKey;
    if (!display && !chosen.video && !chosen.audio) {
      stop();
      return;
    }
    setSnapshot((before) => ({ ...before, state: "requesting", error: null }));
    let stream: MediaStream | undefined;
    const stale = () => !mounted.current || id !== generation.current;
    try {
      const translated = constraints(chosen, display);
      const devices = navigator.mediaDevices;
      if (
        !devices ||
        !(display ? devices.getDisplayMedia : devices.getUserMedia)
      )
        throw new MediaCaptureError(
          "not-supported",
          "media capture is unavailable",
        );
      const interrupted = new Promise<never>((_, reject) => {
        cancelPending.current = () => reject(cancelled());
      });
      const acquisition = (
        display
          ? devices.getDisplayMedia(translated)
          : devices.getUserMedia(translated)
      ).then((candidate) => {
        if (stale()) {
          for (const track of candidate.getTracks()) track.stop();
          throw cancelled();
        }
        return candidate;
      });
      stream = await Promise.race([acquisition, interrupted]);
      if (stale()) throw cancelled();
      cancelPending.current = null;
      const video = stream.getVideoTracks()[0];
      const audio = stream.getAudioTracks()[0];
      for (const kind of ["video", "audio"] as const) {
        if (
          chosen[kind] &&
          (kind === "video" ? video : audio)?.readyState !== "live" &&
          !(display && kind === "audio")
        )
          throw new MediaCaptureError(
            "device-not-found",
            `capture returned no ${kind}`,
            kind,
          );
      }
      const next: CaptureSnapshot = {
        videoTrack:
          video?.readyState === "live"
            ? createCaptureSource(video, "video")
            : null,
        audioTrack:
          audio?.readyState === "live"
            ? createCaptureSource(audio, "audio")
            : null,
        video:
          video?.readyState === "live"
            ? (observe(video, "video", chosen, display) as CapturedVideo)
            : null,
        audio:
          audio?.readyState === "live"
            ? (observe(audio, "audio", chosen, display) as CapturedAudio)
            : null,
        state: "active",
        error: null,
      };
      const session: Session = {
        stream,
        snapshot: next,
        onEnded: () => {
          if (current.current !== session) return;
          if (
            (display && video?.readyState === "ended") ||
            session.stream.getTracks().every((t) => t.readyState === "ended")
          ) {
            stop();
            return;
          }
          const previousSnapshot = session.snapshot;
          if (video?.readyState === "ended") {
            session.snapshot = {
              ...session.snapshot,
              videoTrack: null,
              video: null,
            };
          }
          if (audio?.readyState === "ended") {
            session.snapshot = {
              ...session.snapshot,
              audioTrack: null,
              audio: null,
            };
          }
          if (committed.current?.snapshot === previousSnapshot)
            committed.current.snapshot = session.snapshot;
          setSnapshot(session.snapshot);
        },
      };
      for (const track of stream.getTracks())
        track.addEventListener("ended", session.onEnded);
      const previous = current.current;
      current.current = session;
      const completion = new Promise<void>((resolve, reject) => {
        committed.current = { snapshot: next, resolve, reject };
      });
      setSnapshot(next);
      if (previous) retired.current.add(previous);
      for (const refresh of refreshers) refresh();
      await completion;
    } catch (error) {
      if (stream && current.current?.stream !== stream)
        for (const track of stream.getTracks()) track.stop();
      if (stale()) throw cancelled();
      cancelPending.current = null;
      const failure = captureError(error);
      if (!current.current) requested.current = null;
      setSnapshot((before) => ({
        ...before,
        state: current.current ? "active" : "error",
        error: failure,
      }));
      throw failure;
    }
  }, [display, optionsRef, release, stop]);

  useLayoutEffect(() => {
    mounted.current = true;
    if (!current.current) setSnapshot(EMPTY_CAPTURE);
    return () => {
      mounted.current = false;
      stop();
    };
  }, [stop]);
  useLayoutEffect(() => {
    if (committed.current?.snapshot === snapshot) {
      for (const session of retired.current) release(session);
      retired.current.clear();
      committed.current.resolve();
      committed.current = null;
    }
  }, [snapshot, release]);
  useEffect(() => {
    if (!display && requested.current !== null && requested.current !== key)
      void request().catch(() => {});
  }, [display, key, request]);

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
