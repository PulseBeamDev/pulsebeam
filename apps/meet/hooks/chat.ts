import { useRef, useState } from "react";
import type { Agent, TopicConsumerError, TopicEvent } from "@pulsebeam/react";

type ChatPayload = { text: string };
type Message = ChatPayload & {
  type: "message";
  key: string;
  sender: string;
};
type ChatGap = {
  type: "gap";
  key: string;
  sender: string;
  expectedSequence: bigint;
  availableSequence: bigint;
};

export function useChat(agent: Agent) {
  const { participantId } = agent;
  const Chat = agent.topic<ChatPayload>("chat", { mode: "ordered" });
  const [messages, setMessages] = useState<(Message | ChatGap)[]>([]);
  const [error, setError] = useState<string | null>(null);
  const localKeys = useRef(0);

  return {
    Chat,
    messages,
    error,
    clearError: () => setError(null),
    onEvent(event: TopicEvent<ChatPayload>) {
      if (event.type === "gap") {
        setMessages((current) => [
          ...current,
          {
            type: "gap",
            key: `gap:${event.dataTrackId}:${event.expectedSequence}:${event.availableSequence}`,
            sender: event.participantId,
            expectedSequence: event.expectedSequence,
            availableSequence: event.availableSequence,
          },
        ]);
        return;
      }
      if (typeof event.value?.text !== "string") return;
      setMessages((current) => [
        ...current,
        {
          type: "message",
          key: `${event.dataTrackId}:${event.sequence}`,
          text: event.value.text,
          sender: event.participantId,
        },
      ]);
    },
    onError(failure: TopicConsumerError) {
      setError(
        failure.code === "decode-failed"
          ? "Invalid topic message."
          : failure.event.message,
      );
    },
    async send(text: string): Promise<boolean> {
      const value = text.trim();
      if (!value || !participantId) return false;
      try {
        await Chat.publish({ text: value });
        setMessages((current) => [
          ...current,
          {
            type: "message",
            key: `local:${localKeys.current++}`,
            text: value,
            sender: participantId,
          },
        ]);
        return true;
      } catch (reason) {
        setError(
          reason instanceof Error
            ? reason.message
            : "Message was not accepted. Retry sending.",
        );
        return false;
      }
    },
  };
}
