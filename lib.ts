import {
  atom,
  computed,
  map,
  MapStore,
  ReadableAtom,
  WritableAtom,
} from "nanostores";
import {
  ClientMessage,
  // The MessageType instances for toBinary/fromBinary
  ClientMessage as ClientMessageType,
  // Specific payload types if needed for constructing messages,
  // though ClientMessage.create allows direct oneof assignment
  ClientSubscribePayload,
  ClientUnsubscribePayload,
  ErrorPayload,
  ServerMessage,
  ServerMessage as ServerMessageType,
  // Server payload types for type casting in #handleSfuRpcMessage
  SubscriptionErrorPayload,
  TrackKind,
  TrackPublishedPayload,
  TrackUnpublishedPayload,
} from "./sfu.ts";

export type PulsebeamConnectionState =
  | RTCPeerConnectionState
  | "joining"
  | "signaling"
  | "rpc_ready";

export interface RemoteTrackInfo {
  slotId: string; // The MID this track is using
  remoteTrackId: string; // Application/SFU level track ID
  kind: "video" | "audio"; // Mapped from TrackKind enum
  track: MediaStreamTrack | null;
  state: "requested" | "active" | "ended" | "error";
  error?: string;
  // participantId?: string;
}

export class PulsebeamClient {
  #pc: RTCPeerConnection | null = null;
  #rpcCh: RTCDataChannel | null = null;
  #sfuUrl: string;
  #maxDownstreams: number;

  #videoSender: RTCRtpSender | null = null;
  #audioSender: RTCRtpSender | null = null;

  #videoRecvMids: string[] = [];
  #audioRecvMids: string[] = [];
  #usedMids: Set<string> = new Set();
  #activeSubscriptions = new Map<string, { slotId: string; kind: TrackKind }>();
  #isConnected: ReadableAtom<boolean>; // Let's try a computed one first

  // --- Scoped Nanostores ---
  public readonly connectionState: WritableAtom<PulsebeamConnectionState>;
  public readonly localVideoTrack: WritableAtom<MediaStreamTrack | null>;
  public readonly localAudioTrack: WritableAtom<MediaStreamTrack | null>;
  public readonly remoteTrackInfos: MapStore<Record<string, RemoteTrackInfo>>;
  public readonly sfuErrorMessage: WritableAtom<string | null>;

