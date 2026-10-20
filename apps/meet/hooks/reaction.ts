import { useRef, useState } from "react";
import type { Agent, TopicConsumerError, TopicEvent } from "@pulsebeam/react";

export const reactionEmojis = ["👍", "❤️", "😂", "😮", "👏", "🔥"];
type ReactionPayload = { emoji: string };
type Reaction = ReactionPayload & { key: string };

export function useReaction(agent: Agent) {
  const Reactions = agent.topic<ReactionPayload>("reactions", {
    mode: "lossy",
  });
  const [reactions, setReactions] = useState<Reaction[]>([]);
  const [error, setError] = useState<string | null>(null);
  const localKeys = useRef(0);
  const add = (reaction: Reaction) =>
    setReactions((current) => [...current, reaction].slice(-20));

  return {
    Reactions,
    reactions,
    error,
    clearError: () => setError(null),
    onEvent(event: TopicEvent<ReactionPayload>) {
      if (
        event.type === "message" &&
        typeof event.value?.emoji === "string" &&
        reactionEmojis.includes(event.value.emoji)
      ) {
        add({
          key: `${event.dataTrackId}:${event.sequence}`,
          emoji: event.value.emoji,
        });
      }
    },
    onError(failure: TopicConsumerError) {
      setError(
        failure.code === "decode-failed"
          ? "Invalid topic message."
          : failure.event.message,
      );
    },
    dismiss(key: string) {
      setReactions((current) => current.filter((item) => item.key !== key));
    },
    send(emoji: string) {
      if (!agent.participantId || !reactionEmojis.includes(emoji)) return;
      const key = `local:${localKeys.current++}`;
      void Reactions.publish({ emoji })
        .then(() => add({ key, emoji }))
        .catch(() => setError("Reaction could not be sent"));
    },
  };
}
