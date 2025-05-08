import { atom, map, MapStore, WritableAtom } from "nanostores";
import * as sfu from "./sfu";

const MAX_DOWNSTREAMS = 9;

export type PulsebeamConnectionState =
  | RTCPeerConnectionState
  | "joining"
  | "signaling"
  | "rpc_ready";

export class PulsebeamClient {
  #pc: RTCPeerConnection | null = null;
  #rpcCh: RTCDataChannel | null = null;
  #sfuUrl: string;
  #maxDownstreams: number;
  #videoSender: RTCRtpTransceiver | null = null;
  #audioSender: RTCRtpTransceiver | null = null;
  /** MIDs of pre-allocated recvonly video and audio transceivers. App can use these to manage slots. */
  public readonly recvOnlyMids: string[] = [];

  // Scoped Nanostores (instance-specific)
  public readonly connectionState: WritableAtom<PulsebeamConnectionState>;
  public readonly localVideoTrack: WritableAtom<MediaStreamTrack | null>;
  public readonly localAudioTrack: WritableAtom<MediaStreamTrack | null>;
  /** Remote tracks keyed by their RTCRtpReceiver's MID. */
  public readonly remoteTracks: MapStore<
    Record<string, MediaStreamTrack | undefined>
  >;
  /** Optional: An error message specifically reported by the SFU via RPC. */
  public readonly sfuErrorMessage: WritableAtom<string | null>;

  constructor(sfuUrl: string, maxDownstreams = 9) {
    this.#sfuUrl = sfuUrl;
    this.#maxDownstreams = Math.min(maxDownstreams, MAX_DOWNSTREAMS);
    this.connectionState = atom<PulsebeamConnectionState>("new");
    this.localVideoTrack = atom<MediaStreamTrack | null>(null);
    this.localAudioTrack = atom<MediaStreamTrack | null>(null);
    this.remoteTracks = map<Record<string, MediaStreamTrack | undefined>>({});
    this.sfuErrorMessage = atom<string | null>(null);
  }

