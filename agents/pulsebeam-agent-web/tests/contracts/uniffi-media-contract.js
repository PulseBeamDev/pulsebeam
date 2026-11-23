(async () => {
  let wasmInstances = 0;
  const Instance = WebAssembly.Instance;
  WebAssembly.Instance = new Proxy(Instance, {
    construct(target, args, newTarget) {
      wasmInstances++;
      return Reflect.construct(target, args, newTarget);
    },
  });
  const { Agent, AgentStatus, AgentError, ErrorCode } = await import(
    "/sdk/dist/index.js"
  ).finally(() => {
    WebAssembly.Instance = Instance;
  });
  const { default: web } = await import(
    "/sdk/dist/generated/bindings/pulsebeam_agent_web.js"
  );
  const registry = await import("/sdk/dist/web/media-registry.js");
  const agent = new Agent(config);
  const cursor = agent.updates();
  let initialState;
  let stableState;
  let pendingReadClosed;
  let pendingDataReadsClosed;
  let joinedAndClosed;
  let exactVideoOwner;
  try {
    const current = agent.state();
    const first = await cursor.next();
    initialState =
      current.status === AgentStatus.Idle &&
      first?.observationRevision === current.observationRevision &&
      first.status === current.status;
    const other = new Agent(config);
    try {
      stableState =
        Object.isFrozen(current) &&
        agent.state() === current &&
        other.state() !== current;
      const peer = other
        .room(config.sourceRoom)
        .participant("native-publisher");
      const direct = peer.data("close-reader").subscribe();
      const throughAgent = peer.data("close-agent").subscribe();
      const firstRead = direct.next();
      const secondRead = throughAgent.next();
      const duplicate = await throughAgent.next().then(
        () => false,
        (error) =>
          AgentError.Operation.instanceOf(error) &&
          error.inner.info.code === ErrorCode.Conflict,
      );
      await direct.close();
      await other.close();
      pendingDataReadsClosed =
        duplicate &&
        (await firstRead) === undefined &&
        (await secondRead) === undefined;
    } finally {
      await other.close();
      other.uniffiDestroy();
    }
    const pending = cursor.next();
    let rejected = false;
    try {
      await cursor.next();
    } catch {
      rejected = true;
    } finally {
      cursor.close();
    }
    const end = await pending;
    pendingReadClosed = rejected && end === undefined;

    const room = agent.room(config.sourceRoom);
    const participant = room.participant("native-publisher");
    const camera = participant.video("camera");
    const reception = camera.subscribe();
    const video = reception.updates();
    try {
      const pendingState = await video.next();
      const next = video.next();
      await reception.close();
      await reception.close();
      const closed = await next;
      exactVideoOwner =
        pendingState?.selector.room === config.sourceRoom &&
        pendingState.selector.participantId === "native-publisher" &&
        pendingState.selector.trackId === "camera" &&
        !pendingState.closed &&
        closed?.closed === true &&
        reception.state().closed &&
        (await video.next()) === undefined;
    } finally {
      video.close();
      await reception.close();
      video.uniffiDestroy();
      reception.uniffiDestroy();
      camera.uniffiDestroy();
      participant.uniffiDestroy();
      room.uniffiDestroy();
    }

    const lifecycle = agent.updates();
    try {
      const before = agent.state();
      let invalidRejected = false;
      try {
        agent.renew("");
      } catch (error) {
        invalidRejected =
          AgentError.Operation.instanceOf(error) &&
          error.inner.info.code === ErrorCode.InvalidArgument;
      }
      if (!invalidRejected || agent.state() !== before) {
        throw new Error("invalid renewal changed the current Agent intent");
      }
      agent.renew(config.token);
      if (agent.join("ungranted-room") !== undefined) {
        throw new Error("join must submit intent synchronously");
      }
      const deniedRevision = agent.state().desiredJoinRevision;
      while (true) {
        const denied = await lifecycle.next();
        if (!denied) throw new Error("Agent closed before denial was observed");
        if (denied.evaluatedJoinRevision !== deniedRevision) continue;
        if (
          denied.status !== AgentStatus.Failed ||
          denied.error?.code !== ErrorCode.PermissionDenied ||
          denied.room !== undefined
        ) {
          throw new Error(
            `unexpected denied admission: ${denied.error?.message}`,
          );
        }
        break;
      }
      agent.join(config.room);
      let state;
      do {
        state = await lifecycle.next();
        if (state?.status === AgentStatus.Failed) {
          throw new Error(
            state.error?.message ?? "generated Agent join failed",
          );
        }
      } while (state && state.status !== AgentStatus.Joined);
      const joined = agent.state();
      await agent.close();
      const final = await lifecycle.next();
      joinedAndClosed =
        state?.room === config.room &&
        joined !== current &&
        joined.status === AgentStatus.Joined &&
        final?.status === AgentStatus.Closed &&
        final.room === undefined &&
        agent.state().observationRevision === final.observationRevision &&
        (await lifecycle.next()) === undefined;
    } finally {
      lifecycle.close();
      lifecycle.uniffiDestroy();
    }
  } finally {
    await agent.close();
    cursor.close();
    cursor.uniffiDestroy();
    agent.uniffiDestroy();
  }
  const track = document
    .createElement("canvas")
    .captureStream(5)
    .getVideoTracks()[0];
  const stream = new MediaStream([track]);
  try {
    const tracks = web.converters.FfiConverterTypeWebMediaTrack;
    const streams = web.converters.FfiConverterTypeWebMediaStream;
    return {
      wasmLoaded: wasmInstances === 1,
      initialState,
      stableState,
      pendingReadClosed,
      pendingDataReadsClosed,
      joinedAndClosed,
      exactVideoOwner,
      trackIdentity: tracks.lift(tracks.lower(track)) === track,
      streamIdentity: streams.lift(streams.lower(stream)) === stream,
    };
  } finally {
    registry.invalidateMediaForTest(track);
    registry.invalidateMediaForTest(stream);
    track.stop();
  }
})();