  constructor(sfuUrl: string, maxDownstreams = 9) {
    this.#sfuUrl = sfuUrl;
    this.#maxDownstreams = maxDownstreams;

    this.connectionState = atom<PulsebeamConnectionState>("new");
    this.localVideoTrack = atom<MediaStreamTrack | null>(null);
    this.localAudioTrack = atom<MediaStreamTrack | null>(null);
    this.remoteTrackInfos = map<Record<string, RemoteTrackInfo>>({});
    this.sfuErrorMessage = atom<string | null>(null);
    this.#isConnected = computed(
      this.connectionState,
      (state) => state === "connected" || state === "rpc_ready",
    );
  }

  #getFreeMid(kind: TrackKind): string | undefined {
    const midsPool = kind === TrackKind.VIDEO
      ? this.#videoRecvMids
      : this.#audioRecvMids;
    return midsPool.find((mid) => !this.#usedMids.has(mid));
  }

  #releaseMid(mid: string): void {
    this.#usedMids.delete(mid);
  }
  #allocateMid(mid: string): void {
    this.#usedMids.add(mid);
  } // Simplified: assumes mid is valid

  #mapProtoTrackKindToAppKind(kind: TrackKind): "video" | "audio" {
    return kind === TrackKind.VIDEO ? "video" : "audio";
  }
  #mapAppKindToProtoTrackKind(kind: "video" | "audio"): TrackKind {
    return kind === "video" ? TrackKind.VIDEO : TrackKind.AUDIO;
  }

  #handleSfuRpcMessage(sfuMessage: ServerMessage): void {
    console.debug("Pulsebeam SFU RPC Rcvd (internal):", sfuMessage);

    if (sfuMessage.payload.oneofKind === undefined) {
      console.warn("Received SFU message with undefined payload kind");
      return;
    }

    const payloadKind = sfuMessage.payload.oneofKind;
    const payloadData = sfuMessage.payload[payloadKind];

    switch (payloadKind) {
      case "error":
        this.sfuErrorMessage.set(
          (payloadData as ErrorPayload).message || "Unknown SFU error via RPC",
        );
        break;
      case "subscriptionError": {
        const errorPayload = payloadData as SubscriptionErrorPayload;
        const { remoteTrackId, message, mid } = errorPayload;
        const targetTrackId = remoteTrackId ||
          (mid
            ? Array.from(this.#activeSubscriptions.entries()).find(([, val]) =>
              val.slotId === mid
            )?.[0]
            : undefined);

        if (targetTrackId) {
          const currentInfo = this.remoteTrackInfos.get()[targetTrackId];
          if (currentInfo) {
            this.remoteTrackInfos.setKey(targetTrackId, {
              ...currentInfo,
              state: "error",
              error: message,
            });
            this.#releaseMid(currentInfo.slotId);
            this.#activeSubscriptions.delete(targetTrackId);
          }
        }
        this.sfuErrorMessage.set(
          `Subscription error for ${targetTrackId || "unknown"}: ${message}`,
        );
        break;
      }
      case "trackPublished": {
        const { remoteTrackId, kind, participantId } =
          payloadData as TrackPublishedPayload;
        // This is informational for the app. The app can choose to subscribe.
        // It could emit an event or update a separate store of available tracks.
        console.log(
          `SFU: Track Published - ID: ${remoteTrackId}, Kind: ${TrackKind[kind]
          }, By: ${participantId}`,
        );
        // Example: client.emit('trackAvailable', { remoteTrackId, kind, participantId });
        break;
      }
      case "trackUnpublished": {
        const { remoteTrackId } = payloadData as TrackUnpublishedPayload;
        if (remoteTrackId) {
          console.log(
            `SFU: Track Unpublished - ID: ${remoteTrackId}. Cleaning up subscription.`,
          );
          this.unsubscribe(remoteTrackId); // SFU says it's gone, so clean up our side.
        }
        break;
      }
      case "subscriptionOffer":
        // The current design doesn't expect proactive subscription offers from the SFU,
        // but if it did, you'd handle it here.
        console.log(
          "SFU: Received Subscription Offer (not actively handled in this SDK version):",
          payloadData,
        );
        break;
    }
  }

  async connect(room: string, participantId: string): Promise<void> {
    if (!participantId) {
      throw new Error("participantId is required to connect.");
    }
    if (
      this.#isConnected.get() && this.connectionState.get() !== "closed" &&
      this.connectionState.get() !== "failed"
    ) {
      console.warn("Already connected or connecting. Disconnect first.");
      return;
    }
    this.disconnect(true);
    this.connectionState.set("joining");
    this.sfuErrorMessage.set(null);

    this.#pc = new RTCPeerConnection();
    const pc = this.#pc;

    pc.onconnectionstatechange = () => {
      const newState = pc.connectionState;
      this.connectionState.set(newState);
      if (
        newState === "failed" || newState === "closed" ||
        newState === "disconnected"
      ) this.disconnect();
    };

    pc.ontrack = (event: RTCTrackEvent) => {
      const mid = event.transceiver?.mid;
      const track = event.track;
      if (!mid || !track) return;

      let foundRemoteTrackId: string | undefined;
      for (const [rtId, subInfo] of this.#activeSubscriptions.entries()) {
        if (
          subInfo.slotId === mid &&
          subInfo.kind ===
          this.#mapAppKindToProtoTrackKind(track.kind as "video" | "audio")
        ) {
          foundRemoteTrackId = rtId;
          break;
        }
      }

      if (foundRemoteTrackId) {
        this.remoteTrackInfos.setKey(foundRemoteTrackId, {
          slotId: mid,
          remoteTrackId: foundRemoteTrackId,
          kind: track.kind as "video" | "audio",
          track: track,
          state: "active",
        });
        track.onended = () => {
          const currentInfo = this.remoteTrackInfos.get()[foundRemoteTrackId!];
          if (currentInfo && currentInfo.state === "active") {
            this.remoteTrackInfos.setKey(foundRemoteTrackId!, {
              ...currentInfo,
              track: null,
              state: "ended",
            });
            this.#releaseMid(mid);
            this.#activeSubscriptions.delete(foundRemoteTrackId!);
          }
        };
      } else {
        track.stop();
        console.warn(`Track on MID ${mid} without matching subscription.`);
      }
    };

    this.#rpcCh = pc.createDataChannel("rpc");
    this.#rpcCh.binaryType = "arraybuffer";
    this.#rpcCh.onopen = () => this.connectionState.set("rpc_ready");
    this.#rpcCh.onmessage = (ev) => {
      const sfuMessage = ServerMessageType.fromBinary(
        new Uint8Array(ev.data as ArrayBuffer),
      );
      this.#handleSfuRpcMessage(sfuMessage);
    };
    this.#rpcCh.onclose = () => {
      if (this.#isConnected.get() && pc.connectionState !== "closed") {
        this.connectionState.set(pc.connectionState || "disconnected");
      }
    };
    this.#rpcCh.onerror = (ev) => console.error("RPC Channel Error:", ev);

    this.#videoSender =
      pc.addTransceiver("video", { direction: "sendonly" }).sender;
    this.#audioSender =
      pc.addTransceiver("audio", { direction: "sendonly" }).sender;

    this.#videoRecvMids = [];
    this.#audioRecvMids = [];
    this.#usedMids.clear();
    this.#activeSubscriptions.clear();
    for (let i = 0; i < this.#maxDownstreams; i++) {
      const vt = pc.addTransceiver("video", { direction: "recvonly" });
      const at = pc.addTransceiver("audio", { direction: "recvonly" });
      if (vt.mid) this.#videoRecvMids.push(vt.mid);
      if (at.mid) this.#audioRecvMids.push(at.mid);
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
        if (!r.ok) {
          throw new Error(`SFU Signal Error: ${r.status} ${r.statusText}`);
        }
        return r.text();
      });
      await pc.setRemoteDescription({ type: "answer", sdp: answerSdp });
    } catch (e: any) {
      this.sfuErrorMessage.set(`Signaling failed: ${e.message}`);
      this.connectionState.set("failed"); // Triggers disconnect
    }
  }

  sendRpc(message: ClientMessage): void {
    if (this.#rpcCh?.readyState === "open") {
      const binaryMessage = ClientMessageType.toBinary(message);
      this.#rpcCh.send(binaryMessage);
    } else {
      this.sfuErrorMessage.set("RPC channel not open to send message.");
      console.warn("RPC channel not open. Cannot send:", message);
    }
  }

  subscribe(remoteTrackId: string, kind: "video" | "audio"): boolean {
    if (
      !this.#isConnected.get() || this.connectionState.get() !== "rpc_ready"
    ) {
      this.sfuErrorMessage.set("Not connected or RPC not ready to subscribe.");
      return false;
    }
    if (this.#activeSubscriptions.has(remoteTrackId)) {
      console.warn(
        `Already subscribed or pending for remoteTrackId: ${remoteTrackId}`,
      );
      return true;
    }

    const protoKind = this.#mapAppKindToProtoTrackKind(kind);
    const mid = this.#getFreeMid(protoKind);
    if (!mid) {
      this.sfuErrorMessage.set(`No free ${kind} slots available.`);
      return false;
    }

    this.#allocateMid(mid);
    this.#activeSubscriptions.set(remoteTrackId, {
      slotId: mid,
      kind: protoKind,
    });
    this.remoteTrackInfos.setKey(remoteTrackId, {
      slotId: mid,
      remoteTrackId,
      kind,
      track: null,
      state: "requested",
    });

    const subscribePayload: ClientSubscribePayload = {
      mid,
      trackId: remoteTrackId,
      kind: protoKind,
    };
    this.sendRpc(
      ClientMessageType.create({
        payload: { oneofKind: "subscribe", subscribe: subscribePayload },
      }),
    );
    return true;
  }

  unsubscribe(remoteTrackId: string): void {
    const subInfo = this.#activeSubscriptions.get(remoteTrackId);
    if (!subInfo) {
      console.warn(`Not subscribed to remoteTrackId: ${remoteTrackId}`);
      return;
    }

    const unsubscribePayload: ClientUnsubscribePayload = {
      mid: subInfo.slotId,
    };
    this.sendRpc(
      ClientMessageType.create({
        payload: { oneofKind: "unsubscribe", unsubscribe: unsubscribePayload },
      }),
    );

    this.#releaseMid(subInfo.slotId);
    this.#activeSubscriptions.delete(remoteTrackId);
    const currentTrackInfo = this.remoteTrackInfos.get()[remoteTrackId];
    currentTrackInfo?.track?.stop();
    this.remoteTrackInfos.setKey(remoteTrackId, undefined); // Remove from public store
    console.log(
      `Unsubscribed from ${remoteTrackId} (was on MID ${subInfo.slotId})`,
    );
  }

  async publishTrack(track: MediaStreamTrack): Promise<void> {
    const sender = track.kind === "video"
      ? this.#videoSender
      : this.#audioSender;
    const store = track.kind === "video"
      ? this.localVideoTrack
      : this.localAudioTrack;
    if (sender && this.#pc && this.#pc.signalingState !== "closed") {
      try {
        await sender.replaceTrack(track);
        store.set(track);
      } catch (e) {
        console.error("Error replacing track:", e);
      }
    } else {
      console.warn(
        "Cannot publish track: PC not ready or sender not available.",
      );
    }
  }
  async unpublishTrack(kind: "video" | "audio"): Promise<void> {
    const sender = kind === "video" ? this.#videoSender : this.#audioSender;
    const store = kind === "video"
      ? this.localVideoTrack
      : this.localAudioTrack;
    if (sender && this.#pc && this.#pc.signalingState !== "closed") {
      try {
        await sender.replaceTrack(null);
      } catch (e) {
        console.error("Error replacing track with null:", e);
      }
    }
    store.get()?.stop();
    store.set(null);
  }

  disconnect(internalCall = false): void {
    if (
      !internalCall && this.connectionState.get() !== "closed" &&
      this.connectionState.get() !== "new"
    ) console.log("Pulsebeam: Disconnecting...");
    this.localVideoTrack.get()?.stop();
    this.localAudioTrack.get()?.stop();
    this.localVideoTrack.set(null);
    this.localAudioTrack.set(null);
    this.remoteTrackInfos.set({});
    this.#activeSubscriptions.forEach((sub) => this.#releaseMid(sub.slotId)); // Ensure all used MIDs are released
    this.#activeSubscriptions.clear();
    this.#usedMids.clear();

    if (this.#rpcCh) {
      this.#rpcCh.onopen =
        this.#rpcCh.onmessage =
        this.#rpcCh.onclose =
        this.#rpcCh.onerror =
        null;
      if (this.#rpcCh.readyState === "open") this.#rpcCh.close();
      this.#rpcCh = null;
    }
    if (this.#pc) {
      this.#pc.onconnectionstatechange = this.#pc.ontrack = null;
      this.#pc.getSenders().forEach((s) => {
        s.track?.stop();
        if (this.#pc && this.#pc.signalingState !== "closed") {
          try {
            this.#pc.removeTrack(s);
          } catch (e) { }
        }
      });
      this.#pc.getReceivers().forEach((r) => r.track?.stop());
      if (this.#pc.signalingState !== "closed") this.#pc.close();
      this.#pc = null;
    }
    this.#videoSender = null;
    this.#audioSender = null;
    this.#videoRecvMids = [];
    this.#audioRecvMids = [];
    if (!internalCall || this.connectionState.get() !== "new") {
      this.connectionState.set("closed");
    }
    if (!internalCall) this.sfuErrorMessage.set(null);
  }
}
