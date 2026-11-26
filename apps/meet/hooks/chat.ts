import { useRef, useState } from "react";
import {
  usePublish,
  type Agent,
  DataDelivery,
  DataEvent,
} from "@pulsebeam/react";
import { decodeText, encodeText } from "@/lib/data-codec";

type LocalMessage = {
  type: "local";
  key: string;
  participantId: string;
  value: string;
};
type RemoteMessage = {
  type: "message";
  publicationId: string;
  sequence: bigint;
  participantId: string;
  value: string;
};
export function useChat(agent: Agent) {
  const { participantId } = agent.state();
  const Chat = agent.data("chat");
  const publication = usePublish(Chat);
  const [messages, setMessages] = useState<(RemoteMessage | LocalMessage)[]>(
    [],
  );
  const [error, setError] = useState<string | null>(null);
  const localKeys = useRef(0);

  return {
    Chat,
    messages,
    error,
    clearError: () => setError(null),
    onEvent(event: DataEvent) {
      if (
        DataEvent.SourceFailed.instanceOf(event) &&
        event.inner.participantId !== participantId &&
        event.inner.delivery === DataDelivery.Reliable
      ) {
        setError(`Chat from ${event.inner.participantId} was interrupted.`);
        return;
      }
      if (
        !DataEvent.Message.instanceOf(event) ||
        event.inner.participantId === participantId ||
        event.inner.delivery !== DataDelivery.Reliable
      )
        return;
      const value = decodeText(event.inner.data);
      if (value === null) return;
      setMessages((current) => [
        ...current,
        {
          type: "message",
          publicationId: event.inner.publicationId,
          sequence: event.inner.sequence,
          participantId: event.inner.participantId,
          value,
        },
      ]);
    },
    onError() {
      setError("Chat is unavailable. Rejoin to restore it.");
    },
    async send(text: string): Promise<boolean> {
      const value = text.trim();
      if (!value || !participantId || !publication) return false;
      try {
        await publication.send(encodeText(value));
        setMessages((current) => [
          ...current,
          {
            type: "local",
            key: `local:${localKeys.current++}`,
            participantId,
            value,
          },
        ]);
        return true;
      } catch {
        setError("Message could not be sent.");
        return false;
      }
    },
  };
}
