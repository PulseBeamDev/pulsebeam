import { useEffect, useMemo, useRef, useState } from "react";
import type { Agent, Topic } from "@pulsebeam/react";

const emojis = new Set(["👍", "❤️", "😂", "😮", "👏", "🔥"]);
type Message = { id: string; sender: string; text: string; self: boolean };
type Reaction = { id: string; emoji: string };
type ChatPayload = { sender: string; text: string; ts: number; id: string };
type ReactionPayload = {
  id: string;
  emoji: string;
  sender: string;
  ts: number;
};

export function useTopics(agent: Agent, participantId: string | null) {
  const chat = useMemo(
    () => agent.topic<ChatPayload>("chat", { mode: "reliable" }),
    [agent],
  );
  const reactionsTopic = useMemo(
    () => agent.topic<ReactionPayload>("reactions", { mode: "unreliable" }),
    [agent],
  );
  const [messages, setMessages] = useState<Message[]>([]);
  const [reactions, setReactions] = useState<Reaction[]>([]);
  const [error, setError] = useState<string | null>(null);
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
  useEffect(() => {
    const controller = new AbortController();
    const receiveChat = async (topic: Topic<ChatPayload>) => {
      try {
        for await (const payload of topic.subscribe({
          signal: controller.signal,
        })) {
          if (
            typeof payload?.sender !== "string" ||
            typeof payload.text !== "string" ||
            typeof payload.id !== "string" ||
            typeof payload.ts !== "number"
          )
            continue;
          setMessages((old) =>
            old.some((message) => message.id === payload.id)
              ? old
              : [
                  ...old,
                  {
                    id: payload.id,
                    sender: payload.sender,
                    text: payload.text,
                    self: false,
                  },
                ],
          );
        }
      } catch (reason) {
        if (!controller.signal.aborted)
          setError(
            reason instanceof Error
              ? reason.message
              : "Chat subscription failed",
          );
      }
    };
    const receiveReactions = async (topic: Topic<ReactionPayload>) => {
      try {
        for await (const payload of topic.subscribe({
          signal: controller.signal,
        })) {
          if (
            typeof payload?.id === "string" &&
            typeof payload.emoji === "string" &&
            emojis.has(payload.emoji) &&
            typeof payload.sender === "string" &&
            typeof payload.ts === "number"
          )
            addReaction({ id: payload.id, emoji: payload.emoji });
        }
      } catch (reason) {
        if (!controller.signal.aborted)
          setError(
            reason instanceof Error
              ? reason.message
              : "Reaction subscription failed",
          );
      }
    };
    void receiveChat(chat);
    void receiveReactions(reactionsTopic);
    const activeTimers = timers.current;
    return () => {
      controller.abort();
      for (const timer of activeTimers.values()) clearTimeout(timer);
      activeTimers.clear();
    };
  }, [chat, reactionsTopic]);

  const publish = (operation: Promise<void>, accepted: () => void) => {
    void operation
      .then(accepted)
      .catch((reason: unknown) =>
        setError(
          reason instanceof Error ? reason.message : "Unable to send message",
        ),
      );
  };
  return {
    messages,
    reactions,
    error,
    clearError: () => setError(null),
    sendChat(text: string) {
      const value = text.trim();
      if (!value || !participantId) return;
      const id = crypto.randomUUID();
      publish(
        chat.publish({
          id,
          sender: participantId,
          text: value,
          ts: Date.now(),
        }),
        () =>
          setMessages((old) => [
            ...old,
            { id, sender: participantId, text: value, self: true },
          ]),
      );
    },
    sendReaction(emoji: string) {
      if (!participantId || !emojis.has(emoji)) return;
      const id = crypto.randomUUID();
      publish(
        reactionsTopic.publish({
          id,
          emoji,
          sender: participantId,
          ts: Date.now(),
        }),
        () => addReaction({ id, emoji }),
      );
    },
  };
}
