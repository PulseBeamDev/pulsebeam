import { useEffect, useRef, useState } from "react";
import type { Agent, TopicConsumerError, TopicEvent } from "@pulsebeam/react";

const emojis = new Set(["👍", "❤️", "😂", "😮", "👏", "🔥"]);
type ChatPayload = { text: string; ts: number };
type Message = ChatPayload & {
  type: "message";
  key: string;
  sender: string;
  self: boolean;
  status: "pending" | "accepted";
};
type ChatGap = {
  type: "gap";
  key: string;
  sender: string;
  expectedSequence: bigint;
  availableSequence: bigint;
};
type ReactionPayload = { emoji: string };
type Reaction = ReactionPayload & { key: string };

export function useTopics(agent: Agent, participantId: string | null) {
  const Chat = agent.topic<ChatPayload>("chat", { mode: "ordered" });
  const Reactions = agent.topic<ReactionPayload>("reactions", {
    mode: "lossy",
  });
  const [messages, setMessages] = useState<(Message | ChatGap)[]>([]);
  const [reactions, setReactions] = useState<Reaction[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [subscriptionRevision, retrySubscriptions] = useState(0);
  const localKeys = useRef(0);
  const timers = useRef(new Map<string, ReturnType<typeof setTimeout>>());
  const addReaction = (reaction: Reaction) => {
    setReactions((old) =>
      old.some((item) => item.key === reaction.key)
        ? old
        : [...old, reaction].slice(-20),
    );
    if (!timers.current.has(reaction.key))
      timers.current.set(
        reaction.key,
        setTimeout(() => {
          timers.current.delete(reaction.key);
          setReactions((old) =>
            old.filter((item) => item.key !== reaction.key),
          );
        }, 3000),
      );
  };
  const receiveChat = (event: TopicEvent<ChatPayload>) => {
    if (event.type === "gap") {
      const key = `gap:${event.dataTrackId}:${event.expectedSequence}:${event.availableSequence}`;
      setMessages((old) =>
        old.some((entry) => entry.key === key)
          ? old
          : [
              ...old,
              {
                type: "gap",
                key,
                sender: event.participantId,
                expectedSequence: event.expectedSequence,
                availableSequence: event.availableSequence,
              },
            ],
      );
      return;
    }
    const payload = event.value;
    if (typeof payload?.text !== "string" || typeof payload.ts !== "number")
      return;
    const key = `${event.dataTrackId}:${event.sequence}`;
    setMessages((old) =>
      old.some((message) => message.key === key)
        ? old
        : [
            ...old,
            {
              type: "message",
              key,
              text: payload.text,
              ts: payload.ts,
              sender: event.participantId,
              self: event.participantId === participantId,
              status: "accepted",
            },
          ],
    );
  };
  const receiveReaction = (event: TopicEvent<ReactionPayload>) => {
    if (event.type !== "message") return;
    const payload = event.value;
    if (typeof payload?.emoji === "string" && emojis.has(payload.emoji))
      addReaction({
        key: `${event.dataTrackId}:${event.sequence}`,
        emoji: payload.emoji,
      });
  };
  const onError = (error: TopicConsumerError) =>
    setError(
      error.code === "decode-failed"
        ? "Invalid topic message. Retry chat."
        : error.event.message,
    );
  useEffect(() => {
    const activeTimers = timers.current;
    return () => {
      for (const timer of activeTimers.values()) clearTimeout(timer);
      activeTimers.clear();
    };
  }, [subscriptionRevision]);

  return {
    Chat,
    Reactions,
    receiveChat,
    receiveReaction,
    onTopicError: onError,
    subscriptionRevision,
    messages,
    reactions,
    error,
    clearError: () => setError(null),
    retrySubscriptions: () => {
      setError(null);
      retrySubscriptions((n) => n + 1);
    },
    async sendChat(text: string): Promise<boolean> {
      const value = text.trim();
      if (!value || !participantId) return false;
      const payload = { text: value, ts: Date.now() };
      const key = `local:${localKeys.current++}`;
      setMessages((old) => [
        ...old,
        {
          ...payload,
          type: "message",
          key,
          sender: participantId,
          self: true,
          status: "pending",
        },
      ]);
      try {
        await Chat.publish(payload);
        setMessages((old) =>
          old.map((message) =>
            message.type === "message" && message.key === key
              ? { ...message, status: "accepted" }
              : message,
          ),
        );
        return true;
      } catch (reason) {
        setMessages((old) => old.filter((message) => message.key !== key));
        setError(
          reason instanceof Error
            ? reason.message
            : "Message was not accepted. Retry sending.",
        );
        return false;
      }
    },
    sendReaction(emoji: string) {
      if (!participantId || !emojis.has(emoji)) return;
      const key = `local:${localKeys.current++}`;
      void Reactions.publish({ emoji })
        .then(() => addReaction({ key, emoji }))
        .catch(() => setError("Reaction could not be sent"));
    },
  };
}
