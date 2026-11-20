import { createElement, Fragment, useEffect, useMemo } from "react";
import { createRoot } from "react-dom/client";
import {
  Data,
  Video,
  useAgent,
  useSubscribe,
  useWatch,
  WatchStatus,
  type Watch,
  type Agent,
} from "@pulsebeam/react";

import { Agent as WebAgent } from "@pulsebeam/web";

interface Config {
  endpoint: string;
  token: string;
  room: string;
  sourceRoom: string;
}

export async function observeCatalog(config: {
  endpoint: string;
  token: string;
  publisherToken: string;
  room: string;
}): Promise<boolean> {
  let ownedWatch: Watch | undefined;
  function Catalog() {
    const agent = useAgent({ endpoint: config.endpoint, token: config.token });
    useEffect(() => {
      agent?.join(config.room);
    }, [agent]);
    const room = useMemo(() => agent?.room(config.room) ?? null, [agent]);
    const watch = useWatch(room, { limit: 7 });
    const state = watch?.state();
    useEffect(() => {
      if (watch) ownedWatch = watch;
    }, [watch]);
    return createElement(
      "output",
      { id: "catalog", "data-status": state?.status },
      state?.error?.message ??
        state?.page?.participants
          .flatMap((participant) =>
            participant.publications.map(
              (publication) => `${participant.id}/${publication.trackId}`,
            ),
          )
          .join(","),
    );
  }
  const container = document.getElementById("root");
  if (!container) throw new Error("missing React fixture root");
  const root = createRoot(container);
  const publisher = new WebAgent({
    endpoint: config.endpoint,
    token: config.publisherToken,
  });
  const rendered = () => document.getElementById("catalog");
  const until = async (predicate: () => boolean, label: string) => {
    for (let attempt = 0; attempt < 500; attempt++) {
      if (predicate()) return;
      await new Promise((resolve) => setTimeout(resolve, 10));
    }
    throw new Error(`${label}: ${rendered()?.outerHTML}`);
  };
  try {
    publisher.join(config.room);
    root.render(createElement(Catalog));
    await until(
      () =>
        rendered()?.dataset.status === String(WatchStatus.Live) &&
        Boolean(rendered()?.textContent?.includes("native-publisher/camera")) &&
        Boolean(
          rendered()?.textContent?.includes("native-publisher/frame-events"),
        ),
      "React useWatch renders the actual Native catalog",
    );
    const publication = publisher.data("catalog-change", "ordered").publish();
    await until(
      () =>
        Boolean(
          rendered()?.textContent?.includes(
            "react-metadata-publisher/catalog-change",
          ),
        ),
      "React useWatch renders a real publication change",
    );
    if (!ownedWatch) throw new Error("React did not commit a Watch owner");
    await ownedWatch.close();
    await until(
      () =>
        rendered()?.dataset.status === String(WatchStatus.Closed) &&
        rendered()?.textContent === "",
      "React renders the Watch final closed state",
    );
    await publication.close();
    return true;
  } finally {
    root.unmount();
    await publisher.close();
  }
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
            !event.publicationId ||
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
