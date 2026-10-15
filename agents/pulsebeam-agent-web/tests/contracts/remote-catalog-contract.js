(async () => {
  const { RemoteCatalog, attachRemoteVideo } = await import(
    "/dist/remote-catalog.js"
  );
  const snapshot = {
    connection: "disconnected",
    catalog: {
      participants: [{ id: "participant-1", externalId: "alice" }],
      publications: [
        {
          id: "video-1",
          kind: "video",
          participantId: "participant-1",
          label: "camera",
        },
      ],
    },
    mapping: { video: [], audio: [] },
    tracks: {},
  };
  const listeners = new Set();
  const agent = {
    getSnapshot: () => snapshot,
    subscribe(listener) {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
  };
  let wire = [];
  const catalog = new RemoteCatalog(agent, 1, (next) => {
    wire = next;
  });
  catalog.update(snapshot);
  const handle = catalog.participant("alice").video("camera");
  handle.setReceiveOptions({ minHeight: 720 });
  const host = document.createElement("div");
  document.body.append(host);
  const attachments = [];
  const until = async (predicate) => {
    for (let attempt = 0; attempt < 100; attempt++) {
      if (predicate()) return;
      await new Promise((resolve) => setTimeout(resolve, 10));
    }
    throw new Error(`remote layout condition not met: ${JSON.stringify(wire)}`);
  };
  const mount = (height) => {
    const element = document.createElement("video");
    Object.assign(element.style, {
      width: "160px",
      height: `${height}px`,
      position: "fixed",
      top: "0",
      left: "0",
      display: "block",
    });
    host.append(element);
    const attachment = attachRemoteVideo(handle, element);
    attachments.push(attachment);
    return { element, attachment };
  };
  try {
    const small = mount(120);
    await until(() => wire[0]?.height === Math.ceil(120 * devicePixelRatio));
    const large = mount(230);
    await until(() => wire[0]?.height === Math.ceil(230 * devicePixelRatio));
    const maxPhysicalHeight = wire.length === 1;
    large.attachment.close();
    large.element.remove();
    await until(() => wire[0]?.height === Math.ceil(120 * devicePixelRatio));
    small.element.style.height = "200px";
    await until(() => wire[0]?.height === Math.ceil(200 * devicePixelRatio));
    const height = wire[0].height;
    for (const [property, hidden, visible] of [
      ["opacity", "0", "1"],
      ["visibility", "hidden", "visible"],
      ["display", "none", "block"],
    ]) {
      host.style[property] = hidden;
      await until(() => wire.length === 0);
      host.style[property] = visible;
      await until(() => wire[0]?.height === height);
    }
    small.element.style.top = "-500px";
    await until(() => wire.length === 0);
    small.element.style.top = "-50px";
    await until(() => wire[0]?.height === height);
    Object.defineProperty(document, "hidden", {
      configurable: true,
      value: true,
    });
    document.dispatchEvent(new Event("visibilitychange"));
    await until(() => wire.length === 0);
    delete document.hidden;
    document.dispatchEvent(new Event("visibilitychange"));
    await until(() => wire[0]?.height === height);
    const measuredVisibility = wire[0].minHeight === 720;
    small.attachment.close();
    const released = wire.length === 0 && listeners.size === 0;
    return { maxPhysicalHeight, measuredVisibility, released };
  } finally {
    delete document.hidden;
    for (const attachment of attachments) attachment.close();
    host.remove();
    catalog.close();
  }
})();
