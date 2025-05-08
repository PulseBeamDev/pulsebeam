import { atom, map, MapStore, WritableAtom } from "nanostores";
import {
  ClientMessage,
  ClientSubscribePayload,
  ClientUnsubscribePayload,
  ErrorPayload,
  ServerMessage,
  SubscriptionErrorPayload,
  TrackKind,
  TrackPublishedPayload,
  TrackUnpublishedPayload,
} from "./sfu";

export type ClientStatus =
  | "new"
  | "connecting"
  | "connected"
  | "disconnected"
  | "failed";

export interface RemoteTrack {
  slotId: string;
  remoteTrackId: string;
  kind: "video" | "audio";
  track: MediaStreamTrack | null;
  error?: string;
}

export interface AvailableTrack {
  remoteTrackId: string;
  kind: "video" | "audio";
  participantId: string;
}

export interface AppDataChannelConfig {
  onMessage: (data: string) => void; // Expects string data
  onOpen?: (event: Event) => void;
  onClose?: (event: Event) => void;
  options?: RTCDataChannelInit; // Still allow options like 'ordered'
}

export class PulsebeamClient {
  #pc: RTCPeerConnection | null = null;
  #sfuRpcCh: RTCDataChannel | null = null;
  #appDataCh: RTCDataChannel | null = null;

  readonly #sfuUrl: string;
  readonly #maxDownstreams: number;
  readonly #appDataConfig?: AppDataChannelConfig;

  #videoSender: RTCRtpSender | null = null;
  #audioSender: RTCRtpSender | null = null;
  #videoRecvMids: string[] = [];
  #audioRecvMids: string[] = [];
  #usedMids = new Set<string>();
  #activeSubscriptions = new Map<string, { slotId: string; kind: TrackKind }>(); // remoteTrackId -> subInfo

  #instanceTerminated = false;

  public readonly status: WritableAtom<ClientStatus> = atom("new");
  public readonly localVideo: WritableAtom<MediaStreamTrack | null> = atom(
    null,
  );
  public readonly localAudio: WritableAtom<MediaStreamTrack | null> = atom(
    null,
  );
  public readonly remoteTracks: MapStore<Record<string, RemoteTrack>> = map({});
  public readonly availableTracks: MapStore<Record<string, AvailableTrack>> =
    map({});
  public readonly errorMsg: WritableAtom<string | null> = atom(null);

  constructor(
    sfuUrl: string,
    maxDownstreams: number = 9,
    appDataConfig?: AppDataChannelConfig,
  ) {
    this.#sfuUrl = sfuUrl;
    this.#maxDownstreams = maxDownstreams;
    this.#appDataConfig = appDataConfig;
  }

