import assert from "node:assert/strict";
import { Track } from "./platform.mjs";

export async function localOperationContract(BrowserRuntime) {
  const runtime = new BrowserRuntime({
    endpoint: "http://localhost",
    token: "runtime-local-source-contract",
  });
  const firstTrack = new Track("video");
  const secondTrack = new Track("video");
  const originalClone = MediaStreamTrack.prototype.clone;
  const ownedTracks = [];
  const cloneSources = new WeakMap();
  MediaStreamTrack.prototype.clone = function () {
    const clone = originalClone.call(this);
    ownedTracks.push(clone);
    cloneSources.set(clone, this);
    return clone;
  };
  const config = { contentHint: "motion" };
  try {
    assert.equal(runtime.diagnostics().peers, 0);
    assert.equal(runtime.replace_local_track, undefined);
    assert.equal(runtime.set_local_muted, undefined);
    const initial = await runtime.resource({
      op: "claimPublication",
      spec: { kind: "video", label: "camera" },
    });
    await runtime.replace_owned_track(
      initial.owner,
      firstTrack,
      config,
      false,
      true,
    );
    const firstClone = ownedTracks.at(-1);
    assert.notEqual(firstClone.id, firstTrack.id);
    assert.equal(cloneSources.get(firstClone), firstTrack);
    await runtime.set_owned_muted(initial.owner, true);
    assert.equal(firstClone.enabled, false);
    assert.equal(firstTrack.enabled, true);

    await runtime.replace_owned_track(
      initial.owner,
      secondTrack,
      config,
      true,
      true,
    );
    const secondClone = ownedTracks.at(-1);
    assert.equal(firstClone.readyState, "ended");
    assert.equal(firstTrack.readyState, "live");
    assert.equal(secondClone.enabled, false);
    assert.equal(secondTrack.enabled, true);
    await runtime.replace_owned_track(initial.owner, null, config, true, true);
    assert.equal(secondClone.readyState, "ended");
    assert.equal(secondTrack.readyState, "live");
    await runtime.replace_owned_track(
      initial.owner,
      secondTrack,
      config,
      true,
      true,
    );
    assert.equal(ownedTracks.at(-1).enabled, false);
    await runtime.set_owned_muted(initial.owner, false);
    assert.equal(ownedTracks.at(-1).enabled, true);
    await runtime.replace_owned_track(initial.owner, null, config, false, true);
    await runtime.resource({ op: "release", owner: initial.owner });

    const publication = await runtime.resource({
      op: "claimPublication",
      spec: { kind: "video", label: "camera" },
    });
    await runtime.replace_owned_track(
      publication.owner,
      firstTrack,
      config,
      false,
      true,
    );
    const releasedClone = ownedTracks.at(-1);
    await runtime.set_owned_muted(publication.owner, true);
    assert.equal(releasedClone.enabled, false);
    await runtime.resource({ op: "release", owner: publication.owner });
    assert.equal(releasedClone.readyState, "ended");
    assert.equal(firstTrack.readyState, "live");
    assert.throws(
      () =>
        runtime.replace_owned_track(
          publication.owner,
          firstTrack,
          config,
          false,
          true,
        ),
      (error) => error.pulsebeamCode === "released",
    );
    const reused = await runtime.resource({
      op: "claimPublication",
      spec: { kind: "video", label: "screen" },
    });
    assert.equal(reused.slot, publication.slot);
    assert.notEqual(reused.owner, publication.owner);
    await runtime.replace_owned_track(
      reused.owner,
      secondTrack,
      config,
      false,
      true,
    );
    const reusedClone = ownedTracks.at(-1);
    assert.equal(cloneSources.get(reusedClone), secondTrack);
    assert.equal(reusedClone.enabled, true);
    assert.equal(reusedClone.readyState, "live");
    assert.equal(runtime.diagnostics().peers, 0);
    const obsolete = runtime.replace_owned_track(
      reused.owner,
      firstTrack,
      config,
      false,
      true,
    );
    const latest = runtime.replace_owned_track(
      reused.owner,
      secondTrack,
      config,
      true,
      true,
    );
    await Promise.all([obsolete, latest]);
    const latestClone = ownedTracks.at(-1);
    assert.equal(cloneSources.get(latestClone), secondTrack);
    assert.equal(latestClone.enabled, false);
    await runtime.set_owned_muted(reused.owner, false);
    assert.equal(latestClone.enabled, true);
    const closing = runtime.replace_owned_track(
      reused.owner,
      firstTrack,
      config,
      false,
      true,
    );
    runtime.close();
    await assert.rejects(closing, /closed/);
    assert(ownedTracks.every((track) => track.readyState === "ended"));
    assert.equal(firstTrack.readyState, "live");
    assert.equal(secondTrack.readyState, "live");
    assert.throws(() => runtime.set_owned_muted(reused.owner, true), /closed/);
    assert.throws(
      () =>
        runtime.replace_owned_track(
          reused.owner,
          secondTrack,
          config,
          false,
          true,
        ),
      /closed/,
    );
    return true;
  } finally {
    MediaStreamTrack.prototype.clone = originalClone;
    runtime.abort();
    firstTrack.stop();
    secondTrack.stop();
  }
}
