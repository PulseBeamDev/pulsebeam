import {
  Agent,
  AgentError,
  AgentStatus,
  DataDelivery,
  DataEvent,
  DataPublishOptions,
  DataSendOptions,
  ErrorCode,
  LogLevel,
  PipeStatus,
  PublicationAdmissionStatus,
  VideoSource,
  WatchStatus,
} from "@pulsebeam/web";
import type { RemoteVideoTrack, VideoSubscription } from "@pulsebeam/web";
import {
  attachMedia,
  type BorrowedMediaAttachment,
} from "@pulsebeam/web/internal/react";
import { createElement, useLayoutEffect, type ReactNode } from "react";
import { createRoot } from "react-dom/client";
import { useResource } from "../../src/resource.js";
export { recoveryFixture } from "../application-contract/recovery.js";

function ReceiveVideo({
  camera,
  onOwner,
  children,
}: {
  camera: RemoteVideoTrack;
  onOwner: (owner: VideoSubscription) => void;
  children?: ReactNode;
}) {
  const { value: owner } = useResource(camera, () => camera.subscribe());
  useLayoutEffect(() => {
    if (owner) onOwner(owner);
  }, [owner, onOwner]);
  return children ?? null;
}

async function expectDecodedTone(track: MediaStreamTrack): Promise<void> {
  const stream = new MediaStream([track]);
  const decoder = document.createElement("audio");
  decoder.muted = true;
  decoder.srcObject = stream;
  const context = new AudioContext();
  try {
    await Promise.all([decoder.play(), context.resume()]);
    const analyser = context.createAnalyser();
    analyser.smoothingTimeConstant = 0;
    const sink = context.createGain();
    sink.gain.value = 0;
    context
      .createMediaStreamSource(stream)
      .connect(analyser)
      .connect(sink)
      .connect(context.destination);
    const spectrum = new Float32Array(analyser.frequencyBinCount);
    for (let attempt = 0; attempt < 150; attempt++) {
      analyser.getFloatFrequencyData(spectrum);
      let peak = 0;
      for (let index = 1; index < spectrum.length; index++) {
        if (spectrum[index] > spectrum[peak]) peak = index;
      }
      const frequency = (peak * context.sampleRate) / analyser.fftSize;
      if (Math.abs(frequency - 1000) < 50 && spectrum[peak] > -65) return;
      await new Promise((resolve) => setTimeout(resolve, 20));
    }
    throw new Error(
      "generated audio output did not decode the native 1 kHz tone",
    );
  } finally {
    decoder.pause();
    decoder.srcObject = null;
    await context.close();
  }
}

