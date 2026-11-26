import { Component, type ReactNode } from "react";
import { flushSync } from "react-dom";
import { createRoot } from "react-dom/client";
import { Agent, type VideoCollection, type VideoCollectionEntry } from "@pulsebeam/web";
import { Video, useSubscribe } from "../../src/index.js";
import { publishWithSwap, rejectCodecChange } from "../../../pulsebeam-agent-web/tests/application-contract/publisher.js";

function canvasTrack(color: string) {
  const canvas = document.createElement("canvas");
  canvas.width = 320;
  canvas.height = 180;
  const ctx = canvas.getContext("2d")!;
  let marker = false;
  const paint = () => {
    ctx.fillStyle = color;
    ctx.fillRect(0, 0, canvas.width, canvas.height);
    ctx.fillStyle = (marker = !marker) ? "white" : "black";
    ctx.fillRect(0, 0, 16, 16);
  };
  paint();
  const stream = canvas.captureStream(15);
  const track = stream.getVideoTracks()[0]!;
  const timer = setInterval(paint, 66);
  return { track, close: () => { clearInterval(timer); track.stop(); } };
}

function until<T>(
  sample: () => T | undefined,
  detail: string | (() => string),
): Promise<T> {
  return new Promise((resolve, reject) => {
    const deadline = performance.now() + 8_000;
    const timer = setInterval(() => {
      try {
        const value = sample();
        if (value !== undefined) {
          clearInterval(timer);
          resolve(value);
        } else if (performance.now() >= deadline) {
          throw new Error(typeof detail === "function" ? detail() : detail);
        }
      } catch (error) {
        clearInterval(timer);
        reject(error);
      }
    }, 25);
  });
}

