import { useRef, useState } from "react";
import type { Agent, TopicEvent } from "@pulsebeam/react";

export const reactionEmojis = ["👍", "❤️", "😂", "😮", "👏", "🔥"];
type Reaction = { key: string; emoji: string };

export function useReaction(agent: Agent) {
  const Reactions = agent.topic<string>("reactions", {
    mode: "lossy",
  });
  const [reactions, setReactions] = useState<Reaction[]>([]);
  const localKeys = useRef(0);
  const add = (reaction: Reaction) =>
    setReactions((current) => [...current, reaction].slice(-20));

  return {
    Reactions,
    reactions,
    onEvent(event: TopicEvent<string>) {
      if (
        event.type === "message" &&
        typeof event.value === "string" &&
        reactionEmojis.includes(event.value)
      ) {
        add({
          key: `${event.dataTrackId}:${event.sequence}`,
          emoji: event.value,
        });
      }
    },
    dismiss(key: string) {
      setReactions((current) => current.filter((item) => item.key !== key));
    },
    send(emoji: string) {
      if (!agent.participantId || !reactionEmojis.includes(emoji)) return;
      const key = `local:${localKeys.current++}`;
      void Reactions.publish(emoji)
        .then(() => add({ key, emoji }))
        .catch(() => {});
    },
  };
}