export async function receiveGenerated(config: {
  endpoint: string;
  token: string;
  room: string;
  sourceRoom: string;
  catalogToken: string;
}): Promise<boolean> {
  const agent = new Agent({
    endpoint: config.endpoint,
    token: config.token,
    logLevel: LogLevel.Off,
  });
  agent.setLogging(LogLevel.Warn);
  if (!Agent.instanceOf(agent)) throw new Error("expected a generated Agent");
  const state = agent.state();
  if (state.status !== AgentStatus.Idle || agent.state() !== state) {
    throw new Error("Packed Agent is not synchronously inert");
  }
  const room = agent.room(config.sourceRoom);
  const participant = room.participant("native-publisher");
  const camera = participant.video("camera");
  const localData = agent.data("local-events");
  try {
    const address = camera.selector();
    if (
      room.name() !== config.sourceRoom ||
      address.room !== config.sourceRoom ||
      address.participantId !== "native-publisher" ||
      address.trackId !== "camera" ||
      localData.trackId() !== "local-events"
    ) {
      throw new Error(
        "Generated logical handles lost their addresses before admission",
      );
    }
    if (
      agent.room(config.sourceRoom) !== room ||
      room.participant("native-publisher") !== participant ||
      participant.video("camera") !== camera ||
      agent.data("local-events") !== localData ||
      agent.state() !== state
    ) {
      throw new Error(
        "Equivalent generated lookups must stay inert and stable",
      );
    }
  } finally {
    localData.uniffiDestroy();
  }
  const replacementData = agent.data("local-events");
  try {
    if (replacementData === localData) {
      throw new Error("Generated lookup reused a destroyed reference");
    }
  } finally {
    replacementData.uniffiDestroy();
  }
  const container = document.getElementById("root");
  if (!container) throw new Error("missing React fixture root");
  const root = createRoot(container);
  let reception: VideoSubscription | undefined;
  try {
    reception = await new Promise<VideoSubscription>((resolve) => {
      root.render(createElement(ReceiveVideo, { camera, onOwner: resolve }));
    });
    const videoConfiguration = reception.state().configuration;
    reception.configure({ ...videoConfiguration, visible: false });
    if (reception.state().configuration.visible) {
      throw new Error(
        "Generated video reconfiguration did not reach the same owner",
      );
    }
    reception.configure();
    if (!reception.state().configuration.visible) {
      throw new Error(
        "Generated video reconfiguration did not restore default visibility",
      );
    }
    const updates = agent.updates();
    try {
      const first = await updates.next();
      if (first?.observationRevision !== state.observationRevision) {
        throw new Error("packed cursor did not return the initial state");
      }
      const peers = Array.from({ length: 31 }, () => agent.updates());
      try {
        try {
          agent.updates();
          throw new Error("packed Agent admitted an observer above capacity");
        } catch (error) {
          if (
            !AgentError.Operation.instanceOf(error) ||
            error.inner.info.code !== ErrorCode.CapacityExceeded
          ) {
            throw error;
          }
        }
      } finally {
        for (const cursor of peers) {
          cursor.close();
          cursor.uniffiDestroy();
        }
      }
      const pending = updates.next();
      updates.close();
      if ((await pending) !== undefined) {
        throw new Error("packed cursor close did not end the pending read");
      }
    } finally {
      updates.close();
      updates.uniffiDestroy();
    }
    const catalog = room.watch();
    const catalogUpdates = catalog.updates();
    try {
      const loading = catalog.state();
      if (
        loading.status !== WatchStatus.Loading ||
        loading.room !== config.sourceRoom ||
        loading.limit !== 100 ||
        catalog.state() !== loading ||
        (await catalogUpdates.next())?.observationRevision !==
          loading.observationRevision
      ) {
        throw new Error("packed catalog missed its bounded initial state");
      }
      const pendingCatalog = catalogUpdates.next();
      await catalog.close();
      if (
        (await pendingCatalog)?.status !== WatchStatus.Closed ||
        (await catalogUpdates.next()) !== undefined
      ) {
        throw new Error("packed catalog missed terminal state");
      }
    } finally {
      catalogUpdates.close();
      await catalog.close();
      catalogUpdates.uniffiDestroy();
      catalog.uniffiDestroy();
    }
    const remoteAudio = participant.audio("microphone");
    const audio = remoteAudio.subscribe();
    const audioUpdates = audio.updates();
    try {
      const initialAudio = audio.state();
      if (
        initialAudio.closed ||
        initialAudio.selector.room !== config.sourceRoom ||
        initialAudio.selector.participantId !== "native-publisher" ||
        initialAudio.selector.trackId !== "microphone" ||
        audio.state() !== initialAudio ||
        (await audioUpdates.next())?.observationRevision !==
          initialAudio.observationRevision
      ) {
        throw new Error(
          "packed audio subscription lost its exact source or initial state",
        );
      }
      const pendingOutput = audio.playbackOutput().then(
        (output) => ({ output }),
        (error: unknown) => ({ error }),
      );
      const pendingAudio = audioUpdates.next();
      await audio.close();
      const result = await pendingOutput;
      if ("output" in result) {
        await result.output.close();
        result.output.uniffiDestroy();
        throw new Error(
          "an absent exact audio source acquired playback output",
        );
      }
      if (
        !AgentError.Operation.instanceOf(result.error) ||
        result.error.inner.info.code !== ErrorCode.Closed
      ) {
        throw new Error("closing exact audio did not end playback acquisition");
      }
      if (
        !(await pendingAudio)?.closed ||
        (await audioUpdates.next()) !== undefined
      ) {
        throw new Error("packed audio subscription missed terminal state");
      }
    } finally {
      audioUpdates.close();
      await audio.close();
      audioUpdates.uniffiDestroy();
      audio.uniffiDestroy();
      remoteAudio.uniffiDestroy();
    }
    const localData = agent.data("pending-data");
    const remoteData = participant.data("frame-events");
    const publicationData = localData.publish();
    const subscriptionData = remoteData.subscribe();
    try {
      const pendingData = subscriptionData.next();
      try {
        await subscriptionData.next();
        throw new Error("overlapping generated Web data read was admitted");
      } catch (error) {
        if (
          !AgentError.Operation.instanceOf(error) ||
          error.inner.info.code !== ErrorCode.Conflict
        )
          throw error;
      }
      for (const owner of [publicationData, subscriptionData]) {
        const updates = owner.updates();
        try {
          const initial = owner.state();
          if (
            initial.admission !== PublicationAdmissionStatus.Pending ||
            initial.closed ||
            owner.state() !== initial ||
            (await updates.next())?.observationRevision !==
              initial.observationRevision
          ) {
            throw new Error("packed data owner missed coherent pending state");
          }
          const pending = updates.next();
          await owner.close();
          if (
            !(await pending)?.closed ||
            (await updates.next()) !== undefined
          ) {
            throw new Error("packed data owner missed terminal state");
          }
        } finally {
          updates.close();
          updates.uniffiDestroy();
        }
      }
      const reliable = localData.publish(
        DataPublishOptions.create({ delivery: DataDelivery.Reliable }),
      );
      try {
        if (reliable.state().delivery !== publicationData.state().delivery) {
          throw new Error(
            "omitting generated publish options did not select reliable delivery",
          );
        }
      } finally {
        await reliable.close();
        reliable.uniffiDestroy();
      }
      if ((await pendingData) !== undefined)
        throw new Error(
          "closed generated data subscription did not end pending read",
        );
      try {
        await publicationData.send(
          new Uint8Array([0, 255]),
          DataSendOptions.create({
            capturedAt: {
              clock: { id: "pending-data", epoch: 1n },
              nanos: -(1n << 63n),
              uncertaintyNs: 0,
            },
          }),
        );
        throw new Error("closed generated data publication accepted bytes");
      } catch (error) {
        if (
          !AgentError.Operation.instanceOf(error) ||
          error.inner.info.code !== ErrorCode.Closed
        )
          throw error;
      }
    } finally {
      await publicationData.close();
      await subscriptionData.close();
      publicationData.uniffiDestroy();
      subscriptionData.uniffiDestroy();
      localData.uniffiDestroy();
      remoteData.uniffiDestroy();
    }
    const local = agent.video("pending-camera");
    const publication = local.publish();
    const publicationUpdates = publication.updates();
    try {
      const initial = publication.state();
      if (
        initial.admission.status !== PublicationAdmissionStatus.Pending ||
        publication.state() !== initial ||
        (await publicationUpdates.next())?.observationRevision !==
          initial.observationRevision
      ) {
        throw new Error("packed publication has no coherent pending state");
      }
      publication.configure({ enabled: false });
      if (
        publication.state().configuration.enabled ||
        publication.state().closed
      ) {
        throw new Error(
          "Generated publication reconfiguration replaced or left the owner enabled",
        );
      }
      publication.configure({ enabled: true });
      const track = document
        .createElement("canvas")
        .captureStream(5)
        .getVideoTracks()[0];
      const source = VideoSource.fromTrack(track);
      const previewElement = document.createElement("video");
      const failPreview = (error: unknown) => {
        throw error;
      };
      const preview = attachMedia(
        source,
        "video",
        previewElement,
        false,
        failPreview,
        failPreview,
      );
      const mirror = VideoSource.fromTrack(track);
      try {
        if (mirror.previewTrack() !== track)
          throw new Error("Generated source preview copied the borrowed track");
      } finally {
        mirror.uniffiDestroy();
      }
      const previewStream = previewElement.srcObject;
      if (
        !(previewStream instanceof MediaStream) ||
        previewStream.getVideoTracks()[0] !== track ||
        source.previewTrack() !== track
      )
        throw new Error(
          "Closing another source's lease detached the borrowed preview",
        );
      const pipe = source.pipeTo(publication);
      const pipeUpdates = pipe.updates();
      try {
        const current = await pipeUpdates.next();
        if (!current || current.status === PipeStatus.Closed) {
          throw new Error("packed pipe closed before input attachment");
        }
        await pipe.close();
        if (
          (await pipeUpdates.next())?.status !== PipeStatus.Closed ||
          (await pipeUpdates.next()) !== undefined ||
          track.readyState !== "live" ||
          publication.state().closed
        ) {
          throw new Error("packed pipe close did not isolate borrowed capture");
        }
        await source.close();
        if (
          previewElement.srcObject !== null ||
          source.previewTrack() !== undefined
        )
          throw new Error(
            "Closing a generated source did not detach its preview",
          );
      } finally {
        pipeUpdates.close();
        await pipe.close();
        await preview.close();
        await source.close();
        pipeUpdates.uniffiDestroy();
        pipe.uniffiDestroy();
        source.uniffiDestroy();
      }
      await publication.close();
      if (
        !(await publicationUpdates.next())?.closed ||
        (await publicationUpdates.next()) !== undefined
      ) {
        throw new Error("packed publication cursor missed terminal state");
      }
    } finally {
      publicationUpdates.close();
      await publication.close();
      publicationUpdates.uniffiDestroy();
      publication.uniffiDestroy();
      local.uniffiDestroy();
    }
    const cancellation = new AbortController();
    let settled = false;
    const pendingPlayback = reception
      .playbackOutput({ signal: cancellation.signal })
      .then(
        async (output) => {
          settled = true;
          await output.close();
          output.uniffiDestroy();
          return true;
        },
        () => {
          settled = true;
          return false;
        },
      );
    await new Promise((resolve) => setTimeout(resolve, 0));
    if (settled)
      throw new Error("Playback acquisition was not waiting before join");
    cancellation.abort();
    if ((await pendingPlayback) || reception.state().closed)
      throw new Error("Cancelling playback must leave the subscription open");
    agent.join(config.room);
    const videoSubscription = reception;
    let attachment: BorrowedMediaAttachment | undefined;
    try {
      const track = await new Promise<MediaStreamTrack>((resolve, reject) => {
        root.render(
          createElement(
            ReceiveVideo,
            {
              camera: participant.video("camera"),
              onOwner(owner) {
                if (owner !== reception)
                  reject(
                    new Error(
                      "A React rerender replaced an equivalent generated track",
                    ),
                  );
              },
            },
            createElement("video", {
              muted: true,
              autoPlay: true,
              playsInline: true,
              ref(element: HTMLVideoElement | null) {
                if (element) {
                  attachment = attachMedia(
                    videoSubscription,
                    "video",
                    element,
                    true,
                    reject,
                    reject,
                  );
                }
              },
              onError: () =>
                reject(new Error("generated video playback failed")),
              onLoadedData(event: { currentTarget: HTMLVideoElement }) {
                try {
                  const video = event.currentTarget;
                  const canvas = document.createElement("canvas");
                  canvas.width = video.videoWidth;
                  canvas.height = video.videoHeight;
                  const context = canvas.getContext("2d");
                  if (
                    !context ||
                    canvas.width !== 160 ||
                    canvas.height !== 96
                  ) {
                    throw new Error("missing expected decoded video geometry");
                  }
                  context.drawImage(video, 0, 0);
                  const pixel = context.getImageData(80, 48, 1, 1).data;
                  if (pixel[0] < 150 || pixel[1] < 50 || pixel[3] !== 255) {
                    throw new Error(
                      "generated output did not decode native pixels",
                    );
                  }
                  const stream = video.srcObject;
                  const track =
                    stream instanceof MediaStream && stream.getVideoTracks()[0];
                  if (!track)
                    throw new Error("Decoded video has no borrowed track");
                  resolve(track);
                } catch (error) {
                  reject(error);
                }
              },
            }),
          ),
        );
      });
      await attachment?.close();
      if (track.readyState !== "ended" || reception.state().closed)
        return false;
    } finally {
      await attachment?.close();
    }
    const events = participant.data("frame-events");
    const stream = events.subscribe();
    try {
      const event = await stream.next();
      if (!DataEvent.Message.instanceOf(event)) {
        throw new Error(
          "generated exact subscription did not receive a complete message",
        );
      }
      const { data, participantId, publicationId, sequence, capturedAt } =
        event.inner;
      if (
        data.length !== 11 ||
        data[0] !== 0 ||
        data[1] !== 255 ||
        data[2] !== 128 ||
        participantId !== "native-publisher" ||
        !publicationId ||
        typeof sequence !== "bigint"
      ) {
        throw new Error(
          "generated data lost its payload or source attribution",
        );
      }
      const sourceSequence = new DataView(
        data.buffer,
        data.byteOffset,
        data.byteLength,
      ).getBigUint64(3, true);
      if (
        capturedAt?.clock.id !== "frame-events" ||
        capturedAt.clock.epoch !== 1n ||
        capturedAt.nanos !== -9007199254740993n + sourceSequence ||
        capturedAt.uncertaintyNs !== 0
      ) {
        throw new Error(
          "generated data lost capture timestamp precision or clock attribution",
        );
      }
    } finally {
      await stream.close();
      stream.uniffiDestroy();
      events.uniffiDestroy();
    }
    const microphone = participant.audio("microphone");
    const listening = microphone.subscribe();
    try {
      const audioOutput = await listening.playbackOutput();
      try {
        const track = audioOutput.track();
        if (!track || track.kind !== "audio")
          throw new Error("missing exact audio track");
        await expectDecodedTone(track);
        await audioOutput.close();
        if (track.readyState !== "ended" || listening.state().closed) {
          throw new Error(
            "audio playback close did not isolate subscription ownership",
          );
        }
      } finally {
        await audioOutput.close();
        audioOutput.uniffiDestroy();
      }
    } finally {
      await listening.close();
      listening.uniffiDestroy();
      microphone.uniffiDestroy();
    }
    const forbiddenRoster = room.watch();
    const forbiddenUpdates = forbiddenRoster.updates();
    try {
      while (true) {
        const state = await forbiddenUpdates.next();
        if (state?.status === WatchStatus.Loading) continue;
        if (
          state?.status !== WatchStatus.Failed ||
          state.error?.code !== ErrorCode.PermissionDenied ||
          state.participants !== undefined
        ) {
          throw new Error("Subscribe-only token gained catalog access");
        }
        break;
      }
    } finally {
      forbiddenUpdates.close();
      await forbiddenRoster.close();
      forbiddenUpdates.uniffiDestroy();
      forbiddenRoster.uniffiDestroy();
    }
    const viewer = new Agent({
      endpoint: config.endpoint,
      token: config.catalogToken,
    });
    const rosterRoom = viewer.room(config.sourceRoom);
    const roster = rosterRoom.watch();
    const rosterUpdates = roster.updates();
    try {
      viewer.join(config.sourceRoom);
      while (true) {
        const state = await rosterUpdates.next();
        if (!state || state.error)
          throw new Error("generated catalog did not synchronize");
        if (!state.participants) continue;
        const entries = state.participants;
        const publisher = entries.find(
          (entry) => entry.id() === "native-publisher",
        );
        try {
          if (!publisher?.tracks().some((track) => track.trackId === "camera"))
            continue;
          await roster.close();
          const cameraRef = publisher.video("camera");
          const interest = cameraRef.subscribe();
          const states = interest.updates();
          try {
            const { selector } = interest.state();
            if (
              selector.room !== config.sourceRoom ||
              selector.participantId !== publisher.id() ||
              selector.trackId !== "camera"
            ) {
              throw new Error(
                "generated catalog entry did not retain its exact track address",
              );
            }
            while (true) {
              const state = await states.next();
              if (!state || state.closed || state.sourceTrackId)
                throw new Error("Discover-only token gained video reception");
              if (state.error) {
                if (state.error.code !== ErrorCode.PermissionDenied)
                  throw new Error("Unexpected Discover-only reception error");
                break;
              }
            }
          } finally {
            states.close();
            states.uniffiDestroy();
            await interest.close();
            interest.uniffiDestroy();
            cameraRef.uniffiDestroy();
          }
          break;
        } finally {
          for (const entry of entries) entry.uniffiDestroy();
        }
      }
    } finally {
      rosterUpdates.close();
      await roster.close();
      rosterUpdates.uniffiDestroy();
      roster.uniffiDestroy();
      rosterRoom.uniffiDestroy();
      await viewer.close();
      viewer.uniffiDestroy();
    }
    const lifecycle = reception.updates();
    try {
      await lifecycle.next();
      const closed = lifecycle.next();
      root.unmount();
      if (!(await closed)?.closed || (await lifecycle.next()) !== undefined)
        throw new Error(
          "React unmount did not release the generated subscription",
        );
    } finally {
      lifecycle.close();
      lifecycle.uniffiDestroy();
    }
    await agent.close();
    try {
      agent.setLogging(LogLevel.Off);
      throw new Error("A closed generated Agent accepted a logging update");
    } catch (error) {
      if (
        !AgentError.Operation.instanceOf(error) ||
        error.inner.info.code !== ErrorCode.Closed
      )
        throw error;
    }
    return (
      reception.state().closed && agent.state().status === AgentStatus.Closed
    );
  } finally {
    root.unmount();
    await reception?.close();
    await agent.close();
    reception?.uniffiDestroy();
    camera.uniffiDestroy();
    participant.uniffiDestroy();
    room.uniffiDestroy();
    agent.uniffiDestroy();
  }
}
