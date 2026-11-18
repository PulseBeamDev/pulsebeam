import assert from "node:assert/strict";
import { setImmediate } from "node:timers/promises";
import { Track } from "./platform.mjs";

export async function mediaResourceContract(pulsebeam) {
  const agent = new pulsebeam.Agent({
    endpoint: "http://localhost",
    token: "media-resource-contract",
    logging: { level: "off" },
  });
  const borrowed = new Track("video");
  const source = pulsebeam.VideoSource.fromTrack(borrowed);
  try {
    const beforeLookup = agent.snapshot().desiredRevision;
    const local = agent.video("camera");
    assert.equal(agent.video("camera"), local);
    const other = new pulsebeam.Agent({
      endpoint: "http://localhost",
      token: "other-media-resource-contract",
      logging: { level: "off" },
    });
    assert.notEqual(other.video("camera"), local);
    await other.close();
    assert.equal(
      agent.snapshot().desiredRevision,
      beforeLookup,
      "inert lookup cannot claim a sender",
    );

    const first = local.publish({ enabled: false });
    assert.equal(first instanceof Promise, false);
    const input = source.pipeTo(first);
    assert.equal(input instanceof Promise, false);
    assert.equal(input.state(), input.state());
    const initial = first.state();
    assert.equal(initial.kind, pulsebeam.PublicationMediaKind.Video);
    assert.equal(initial.label, "camera");
    assert.equal(initial.closed, false);
    assert.equal(
      initial.admission.status,
      pulsebeam.PublicationAdmissionStatus.Pending,
    );
    assert.equal(initial.configuration.enabled, false);
    assert.equal(
      first.state(),
      initial,
      "unchanged Core revision reuses state identity",
    );
    assert.equal(Object.isFrozen(initial), true);
    assert.equal(Object.isFrozen(initial.configuration), true);
    assert.equal(Object.isFrozen(initial.admission), true);
    const cursorA = first.updates();
    const cursorB = first.updates();
    assert.equal(await cursorA.next(), initial);
    assert.equal(await cursorB.next(), initial);
    const pending = cursorA.next();
    await assert.rejects(
      cursorA.next(),
      (error) =>
        error instanceof pulsebeam.OperationError &&
        error.info.code === pulsebeam.ErrorCode.Conflict,
    );
    await first.configure({ enabled: true });
    const configured = await pending;
    assert.equal(configured.configuration.enabled, true);
    assert.notEqual(configured, initial);
    assert.equal(await cursorB.next(), configured);
    cursorA.close();
    cursorB.close();
    const boundedCursors = Array.from({ length: 32 }, () => first.updates());
    assert.throws(
      () => first.updates(),
      (error) =>
        error instanceof pulsebeam.OperationError &&
        error.info.code === pulsebeam.ErrorCode.CapacityExceeded,
    );
    boundedCursors.forEach((cursor) => cursor.close());
    assert.throws(
      () => local.publish({ enabled: true }),
      (error) =>
        error instanceof pulsebeam.OperationError &&
        error.info.code === pulsebeam.ErrorCode.Conflict,
    );
    assert.equal(borrowed.readyState, "live");
    await first.configure({ enabled: true });
    await input.close();
    assert.equal(input.state().status, pulsebeam.PipeStatus.Closed);
    assert.equal(first.isClosed(), false);
    assert.equal(
      borrowed.readyState,
      "live",
      "source withdrawal cannot stop capture",
    );
    const retiredCursor = first.updates();
    await first.close();
    assert.equal(first.state().closed, true);
    assert.equal((await retiredCursor.next()).closed, true);
    retiredCursor.close();
    const replacement = local.publish({ enabled: true });
    source.pipeTo(replacement);
    await replacement.close();
    assert.equal(borrowed.readyState, "live");

    const participant = agent.room("room").participant("absent");
    const video = await participant.video("camera").subscribe({
      visible: true,
      height: 360,
      minHeight: 0,
      minFps: 0,
      priority: 1,
      playoutDelay: { mode: "adaptive" },
    });
    const audio = await participant.audio("microphone").subscribe({
      playoutDelay: { mode: "adaptive" },
    });
    const selection = await agent.selectAudio({});
    await selection.close();
    assert.equal(
      audio.isClosed(),
      false,
      "selection close cannot release named audio ownership",
    );
    const pendingVideo = assert.rejects(video.observe(), { code: "closed" });
    const pendingAudio = assert.rejects(audio.observe(), { code: "closed" });
    const pendingPublication = agent.video("late").publish({ enabled: true });
    const pendingPipe = source.pipeTo(pendingPublication);
    await agent.close();
    await Promise.all([pendingVideo, pendingAudio]);
    assert.equal(pendingPublication.isClosed(), true);
    assert.equal(pendingPipe.state().status, pulsebeam.PipeStatus.Closed);
    assert.equal(video.isClosed(), true);
    assert.equal(audio.isClosed(), true);
    assert.equal(
      source.isClosed(),
      false,
      "Agent retirement cannot transfer caller-owned capture",
    );
    await ownedPipeContract();
    await publicationRebindContract(source);
  } finally {
    await agent.close();
    await source.close();
  }
}

