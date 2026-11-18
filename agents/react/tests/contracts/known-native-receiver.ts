import { createElement, Fragment, useEffect, useMemo } from "react";
import { createRoot } from "react-dom/client";
import {
  Data,
  Video,
  useAgent,
  useSubscribe,
  type Agent,
} from "@pulsebeam/react";

interface Config {
  endpoint: string;
  token: string;
  room: string;
  sourceRoom: string;
}

export async function receiveNative(config: Config): Promise<{
  video: boolean;
  data: boolean;
  wasm: boolean;
}> {
  let videoReady!: () => void;
  let dataReady!: () => void;
  let fail!: (reason: unknown) => void;
  let previous: bigint | undefined;
  let messages = 0;
  const video = new Promise<void>((resolve) => {
    videoReady = resolve;
  });
  const data = new Promise<void>((resolve) => {
    dataReady = resolve;
  });
  const failure = new Promise<never>((_, reject) => {
    fail = reject;
  });
  const timeout = window.setTimeout(
    () => fail(new Error("known native source did not deliver media and data")),
    15_000,
  );

  function Receiver({ agent }: { agent: Agent }) {
    const participant = useMemo(
      () => agent.room(config.sourceRoom).participant("native-publisher"),
      [agent],
    );
    const camera = useMemo(() => participant.video("camera"), [participant]);
    const bytes = useMemo(
      () => participant.data("frame-events", "ordered"),
      [participant],
    );
    const reception = useSubscribe(camera);
    return createElement(
      Fragment,
      null,
      createElement(Video, {
        source: reception,
        style: { width: 160, height: 96 },
        onPlaybackError: fail,
        onLoadedData: (event) => {
          try {
            const element = event.currentTarget;
            if (element.videoWidth !== 160 || element.videoHeight !== 96) {
              throw new Error("native video geometry changed");
            }
            const canvas = document.createElement("canvas");
            canvas.width = 160;
            canvas.height = 96;
            const context = canvas.getContext("2d");
            if (!context) throw new Error("missing canvas context");
            context.drawImage(element, 0, 0);
            const pixel = context.getImageData(80, 48, 1, 1).data;
            if (pixel[0] < 150 || pixel[1] < 50 || pixel[3] !== 255) {
              throw new Error("native decoded pixels missing");
            }
            videoReady();
          } catch (error) {
            fail(error);
          }
        },
      }),
      createElement(Data, {
        track: bytes,
        onError: fail,
        onEvent: (event) => {
          if (
            event.type !== "message" ||
            event.participantId !== "native-publisher" ||
            !event.trackId ||
            event.data.length !== 11 ||
            event.data[0] !== 0 ||
            event.data[1] !== 255 ||
            event.data[2] !== 128
          ) {
            throw new Error("wrong known-source data or binary payload");
          }
          const sequence = new DataView(
            event.data.buffer,
            event.data.byteOffset,
            event.data.byteLength,
          ).getBigUint64(3, true);
          if (previous !== undefined && sequence <= previous) {
            throw new Error("native live data duplicated or moved backwards");
          }
          previous = sequence;
          if (++messages === 2) dataReady();
        },
      }),
    );
  }

  function App() {
    const agent = useAgent({ endpoint: config.endpoint, token: config.token });
    useEffect(() => {
      agent?.join(config.room);
    }, [agent]);
    return agent ? createElement(Receiver, { agent }) : null;
  }

  const container = document.getElementById("root");
  if (!container) throw new Error("missing React fixture root");
  const root = createRoot(container);
  const originalInstance = WebAssembly.Instance;
  let instances = 0;
  // The package embeds WASM, so observe real instantiation rather than HTTP.
  WebAssembly.Instance = new Proxy(originalInstance, {
    construct(target, args, newTarget) {
      const instance = Reflect.construct(target, args, newTarget);
      instances++;
      return instance;
    },
  });
  try {
    root.render(createElement(App));
    await Promise.race([Promise.all([video, data]), failure]);
    return { video: true, data: true, wasm: instances > 0 };
  } finally {
    WebAssembly.Instance = originalInstance;
    window.clearTimeout(timeout);
    root.unmount();
  }
}