export function recoveryFixture(config: {
  endpoint: string;
  room: string;
  publisher: string;
  viewer: string;
}) {
  const peers = new Set<RTCPeerConnection>();
  const rtc = RTCPeerConnection.prototype;
  const setLocalDescription = rtc.setLocalDescription;
  rtc.setLocalDescription = function (this: RTCPeerConnection, ...args: unknown[]) {
    peers.add(this);
    return Reflect.apply(setLocalDescription, this, args);
  };
  const videoStats = async () => Promise.all([...peers].map(async (peer) =>
    Promise.all(peer.getTransceivers().filter(({ receiver }) => receiver.track.kind === "video")
      .map(async ({ mid, sender, receiver }) => {
        const [sent, received] = await Promise.all([sender.getStats(), receiver.getStats()]);
        const video = (stats: RTCStatsReport, type: string) => [...stats.values()]
          .filter((entry) => entry.type === type && entry.kind === "video")
          .map(({ ssrc, packetsSent, packetsReceived, framesEncoded, framesDecoded,
            keyFramesEncoded, keyFramesDecoded, pliCount }) => ({ ssrc, packetsSent, packetsReceived,
            framesEncoded, framesDecoded, keyFramesEncoded, keyFramesDecoded, pliCount }));
        return { mid, sender: sender.track?.id, receiver: receiver.track.id,
          outbound: video(sent, "outbound-rtp"), inbound: video(received, "inbound-rtp") };
      })),
  ));
  const publisher = new Agent({ endpoint: config.endpoint, token: config.publisher });
  const viewer = new Agent({ endpoint: config.endpoint, token: config.viewer });
  const room = viewer.room(config.room);
  const host = document.createElement("div");
  document.body.append(host);
  const root = createRoot(host);
  const dark = canvasTrack("#303030");
  const bright = canvasTrack("#d0d0d0");
  const sample = document.createElement("canvas");
  sample.width = 32;
  sample.height = 18;
  const pixels = sample.getContext("2d", { willReadFrequently: true })!;
  let collection: VideoCollection | null = null;
  let retained: VideoCollectionEntry | undefined;
  let failure: unknown;
  let admission: string | undefined;
  let published: (() => string | undefined) | undefined;
  let exchange!: () => void;
  let finish!: () => void;
  const swap = new Promise<void>((resolve) => { exchange = resolve; });
  const stop = new Promise<void>((resolve) => { finish = resolve; });
  const publishing = publishWithSwap(publisher, dark.track, bright.track, async (publication, input, pipe) => {
    published = () => {
      const state = publication.state();
      return `${state.admission.publicationId}/${state.admission.execution}/enabled:${state.configuration.enabled}/pipe:${pipe.state().status}`;
    };
    if (input === "first") {
      await swap;
      await rejectCodecChange(publication);
    } else await stop;
  }).catch((error: unknown) => { failure = error; });
  publisher.join(config.room);
  viewer.join(config.room);

  function Grid({ paused }: { paused: boolean }) {
    const owner = useSubscribe(room.video({ trackId: "camera" }));
    collection = owner;
    const entries = paused && retained ? [retained] : owner?.state().tracks;
    return entries?.map(({ publicationId, participantId, track }) => (
      <Video key={publicationId} source={track} aria-label={participantId} />
    ));
  }
  class Boundary extends Component<{ children: ReactNode }> {
    componentDidCatch(error: unknown) { failure = error; }
    render() { return this.props.children; }
  }
  const render = (paused: boolean) => root.render(<Boundary><Grid paused={paused} /></Boundary>);
  render(false);
  let tile: HTMLVideoElement | undefined;
  let retired: MediaStreamTrack | undefined;
  const picture = (bright: boolean) => {
    let luma: number | undefined;
    return until(() => {
      if (failure) {
        const info = (failure as { inner?: { info?: { message?: string } } }).inner?.info;
        throw new Error(`recovery fixture: ${info?.message ?? failure}`);
      }
      const element = host.querySelector("video");
      if (!element || element.readyState < 2) return;
      if (tile && tile !== element) throw new Error("publication tile was remounted");
      const output = (element.srcObject as MediaStream | null)?.getVideoTracks()[0];
      if (!output || output === retired || output.readyState !== "live") return;
      pixels.drawImage(element, 0, 0, sample.width, sample.height);
      luma = pixels.getImageData(sample.width >> 1, sample.height >> 1, 1, 1).data[0]!;
      if (bright ? luma < 160 : luma > 110 || luma < 15) return;
      tile = element;
      return true;
    }, () => {
      const current = collection?.state();
      const element = host.querySelector("video");
      return `viewer did not decode ${bright ? "bright" : "dark"}: collected=${current?.status}/${current?.tracks.length} error=${current?.error?.message}, published=${published?.()}, sender=${publisher.state().status}, viewer=${viewer.state().status}, ready=${element?.readyState}/${element?.currentTime}/${element?.videoWidth}, attached=${(element?.srcObject as MediaStream | null)?.getTracks().length}, luma=${luma}`;
    });
  };
  const entry = () => collection?.state().tracks.find(({ trackId }) => trackId === "camera");
  return {
    async first() {
      try {
        await picture(false);
      } finally {
        rtc.setLocalDescription = setLocalDescription;
      }
      retained = entry();
      if (!retained || retained.participantId !== "collection-publisher")
        throw new Error("missing admitted publication metadata");
      admission = retained.publicationId;
      return admission;
    },
    async swap() {
      exchange();
      await picture(true);
      if (!retained || !entry()?.track.sameAdmission(retained.track))
        throw new Error("input swap changed admission");
      return true;
    },
    async withdraw(token: string) {
      if (!retained || !tile) throw new Error("missing prior tile");
      retired = (tile.srcObject as MediaStream | null)?.getVideoTracks()[0];
      if (!retired) throw new Error("missing track before withdrawal");
      flushSync(() => render(true));
      viewer.renew(token);
      return until(() => {
        if (collection?.state().tracks.length === 0 && retired?.readyState === "ended") return true;
      }, "viewer did not withdraw old admission");
    },
    async readmit() {
      viewer.renew(config.viewer);
      await until(() => {
        const current = entry();
        if (current?.publicationId === admission && current && retained &&
            !current.track.sameAdmission(retained.track)) return true;
      }, "viewer did not readmit the same publication with fresh authority");
      render(false);
      try {
        return await picture(true);
      } catch (error) {
        const stats = await videoStats().then(JSON.stringify, String);
        throw new Error(`readmitted viewer: ${error}; video RTP stats=${stats}`);
      }
    },
    async close() {
      exchange();
      finish();
      root.unmount();
      await publishing;
      dark.close();
      bright.close();
      await Promise.all([publisher.close(), viewer.close()]);
      if (failure) throw failure;
      return true;
    },
  };
}
