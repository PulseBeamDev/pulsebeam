import { Agent, AgentError, AgentStatus, ErrorCode } from "@pulsebeam/web";
import { createElement } from "react";
import { createRoot } from "react-dom/client";

export async function receiveGenerated(config: {
  endpoint: string;
  token: string;
  room: string;
  sourceRoom: string;
}): Promise<boolean> {
  const agent = new Agent({ endpoint: config.endpoint, token: config.token });
  if (!Agent.instanceOf(agent)) throw new Error("expected a generated Agent");
  const state = agent.state();
  if (state.status !== AgentStatus.Idle || agent.state() !== state) {
    throw new Error("Packed Agent is not synchronously inert");
  }
  const room = agent.room(config.sourceRoom);
  const participant = room.participant("native-publisher");
  const camera = participant.video("camera");
  const reception = camera.subscribe();
  const container = document.getElementById("root");
  if (!container) throw new Error("missing React fixture root");
  const root = createRoot(container);
  try {
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
    agent.join(config.room);
    const output = await reception.playbackOutput();
    try {
      const track = output.track();
      if (!track) throw new Error("missing generated video track");
      const stream = new MediaStream([track]);
      await new Promise<void>((resolve, reject) => {
        root.render(
          createElement("video", {
            muted: true,
            autoPlay: true,
            playsInline: true,
            ref(element: HTMLVideoElement | null) {
              if (element) element.srcObject = stream;
            },
            onError: () => reject(new Error("generated video playback failed")),
            onLoadedData(event: { currentTarget: HTMLVideoElement }) {
              try {
                const video = event.currentTarget;
                const canvas = document.createElement("canvas");
                canvas.width = video.videoWidth;
                canvas.height = video.videoHeight;
                const context = canvas.getContext("2d");
                if (!context || canvas.width !== 160 || canvas.height !== 96) {
                  throw new Error("missing expected decoded video geometry");
                }
                context.drawImage(video, 0, 0);
                const pixel = context.getImageData(80, 48, 1, 1).data;
                if (pixel[0] < 150 || pixel[1] < 50 || pixel[3] !== 255) {
                  throw new Error(
                    "generated output did not decode native pixels",
                  );
                }
                resolve();
              } catch (error) {
                reject(error);
              }
            },
          }),
        );
      });
      await output.close();
      if (track.readyState !== "ended" || reception.state().closed)
        return false;
    } finally {
      await output.close();
      output.uniffiDestroy();
    }
    await reception.close();
    await agent.close();
    return (
      reception.state().closed && agent.state().status === AgentStatus.Closed
    );
  } finally {
    root.unmount();
    await reception.close();
    await agent.close();
    reception.uniffiDestroy();
    camera.uniffiDestroy();
    participant.uniffiDestroy();
    room.uniffiDestroy();
    agent.uniffiDestroy();
  }
}
