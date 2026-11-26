import {
  AgentError,
  ErrorCode,
  VideoCodec,
  VideoSource,
  type Agent,
  type AudioSource,
  type Pipe,
  type VideoPublication,
} from "@pulsebeam/web";

export async function publishWithSwap(
  agent: Agent,
  firstTrack: MediaStreamTrack,
  secondTrack: MediaStreamTrack,
  observe: (
    publication: VideoPublication,
    input: "first" | "second",
    pipe: Pipe,
  ) => Promise<void>,
) {
  const publication = agent
    .video("camera")
    .publish({ enabled: true, codec: VideoCodec.H264 });
  try {
    const first = VideoSource.fromTrack(firstTrack);
    try {
      const pipe = first.pipeTo(publication);
      try {
        await observe(publication, "first", pipe);
      } finally {
        await pipe.close();
      }
    } finally {
      await first.close();
    }

    const replacement = VideoSource.fromTrack(secondTrack);
    try {
      const pipe = replacement.pipeTo(publication);
      try {
        await observe(publication, "second", pipe);
      } finally {
        await pipe.close();
      }
    } finally {
      await replacement.close();
    }
  } finally {
    await publication.close();
  }
}

export function attachWrongKind(
  source: AudioSource,
  publication: VideoPublication,
) {
  // @ts-expect-error An audio source cannot attach to a video publication.
  return source.pipeTo(publication);
}

export async function rejectCodecChange(publication: VideoPublication) {
  const before = publication.state();
  const mimeType = before.admission.effectiveFormat?.mimeType;
  if (!mimeType) throw new Error("expected an admitted video format");
  let rejected = false;
  try {
    await publication.configure({ enabled: false, codec: VideoCodec.Av1 });
  } catch (error) {
    if (
      !AgentError.Operation.instanceOf(error) ||
      error.inner.info.code !== ErrorCode.Unsupported
    ) {
      throw error;
    }
    rejected = true;
  }
  if (!rejected) throw new Error("changed immutable codec");
  const after = publication.state();
  if (
    after.admission.publicationId !== before.admission.publicationId ||
    after.admission.effectiveFormat?.mimeType !== mimeType ||
    after.configuration.acceptedEnabled !== before.configuration.acceptedEnabled
  ) {
    throw new Error("rejected update changed the effective publication");
  }
}