  #handleSfuRpcMessage(
    msg: ReturnType<typeof sfu.ServerMessage.fromBinary>,
  ): void {
    console.debug("Pulsebeam SFU RPC Rcvd (internal):", msg);
    // if (msg.type === "error" && msg.payload?.message) {
    //   console.error("Pulsebeam SFU Reported Error:", msg.payload.message);
    //   this.sfuErrorMessage.set(msg.payload.message);
    // }
    // Example: SFU confirms a track is no longer available for a MID
    // if (msg.type === 'trackSubscriptionEnded' && msg.payload?.mid) {
    //   const track = this.remoteTracks.get()[msg.payload.mid];
    //   track?.stop();
    //   this.remoteTracks.setKey(msg.payload.mid, undefined); // Clear it from the map
    // }
    // Add other internal handling of SFU-initiated messages here
  }

  async connect(room: string, participantId: string): Promise<void> {
    if (
      this.#pc && this.connectionState.get() !== "closed" &&
      this.connectionState.get() !== "failed"
    ) {
      console.warn(
        "Already connected or connecting. Disconnect first or wait.",
      );
      return;
    }
    this.disconnect(true); // Reset internal state before connecting
    this.connectionState.set("joining");
    this.sfuErrorMessage.set(null); // Clear previous SFU errors
    this.#pc = new RTCPeerConnection();
    const pc = this.#pc; // Alias for brevity

    pc.onconnectionstatechange = () =>
      this.connectionState.set(pc.connectionState);
    pc.ontrack = (ev) => {
      if (ev.transceiver.mid && ev.track) {
        this.remoteTracks.setKey(ev.transceiver.mid, ev.track);
        ev.track.onended = () =>
          this.remoteTracks.setKey(ev.transceiver.mid!, undefined); // Clear on track end
        ev.track.onmute =
          () => {/* Track still exists, app can check track.muted */ };
      }
    };

    this.#rpcCh = pc.createDataChannel("rpc", { negotiated: false }); // SDP negotiated
    this.#rpcCh.binaryType = "arraybuffer";
    this.#rpcCh.onopen = () => this.connectionState.set("rpc_ready");
    this.#rpcCh.onmessage = (ev) =>
      this.#handleSfuRpcMessage(
        sfu.ServerMessage.fromBinary(new Uint8Array(ev.data as ArrayBuffer)),
      );
    this.#rpcCh.onclose = () => {
      if (this.connectionState.get() === "rpc_ready") {
        this.connectionState.set(pc.connectionState || "closed");
      }
    };

    this.#videoSender = pc.addTransceiver("video", { direction: "sendonly" });
    this.#audioSender = pc.addTransceiver("audio", { direction: "sendonly" });

    this.recvOnlyMids.length = 0; // Clear previous mids
    for (let i = 0; i < this.#maxDownstreams; i++) {
      const vt = pc.addTransceiver("video", { direction: "recvonly" });
      const at = pc.addTransceiver("audio", { direction: "recvonly" });
      if (vt.mid) this.recvOnlyMids.push(vt.mid);
      if (at.mid) this.recvOnlyMids.push(at.mid);
    }

    this.connectionState.set("signaling");
    const offer = await pc.createOffer();
    await pc.setLocalDescription(offer);

    try {
      const answerSdp = await fetch(
        `${this.#sfuUrl}?room=${room}&participant=${participantId}`,
        {
          method: "POST",
          body: offer.sdp!,
          headers: { "Content-Type": "application/sdp" },
        },
      ).then((r) => {
        if (!r.ok) throw new Error(`SFU Signal Error: ${r.status}`);
        return r.text();
      });
      await pc.setRemoteDescription({ type: "answer", sdp: answerSdp });
    } catch (e: any) {
      console.error("Signaling failed:", e.message);
      this.connectionState.set("failed");
      this.sfuErrorMessage.set(e.message);
      this.disconnect();
    }
  }

  sendRpc(payload: sfu.ClientMessage): void {
    if (this.#rpcCh?.readyState === "open") {
      this.#rpcCh.send(sfu.ClientMessage.toBinary(payload));
    } else {
      console.warn("RPC channel not open.");
      this.sfuErrorMessage.set("RPC channel not open to send.");
    }
  }

  async publishTrack(track: MediaStreamTrack): Promise<void> {
    const trans = track.kind === "video"
      ? this.#videoSender
      : this.#audioSender;
    const store = track.kind === "video"
      ? this.localVideoTrack
      : this.localAudioTrack;
    if (trans) {
      await trans.sender.replaceTrack(track);
      store.set(track);
    }
  }

  async unpublishTrack(kind: "video" | "audio"): Promise<void> {
    const trans = kind === "video" ? this.#videoSender : this.#audioSender;
    const store = kind === "video"
      ? this.localVideoTrack
      : this.localAudioTrack;
    if (trans) await trans.sender.replaceTrack(null);
    store.get()?.stop();
    store.set(null);
  }

  disconnect(internalCall = false): void {
    if (!internalCall) console.log("Disconnecting...");
    this.localVideoTrack.get()?.stop();
    this.localAudioTrack.get()?.stop();
    this.localVideoTrack.set(null);
    this.localAudioTrack.set(null);
    Object.values(this.remoteTracks.get()).forEach((track) => track?.stop());
    this.remoteTracks.set({});

    if (this.#rpcCh) {
      this.#rpcCh.onopen = this.#rpcCh.onmessage = this.#rpcCh.onclose = null;
      if (this.#rpcCh.readyState === "open") this.#rpcCh.close();
      this.#rpcCh = null;
    }
    if (this.#pc) {
      this.#pc.onconnectionstatechange = this.#pc.ontrack = null;
      this.#pc.getSenders().forEach((s) => {
        try {
          this.#pc?.removeTrack(s);
        } catch (e) { }
      }); // Clean up senders
      if (this.#pc.signalingState !== "closed") this.#pc.close();
      this.#pc = null;
    }
    this.#videoSender = null;
    this.#audioSender = null;
    this.recvOnlyMids.length = 0;
    if (!internalCall || this.connectionState.get() !== "new") {
      this.connectionState.set("closed");
    }
    if (!internalCall) this.sfuErrorMessage.set(null); // Clear SFU error on user-initiated disconnect
  }
}