  #terminateInstance(
    finalStatus: "disconnected" | "failed",
    message?: string,
  ): void {
    if (this.#instanceTerminated) {
      return;
    }
    this.#instanceTerminated = true;
    console.debug(
      `PulsebeamClient: Terminating instance with status: ${finalStatus}, message: ${message || "N/A"
      }`,
    );

    this.localVideo.get()?.stop();
    this.localAudio.get()?.stop();
    this.localVideo.set(null);
    this.localAudio.set(null);

    Object.values(this.remoteTracks.get()).forEach((remoteTrackInfo) => {
      remoteTrackInfo?.track?.stop();
    });
    this.remoteTracks.set({});
    this.availableTracks.set({});

    const cleanupChannel = (channel: RTCDataChannel | null): void => {
      if (channel) {
        channel.onopen = null;
        channel.onmessage = null;
        channel.onclose = null;
        channel.onerror = null;
        if (
          channel.readyState === "open" || channel.readyState === "connecting"
        ) {
          try {
            channel.close();
          } catch (e) {
            console.warn("Error closing data channel:", e);
          }
        }
      }
    };

    cleanupChannel(this.#sfuRpcCh);
    this.#sfuRpcCh = null;
    cleanupChannel(this.#appDataCh);
    this.#appDataCh = null;

    if (this.#pc) {
      this.#pc.onconnectionstatechange = null;
      this.#pc.ontrack = null;
      this.#pc.onicecandidate = null;

      this.#pc.getSenders().forEach((sender) => {
        sender.track?.stop();
      });
      // No need to explicitly stop receiver tracks here, as they are managed by remoteTracks cleanup

      if (this.#pc.signalingState !== "closed") {
        try {
          this.#pc.close();
        } catch (e) {
          console.warn("Error closing PeerConnection:", e);
        }
      }
      this.#pc = null;
    }

    this.#activeSubscriptions.clear();
    this.#usedMids.clear();
    this.#videoRecvMids = [];
    this.#audioRecvMids = [];
    this.#videoSender = null;
    this.#audioSender = null;

    if (message) {
      this.errorMsg.set(message);
    }
    this.status.set(finalStatus);
    console.warn(
      "PulsebeamClient instance has been terminated and is no longer usable.",
    );
  }

  #updateConnectedStatus(): void {
    if (this.#instanceTerminated || this.status.get() !== "connecting") {
      return;
    }

    const pcConnected = this.#pc?.connectionState === "connected";
    const rpcReady = this.#sfuRpcCh?.readyState === "open";
    const appDcReady = !this.#appDataConfig ||
      this.#appDataCh?.readyState === "open";

    if (pcConnected && rpcReady && appDcReady) {
      this.status.set("connected");
      this.errorMsg.set(null); // Clear any transient errors from connecting phase
    }
  }

  async connect(room: string, participantId: string): Promise<void> {
    if (this.#instanceTerminated) {
      const errorMessage =
        "This client instance has been terminated and cannot be reused.";
      this.errorMsg.set(errorMessage);
      console.error(errorMessage);
      throw new Error(errorMessage); // More direct feedback to developer
    }

    if (this.status.get() !== "new") {
      const errorMessage =
        `Client can only connect when in "new" state. Current status: ${this.status.get()}. Create a new instance to reconnect.`;
      // Only set error if it's not already a terminal state from a previous attempt on this (now invalid) instance
      if (
        this.status.get() !== "failed" && this.status.get() !== "disconnected"
      ) {
        this.errorMsg.set(errorMessage);
      }
      console.warn(errorMessage);
      return; // Do not proceed
    }

    this.status.set("connecting");
    this.errorMsg.set(null);

    this.#pc = new RTCPeerConnection();
    const peerConnection = this.#pc; // Use a more descriptive local variable
    peerConnection.onicecandidate = null; // No ICE trickling

    peerConnection.onconnectionstatechange = () => {
      if (this.#instanceTerminated || !this.#pc) return; // Guard
      const connectionState = this.#pc.connectionState;
      console.debug(`PeerConnection state changed: ${connectionState}`);
      if (connectionState === "connected") {
        this.#updateConnectedStatus();
      } else if (
        connectionState === "failed" || connectionState === "closed" ||
        connectionState === "disconnected"
      ) {
        // 'disconnected' is often transient, but for this minimal client, we treat it as terminal.
        this.#terminateInstance(
          "failed",
          `PeerConnection state became: ${connectionState}`,
        );
      }
    };

    peerConnection.ontrack = (event: RTCTrackEvent) => {
      if (this.#instanceTerminated) return;
      const mid = event.transceiver?.mid;
      const track = event.track;
      if (!mid || !track) {
        console.warn("Received track event without MID or track object.");
        return;
      }

      const remoteTrackId = Array.from(this.#activeSubscriptions.entries())
        .find(
          ([_rId, subInfo]) =>
            subInfo.slotId === mid &&
            subInfo.kind ===
            (track.kind === "video" ? TrackKind.VIDEO : TrackKind.AUDIO),
        )?.[0];

      if (remoteTrackId) {
        this.remoteTracks.setKey(remoteTrackId, {
          slotId: mid,
          remoteTrackId: remoteTrackId,
          kind: track.kind as "video" | "audio",
          track: track,
          error: undefined,
        });
        track.onended = () => {
          if (this.#instanceTerminated) return;
          const currentTrackInfo = this.remoteTracks.get()[remoteTrackId];
          if (currentTrackInfo) { // Ensure it hasn't been removed by unsubscribe
            this.remoteTracks.setKey(remoteTrackId, {
              ...currentTrackInfo,
              track: null,
            });
          }
        };
      } else {
        console.warn(
          `Received track on MID ${mid} (kind: ${track.kind}) but no matching subscription found. Stopping track.`,
        );
        track.stop();
      }
    };

    // SFU RPC DataChannel
    this.#sfuRpcCh = peerConnection.createDataChannel("pulsebeam::rpc");
    this.#sfuRpcCh.binaryType = "arraybuffer";
    this.#sfuRpcCh.onopen = () => {
      if (!this.#instanceTerminated) this.#updateConnectedStatus();
    };
    this.#sfuRpcCh.onmessage = (event: MessageEvent) => {
      if (this.#instanceTerminated) return;
      try {
        const serverMessage = ServerMessage.fromBinary(
          new Uint8Array(event.data as ArrayBuffer),
        );
        const payloadKind = serverMessage.payload.oneofKind;
        if (!payloadKind) {
          console.warn("Received SFU message with undefined payload kind.");
          return;
        }
        const payloadData = serverMessage.payload[payloadKind];

        switch (payloadKind) {
          case "error":
            this.errorMsg.set((payloadData as ErrorPayload).message);
            break;
          case "subscriptionError":
            const subError = payloadData as SubscriptionErrorPayload;
            const targetId = subError.remoteTrackId ||
              Array.from(this.#activeSubscriptions.entries()).find(([, s]) =>
                s.slotId === subError.mid
              )?.[0];
            if (targetId) {
              const currentRemote = this.remoteTracks.get()[targetId];
              if (currentRemote) {
                this.remoteTracks.setKey(targetId, {
                  ...currentRemote,
                  error: subError.message,
                  track: null,
                });
              }
              const subInfo = this.#activeSubscriptions.get(targetId);
              if (subInfo) {
                this.#usedMids.delete(subInfo.slotId);
                this.#activeSubscriptions.delete(targetId);
              }
            }
            this.errorMsg.set(
              `Subscription error for ${targetId || subError.mid || "track"
              }: ${subError.message}`,
            );
            break;
          case "trackPublished":
            const trackPublished = payloadData as TrackPublishedPayload;
            this.availableTracks.setKey(trackPublished.remoteTrackId, {
              remoteTrackId: trackPublished.remoteTrackId,
              kind: trackPublished.kind === TrackKind.VIDEO ? "video" : "audio",
              participantId: trackPublished.participantId,
            });
            break;
          case "trackUnpublished":
            const trackUnpublished = payloadData as TrackUnpublishedPayload;
            if (this.#activeSubscriptions.has(trackUnpublished.remoteTrackId)) {
              this.unsubscribe(trackUnpublished.remoteTrackId);
            }
            this.availableTracks.setKey(
              trackUnpublished.remoteTrackId,
              undefined,
            );
            break;
        }
      } catch (e: any) {
        console.error("Error processing SFU RPC message:", e);
        this.errorMsg.set("Failed to process server message.");
      }
    };
    const createFatalRpcHandler = (type: string) => () => {
      if (!this.#instanceTerminated) {
        this.#terminateInstance("failed", `SFU RPC DataChannel ${type}`);
      }
    };
    this.#sfuRpcCh.onclose = createFatalRpcHandler("closed");
    this.#sfuRpcCh.onerror = createFatalRpcHandler("error");

    // Optional Application DataChannel
    if (this.#appDataConfig) {
      this.#appDataCh = peerConnection.createDataChannel(
        "app-data",
        this.#appDataConfig.options,
      );
      this.#appDataCh.onmessage = (event: MessageEvent) => {
        if (this.#instanceTerminated || !this.#appDataConfig) return;
        if (typeof event.data === "string") {
          this.#appDataConfig.onMessage(event.data);
        } else {
          console.warn(
            "Received non-string message on app data channel, ignoring.",
          );
        }
      };
      const appDcOpenHandler = (event: Event) => {
        if (!this.#instanceTerminated) {
          this.#appDataConfig?.onOpen?.(event);
          this.#updateConnectedStatus();
        }
      };
      this.#appDataCh.onopen = appDcOpenHandler;

      const createFatalAppDcHandler = (type: string) => (event?: Event) => { // onerror might not pass event
        if (!this.#instanceTerminated) {
          if (type === "close" && this.#appDataConfig?.onClose && event) {
            this.#appDataConfig.onClose(event);
          }
          this.#terminateInstance("failed", `Application DataChannel ${type}`);
        }
      };
      this.#appDataCh.onclose = createFatalAppDcHandler("closed");
      this.#appDataCh.onerror = createFatalAppDcHandler("error");
    }

    // Transceivers
    this.#videoSender =
      peerConnection.addTransceiver("video", { direction: "sendonly" }).sender;
    this.#audioSender =
      peerConnection.addTransceiver("audio", { direction: "sendonly" }).sender;
    for (let i = 0; i < this.#maxDownstreams; i++) {
      const videoTransceiver = peerConnection.addTransceiver("video", {
        direction: "recvonly",
      });
      if (videoTransceiver.mid) this.#videoRecvMids.push(videoTransceiver.mid);
      const audioTransceiver = peerConnection.addTransceiver("audio", {
        direction: "recvonly",
      });
      if (audioTransceiver.mid) this.#audioRecvMids.push(audioTransceiver.mid);
    }

    // Signaling
    try {
      const offer = await peerConnection.createOffer();
      await peerConnection.setLocalDescription(offer);
      const response = await fetch(
        `${this.#sfuUrl}?room=${room}&participant=${participantId}`,
        {
          method: "POST",
          body: offer.sdp!,
          headers: { "Content-Type": "application/sdp" },
        },
      );
      if (!response.ok) {
        throw new Error(
          `Signaling request failed: ${response.status} ${await response
            .text()}`,
        );
      }
      await peerConnection.setRemoteDescription({
        type: "answer",
        sdp: await response.text(),
      });
      // Status transitions to "connected" will be handled by onconnectionstatechange and data channel onopen events.
    } catch (error: any) {
      this.#terminateInstance(
        "failed",
        error.message || "Signaling process failed unexpectedly.",
      );
    }
  }

  disconnect(): void {
    if (
      this.#instanceTerminated ||
      (this.status.get() !== "connected" && this.status.get() !== "connecting")
    ) {
      // Already terminated, or wasn't in a state that can be "disconnected" (e.g. "new")
      return;
    }
    this.errorMsg.set(null); // Clear error on explicit, clean disconnect attempt
    this.#terminateInstance("disconnected", "User initiated disconnect");
  }

  async publish(track: MediaStreamTrack): Promise<void> {
    if (this.status.get() !== "connected" || this.#instanceTerminated) {
      console.warn(
        "Client not connected or instance terminated. Cannot publish.",
      );
      return;
    }
    const sender = track.kind === "video"
      ? this.#videoSender
      : this.#audioSender;
    try {
      if (sender) {
        await sender.replaceTrack(track);
        (track.kind === "video" ? this.localVideo : this.localAudio).set(track);
      } else {
        throw new Error(`No sender found for ${track.kind} track.`);
      }
    } catch (error: any) {
      this.errorMsg.set(`Publish failed: ${error.message}`);
      console.error("Error during publish:", error);
    }
  }

  async unpublish(kind: "video" | "audio"): Promise<void> {
    // Allow unpublish if instance is being disconnected or is already disconnected/failed, to ensure tracks are cleaned up.
    if (
      this.#instanceTerminated && this.status.get() !== "disconnected" &&
      this.status.get() !== "failed"
    ) {
      return; // Don't operate if terminated for reasons other than explicit disconnect/failure
    }

    const sender = kind === "video" ? this.#videoSender : this.#audioSender;
    const store = kind === "video" ? this.localVideo : this.localAudio;
    try {
      if (sender) await sender.replaceTrack(null);
    } catch (error: any) {
      console.warn(`Warning during unpublish ${kind}: ${error.message}`);
    }
    store.get()?.stop();
    store.set(null);
  }

  subscribe(remoteTrackId: string, kind: "video" | "audio"): boolean {
    if (this.status.get() !== "connected" || this.#instanceTerminated) {
      this.errorMsg.set(
        "Client not connected or instance terminated. Cannot subscribe.",
      );
      return false;
    }
    if (this.#activeSubscriptions.has(remoteTrackId)) {
      console.warn(
        `Already subscribed or attempting to subscribe to remote track: ${remoteTrackId}`,
      );
      return true; // Or false depending on desired strictness
    }

    const protoKind = kind === "video" ? TrackKind.VIDEO : TrackKind.AUDIO;
    const midsPool = protoKind === TrackKind.VIDEO
      ? this.#videoRecvMids
      : this.#audioRecvMids;
    const mid = midsPool.find((m) => !this.#usedMids.has(m));

    if (!mid) {
      this.errorMsg.set(
        `No free ${kind} slot available to subscribe to ${remoteTrackId}.`,
      );
      return false;
    }

    this.#usedMids.add(mid);
    this.#activeSubscriptions.set(remoteTrackId, {
      slotId: mid,
      kind: protoKind,
    });
    this.remoteTracks.setKey(remoteTrackId, {
      slotId: mid,
      remoteTrackId,
      kind,
      track: null,
    });

    const clientSubscribePayload: ClientSubscribePayload = {
      mid,
      trackId: remoteTrackId,
      kind: protoKind,
    };
    if (this.#sfuRpcCh?.readyState === "open") {
      this.#sfuRpcCh.send(
        ClientMessage.toBinary({
          payload: {
            oneofKind: "subscribe",
            subscribe: clientSubscribePayload,
          },
        }),
      );
      return true;
    } else {
      // This should ideally not be reached if status is "connected"
      this.#terminateInstance(
        "failed",
        "SFU RPC channel not open during subscribe attempt.",
      );
      return false;
    }
  }

  unsubscribe(remoteTrackId: string): void {
    // Allow unsubscribe during disconnect/failure for cleanup
    if (
      this.#instanceTerminated && this.status.get() !== "disconnected" &&
      this.status.get() !== "failed"
    ) {
      return;
    }

    const subscriptionInfo = this.#activeSubscriptions.get(remoteTrackId);
    if (!subscriptionInfo) {
      return;
    }

    if (this.#sfuRpcCh?.readyState === "open") {
      const clientUnsubscribePayload: ClientUnsubscribePayload = {
        mid: subscriptionInfo.slotId,
      };
      this.#sfuRpcCh.send(
        ClientMessage.toBinary({
          payload: {
            oneofKind: "unsubscribe",
            unsubscribe: clientUnsubscribePayload,
          },
        }),
      );
    }

    this.#usedMids.delete(subscriptionInfo.slotId);
    this.#activeSubscriptions.delete(remoteTrackId);

    const currentRemoteTrack = this.remoteTracks.get()[remoteTrackId];
    currentRemoteTrack?.track?.stop();
    this.remoteTracks.setKey(remoteTrackId, undefined); // Remove from map
  }

  sendAppData(data: string): boolean {
    if (this.status.get() !== "connected" || this.#instanceTerminated) {
      this.errorMsg.set(
        "Client not connected or instance terminated. Cannot send app data.",
      );
      return false;
    }
    if (!this.#appDataCh || this.#appDataCh.readyState !== "open") {
      this.errorMsg.set(
        "Application DataChannel is not configured or not open.",
      );
      return false;
    }
    try {
      this.#appDataCh.send(data);
      return true;
    } catch (error: any) {
      this.errorMsg.set(`Failed to send app data: ${error.message}`);
      console.error("Error sending app data:", error);
      return false;
    }
  }
}
