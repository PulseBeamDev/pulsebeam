import { useEffect, useRef, useState } from "react";
import type { Agent, TopicConsumerError } from "@pulsebeam/react";

const emojis = new Set(["👍", "❤️", "😂", "😮", "👏", "🔥"]);
type Message = ChatPayload & { self: boolean; status: "pending" | "accepted" };
type Reaction = { id: string; emoji: string };
type ChatPayload = { sender: string; text: string; ts: number; id: string };
type ReactionPayload = Reaction & { sender: string; ts: number };

export function useTopics(agent: Agent, participantExternalId: string | null) {
  const Chat = agent.topic<ChatPayload>("chat", { mode: "reliable" });
  const Reactions = agent.topic<ReactionPayload>("reactions", {
    mode: "unreliable",
  });
  const [messages, setMessages] = useState<Message[]>([]);
  const [reactions, setReactions] = useState<Reaction[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [gap, setGap] = useState(false);
  const [subscriptionRevision, retrySubscriptions] = useState(0);
  const timers = useRef(new Map<string, ReturnType<typeof setTimeout>>());
  const addReaction = (reaction: Reaction) => {
    setReactions((old) =>
      old.some((item) => item.id === reaction.id)
        ? old
        : [...old, reaction].slice(-20),
    );
    if (!timers.current.has(reaction.id))
      timers.current.set(
        reaction.id,
        setTimeout(() => {
          timers.current.delete(reaction.id);
          setReactions((old) => old.filter((item) => item.id !== reaction.id));
        }, 3000),
      );
  };
  const receiveChat = (payload: ChatPayload) => {
    if (
      typeof payload?.sender !== "string" ||
      typeof payload.text !== "string" ||
      typeof payload.id !== "string" ||
      typeof payload.ts !== "number"
    )
      return;
    setMessages((old) =>
      old.some((message) => message.id === payload.id)
        ? old.map((message) =>
            message.id === payload.id
              ? { ...message, status: "accepted" }
              : message,
          )
        : [
            ...old,
            {
              ...payload,
              self: payload.sender === participantExternalId,
              status: "accepted",
            },
          ],
    );
  };
  const receiveReaction = (payload: ReactionPayload) => {
    if (
      typeof payload?.id === "string" &&
      typeof payload.emoji === "string" &&
      emojis.has(payload.emoji) &&
      typeof payload.sender === "string" &&
      typeof payload.ts === "number"
    )
      addReaction({ id: payload.id, emoji: payload.emoji });
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
    onTopicGap: () => setGap(true),
    subscriptionRevision,
    messages,
    reactions,
    error,
    gap,
    clearError: () => setError(null),
    clearGap: () => setGap(false),
    retrySubscriptions: () => {
      setError(null);
      retrySubscriptions((n) => n + 1);
    },
    async sendChat(text: string): Promise<boolean> {
      const value = text.trim();
      if (!value || !participantExternalId) return false;
      const payload = {
        id: crypto.randomUUID(),
        sender: participantExternalId,
        text: value,
        ts: Date.now(),
      };
      const { id } = payload;
      setMessages((old) => [
        ...old,
        { ...payload, self: true, status: "pending" },
      ]);
      try {
        await Chat.publish(payload);
        setMessages((old) =>
          old.map((message) =>
            message.id === id ? { ...message, status: "accepted" } : message,
          ),
        );
        return true;
      } catch (reason) {
        setMessages((old) => old.filter((message) => message.id !== id));
        setError(
          reason instanceof Error
            ? reason.message
            : "Message was not accepted. Retry sending.",
        );
        return false;
      }
    },
    sendReaction(emoji: string) {
      if (!participantExternalId || !emojis.has(emoji)) return;
      const id = crypto.randomUUID();
      void Reactions.publish({
        id,
        emoji,
        sender: participantExternalId,
        ts: Date.now(),
      })
        .then(() => addReaction({ id, emoji }))
        .catch(() => setError("Reaction could not be sent"));
    },
  };
}