async function ownedPipeContract() {
  const { BrowserRuntime } = await import("../../node_assets/dist/wasm.js");
  const runtime = new BrowserRuntime({
    endpoint: "http://localhost",
    token: "owned-pipe-contract",
  });
  try {
    const declaration = { kind: "video", label: "pipe-contract" };
    const publication = runtime.claim_owned_publication(declaration);
    assert.equal(publication.then, undefined, "declaration is synchronous");
    const observation = runtime.publication(publication.owner);
    const pending = observation.state();
    assert.equal(
      pending.admission.status,
      0,
      "declaration is pending without admission",
    );
    assert.equal(pending.inputAttached, false, "declaration needs no source");
    assert.throws(
      () => runtime.claim_owned_publication(declaration),
      (error) => error.pulsebeamInfo?.code === 1,
      "duplicate publication conflicts before actor progress",
    );
    assert.deepEqual(
      observation.state(),
      pending,
      "rejected declaration preserves incumbent",
    );
    const pipe = runtime.create_owned_publication_pipe(publication.owner);
    assert.equal(observation.state().inputAttached, true);
    assert.throws(
      () => runtime.create_owned_publication_pipe(publication.owner),
      (error) => error.pulsebeamInfo?.code === 1,
      "attachment conflict retains the shared Core classification",
    );
    const borrowed = new Track("video");
    const clone = borrowed.clone.bind(borrowed);
    let ownedClone;
    borrowed.clone = () => (ownedClone = clone());
    await pipe.install_track(borrowed, {
      contentHint: "text",
      degradationPreference: null,
      encodings: [],
    });
    const cursor = pipe.updates();
    const initial = await cursor.next();
    assert.equal(
      initial.status,
      1,
      "Core reports the registered input available",
    );
    assert.equal(pipe.state().status, initial.status);
    const finalRead = cursor.next();
    const cleanup = pipe.close();
    assert.equal(
      ownedClone.enabled,
      false,
      "close immediately fences the owned sender track",
    );
    assert.equal(
      pipe.close(),
      cleanup,
      "repeated close waits on the same cleanup",
    );
    const replacement = runtime.create_owned_publication_pipe(
      publication.owner,
    );
    const replacementSource = new Track("video");
    await replacement.install_track(replacementSource, {
      contentHint: "text",
      degradationPreference: null,
      encodings: [],
    });
    await cleanup;
    assert.equal(
      replacement.state().status,
      1,
      "old cleanup cannot detach the replacement",
    );
    await replacement.close();
    assert.equal(replacementSource.readyState, "live");
    assert.equal(
      borrowed.readyState,
      "live",
      "pipe close preserves borrowed capture",
    );
    assert.equal(
      pipe.state().status,
      3,
      "close synchronously revokes the pipe",
    );
    assert.equal((await finalRead).status, 3);
    assert.equal(
      await cursor.next(),
      undefined,
      "final state is followed by normal end",
    );
    await pipe.close();
    cursor.close();

    const legacyPublication = await runtime.resource({
      op: "claimPublication",
      spec: { kind: "video", label: "legacy-replacement" },
    });
    const superseded = runtime.create_owned_publication_pipe(
      legacyPublication.owner,
    );
    const supersededUpdates = superseded.updates();
    await supersededUpdates.next();
    const revoked = supersededUpdates.next();
    const legacySource = new Track("video");
    await runtime.replace_owned_track(
      legacyPublication.owner,
      legacySource,
      {
        contentHint: "text",
        degradationPreference: null,
        encodings: [],
      },
      false,
      true,
    );
    assert.equal(
      (await revoked).status,
      3,
      "a replaced input is terminal, not stale Pending",
    );
    assert.equal(await supersededUpdates.next(), undefined);
    assert.equal(superseded.state().status, 3);
    await superseded.close();
    assert.equal(legacySource.readyState, "live");
    await runtime.resource({ op: "release", owner: legacyPublication.owner });
    supersededUpdates.close();

    const second = await runtime.resource({
      op: "claimPublication",
      spec: { kind: "video", label: "release-pipe-contract" },
    });
    const released = runtime.create_owned_publication_pipe(second.owner);
    const releaseCursor = released.updates();
    await releaseCursor.next();
    const releaseRead = releaseCursor.next();
    await runtime.resource({ op: "release", owner: second.owner });
    assert.equal((await releaseRead).status, 3);
    assert.equal(await releaseCursor.next(), undefined);
    releaseCursor.close();

    const racePublication = await runtime.resource({
      op: "claimPublication",
      spec: { kind: "video", label: "install-close-race-contract" },
    });
    const racingPipe = runtime.create_owned_publication_pipe(
      racePublication.owner,
    );
    const racingSource = new Track("video");
    const installing = racingPipe.install_track(racingSource, {
      contentHint: "text",
      degradationPreference: null,
      encodings: [],
    });
    const closing = racingPipe.close();
    const [installation, cleanupResult] = await Promise.allSettled([
      installing,
      closing,
    ]);
    assert.equal(
      cleanupResult.status,
      "fulfilled",
      "racing close must finish local cleanup",
    );
    if (installation.status === "rejected") {
      assert.equal(installation.reason.pulsebeamInfo?.code, 8);
    }
    assert.equal(racingPipe.state().status, 3);
    assert.equal(racingSource.readyState, "live");

    const third = await runtime.resource({
      op: "claimPublication",
      spec: { kind: "video", label: "shutdown-pipe-contract" },
    });
    const retained = runtime.create_owned_publication_pipe(third.owner);
    const shutdownCursor = retained.updates();
    await shutdownCursor.next();
    const shutdownRead = shutdownCursor.next();
    await runtime.close();
    assert.equal((await shutdownRead).status, 3);
    assert.equal(await shutdownCursor.next(), undefined);
    shutdownCursor.close();

    const abortRuntime = new BrowserRuntime({
      endpoint: "http://localhost",
      token: "abort-pipe",
    });
    try {
      const publication = await abortRuntime.resource({
        op: "claimPublication",
        spec: { kind: "video", label: "abort" },
      });
      const pipe = abortRuntime.create_owned_publication_pipe(
        publication.owner,
      );
      const updates = pipe.updates();
      await updates.next();
      const finalRead = updates.next();
      abortRuntime.abort();
      assert.equal((await finalRead).status, 3);
      assert.equal(await updates.next(), undefined);
      await pipe.close();
      updates.close();
    } finally {
      abortRuntime.abort();
    }
  } finally {
    await runtime.close();
  }
}

