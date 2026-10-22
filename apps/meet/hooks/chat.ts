import { useRef, useState } from "react";
import { track, type Agent, type DataEvent } from "@pulsebeam/react";
import { decodeText, encodeText } from "@/lib/data-codec";

const chat = track.data("chat", "ordered");

type LocalMessage = {
  type: "local";
  key: string;
  participantId: string;
  value: string;
};
type RemoteMessage = {
  type: "message";
  dataTrackId: string;
  sequence: bigint;
  participantId: string;
  value: string;
};
type Gap = {
  type: "gap";
  dataTrackId: string;
  participantId: string;
  expectedSequence: bigint;
  availableSequence: bigint;
};

export function useChat(agent: Agent) {
  const { participantId } = agent;
  const Chat = agent.track(chat);
  const [messages, setMessages] = useState<
    (RemoteMessage | Gap | LocalMessage)[]
  >([]);
  const [error, setError] = useState<string | null>(null);
  const localKeys = useRef(0);

  return {
    Chat,
    messages,
    error,
    clearError: () => setError(null),
    onEvent(event: DataEvent<"ordered">) {
      if (event.type === "gap") {
        setMessages((current) => [
          ...current,
          {
            type: "gap",
            dataTrackId: event.trackId,
            participantId: event.participantId,
            expectedSequence: event.from,
            availableSequence: event.to,
          },
        ]);
        return;
      }
      const value = decodeText(event.data);
      if (value === null) return;
      setMessages((current) => [
        ...current,
        {
          type: "message",
          dataTrackId: event.trackId,
          sequence: event.sequence,
          participantId: event.participantId,
          value,
        },
      ]);
    },
    onError() {
      setError("Chat is unavailable. Rejoin to restore it.");
    },
    async send(text: string): Promise<boolean> {
      const value = text.trim();
      if (!value || !participantId) return false;
      try {
        await Chat.send(encodeText(value));
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
