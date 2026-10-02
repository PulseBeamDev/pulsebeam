import type { AgentSnapshot, LogLevel } from "./types.js";

interface Route {
  readonly publicationId: string;
  readonly track: MediaStreamTrack;
  readonly source: MediaStreamAudioSourceNode;
  readonly analyser: AnalyserNode;
  readonly waveform: Float32Array<ArrayBuffer>;
  readonly spectrum: Float32Array<ArrayBuffer>;
}

/** One decoded, audible route per negotiated receiver, owned by one Agent. */
export class RemoteAudioPlayback {
  #context: AudioContext | undefined;
  #decoder: HTMLAudioElement | undefined;
  #routes = new Map<number, Route>();
  #closed = false;

  constructor(private readonly logLevel: LogLevel) {}

  #contextForPlayback(): AudioContext {
    if (this.#closed) throw new Error("agent is closed");
    return (this.#context ??= new AudioContext());
  }

  async resume(): Promise<void> {
    const context = this.#contextForPlayback();
    // Start both operations in the caller's gesture, before awaiting either.
    await Promise.all([
      context.resume(),
      this.#decoder?.srcObject ? this.#decoder.play() : undefined,
    ]);
    if (this.#closed) throw new Error("agent is closed");
    if (context.state !== "running")
      throw new Error("audio playback is not running");
  }

  update(snapshot: AgentSnapshot): void {
    if (this.#closed) return;
    const bindings = new Map(
      snapshot.mapping.audio.map((entry) => [entry.receiverIndex, entry]),
    );
    const available = snapshot.connection === "connected";
    for (const [index, route] of this.#routes) {
      const binding = bindings.get(index);
      if (
        available &&
        binding?.publicationId === route.publicationId &&
        snapshot.tracks[route.publicationId]?.media === route.track
      )
        continue;
      route.source.disconnect();
      route.analyser.disconnect();
      this.#routes.delete(index);
    }
    if (!available) {
      this.#syncDecoder();
      return;
    }
    for (const [index, binding] of bindings) {
      const track = snapshot.tracks[binding.publicationId]?.media;
      if (
        this.#routes.has(index) ||
        !track ||
        track.kind !== "audio" ||
        track.readyState === "ended"
      )
        continue;
      try {
        const context = this.#contextForPlayback();
        const source = context.createMediaStreamSource(
          new MediaStream([track]),
        );
        const analyser = context.createAnalyser();
        source.connect(analyser);
        analyser.connect(context.destination);
        this.#routes.set(index, {
          publicationId: binding.publicationId,
          track,
          source,
          analyser,
          waveform: new Float32Array(analyser.fftSize),
          spectrum: new Float32Array(analyser.frequencyBinCount),
        });
      } catch (error) {
        this.#warn(error);
      }
    }
    try {
      const changed = this.#syncDecoder();
      const context = this.#context;
      if (!context || !this.#routes.size) return;
      if (context.state !== "running")
        this.#warn(new Error("browser audio context is suspended"));
      if (changed || context.state !== "running" || this.#decoder?.paused)
        void this.resume().catch((error: unknown) => this.#warn(error));
    } catch (error) {
      this.#warn(error);
    }
  }

  #syncDecoder(): boolean {
    const tracks = new Set(
      [...this.#routes.values()].map(({ track }) => track),
    );
    if (!tracks.size) {
      this.#decoder?.pause();
      if (this.#decoder) this.#decoder.srcObject = null;
      return false;
    }
    if (!this.#decoder) {
      // Chrome's WebRTC decoder needs a native renderer even when Web Audio is
      // consuming the track. This detached element is never an audible route.
      const decoder = document.createElement("audio");
      decoder.muted = true;
      decoder.defaultMuted = true;
      decoder.hidden = true;
      this.#decoder = decoder;
    }
    const decoder = this.#decoder;
    const stream = decoder.srcObject as MediaStream | null;
    if (!stream) {
      decoder.srcObject = new MediaStream([...tracks]);
      return true;
    }
    let changed = false;
    for (const track of stream.getAudioTracks()) {
      if (tracks.has(track)) continue;
      stream.removeTrack(track);
      changed = true;
    }
    for (const track of tracks) {
      if (stream.getAudioTracks().includes(track)) continue;
      stream.addTrack(track);
      changed = true;
    }
    return changed;
  }

  read(
    publicationId: string | undefined,
    buffer: Float32Array,
    spectrum: boolean,
  ): void {
    buffer.fill(spectrum ? -Infinity : 0);
    if (!buffer.length || this.#closed || this.#context?.state !== "running")
      return;
    const route = [...this.#routes.values()].find(
      (route) => route.publicationId === publicationId,
    );
    if (!route || route.track.muted || route.track.readyState !== "live")
      return;
    const samples = spectrum ? route.spectrum : route.waveform;
    if (spectrum) route.analyser.getFloatFrequencyData(samples);
    else route.analyser.getFloatTimeDomainData(samples);
    buffer.set(samples.subarray(0, buffer.length));
  }

  #warn(error: unknown): void {
    if (this.#closed || this.logLevel === "off" || this.logLevel === "error")
      return;
    console.warn(
      "PulseBeam remote audio playback unavailable; call agent.remote.resumeAudio() from a user gesture",
      error,
    );
  }

  close(): void {
    if (this.#closed) return;
    this.#closed = true;
    for (const route of this.#routes.values()) {
      route.source.disconnect();
      route.analyser.disconnect();
    }
    this.#routes.clear();
    this.#syncDecoder();
    this.#decoder = undefined;
    const context = this.#context;
    this.#context = undefined;
    if (context) void context.close().catch(() => {});
  }
}
