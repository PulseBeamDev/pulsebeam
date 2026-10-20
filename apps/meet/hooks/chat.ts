import { useRef, useState } from "react";
import type { Agent, TopicEvent } from "@pulsebeam/react";

type LocalMessage = {
  type: "local";
  key: string;
  participantId: string;
  value: string;
};

export function useChat(agent: Agent) {
  const { participantId } = agent;
  const Chat = agent.topic<string>("chat", { mode: "ordered" });
  const [messages, setMessages] = useState<
    (TopicEvent<string> | LocalMessage)[]
  >([]);
  const [error, setError] = useState<string | null>(null);
  const localKeys = useRef(0);

  return {
    Chat,
    messages,
    error,
    clearError: () => setError(null),
    onEvent(event: TopicEvent<string>) {
      if (event.type === "message" && typeof event.value !== "string") return;
      setMessages((current) => [...current, event]);
    },
    onError() {
      setError("Chat is unavailable. Rejoin to restore it.");
    },
    async send(text: string): Promise<boolean> {
      const value = text.trim();
      if (!value || !participantId) return false;
      try {
        await Chat.publish(value);
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