async function publicationRebindContract(source) {
  const { createManagedAgent } = await import(
    "../../node_assets/dist/internal/react.js"
  );
  const agent = createManagedAgent({
    endpoint: "http://localhost",
    token: "publication-rebind",
    logging: { level: "off" },
  });
  agent.activate();
  try {
    await agent.initialized();
    const publication = agent.video("rebound").publish({ enabled: false });
    const oldPipe = source.pipeTo(publication);
    const oldCursor = publication.updates();
    await oldCursor.next();
    agent.deactivate();
    assert.equal(oldPipe.isClosed(), true);
    assert.equal(publication.state().closed, true);
    agent.activate();
    await agent.initialized();
    let rebound;
    for (let turn = 0; turn < 32; turn++) {
      const state = publication.state();
      if (!state.closed) {
        rebound = state;
        break;
      }
      await setImmediate();
    }
    assert.ok(rebound, "publication did not bind to its new Core owner");
    assert.equal(
      publication.state(),
      rebound,
      "rebound state identity is stable",
    );
    assert.equal((await oldCursor.next()).closed, true);
    const current = publication.state();
    assert.equal(
      current.closed,
      false,
      "old cursor cannot restore closed state",
    );
    assert.equal(
      publication.state(),
      current,
      "old cursor cannot poison cache",
    );
    oldCursor.close();
  } finally {
    await agent.close();
  }
}
