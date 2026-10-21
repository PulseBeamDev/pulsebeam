(async () => {
  const { RemoteCatalog, attachRemoteVideo } = await import(
    "/dist/remote-catalog.js"
  );
  const { DataRegistry } = await import("/dist/data.js");
  const { ResourceOwners } = await import("/dist/resources.js");
  const snapshot = {
    connection: "disconnected",
    catalog: {
      participants: [{ participantId: "alice" }],
      publications: [
        {
          id: "video-1",
          kind: "video",
          participantId: "alice",
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
  let owner = 0n;
  const owners = new ResourceOwners(() => {});
  owners.activate({
    async resource(command) {
      if (command.op === "claimVideo") return { owner: ++owner };
      if (command.op === "observeVideo") {
        wire = command.observation.visible ? [command.observation] : [];
        return { type: "updated" };
      }
      if (command.op === "release") {
        wire = [];
        return { type: "released" };
      }
      throw new Error(`unexpected resource command ${command.op}`);
    },
  });
  snapshot.catalog.tracks = snapshot.catalog.publications;
  const catalog = new RemoteCatalog(agent, new DataRegistry(agent), owners);
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
    if (devicePixelRatio !== 2)
      throw new Error("layout smoke requires non-unit DPR=2");
    const small = mount(120);
    await until(() => wire[0]?.height === Math.ceil(120 * devicePixelRatio));
    small.element.style.height = "230px";
    await until(() => wire[0]?.height === Math.ceil(230 * devicePixelRatio));
    const resizedPhysicalPixels = wire.length === 1;
    small.element.style.top = "-500px";
    await until(() => wire.length === 0);
    small.element.style.top = "0";
    await until(() => wire[0]?.height === Math.ceil(230 * devicePixelRatio));
    const viewportNotifications = wire.length === 1;
    small.attachment.close();
    const released = wire.length === 0 && listeners.size === 0;
    return { resizedPhysicalPixels, viewportNotifications, released };
  } finally {
    for (const attachment of attachments) attachment.close();
    host.remove();
    catalog.close();
  }
})();
