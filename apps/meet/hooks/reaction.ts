import { useRef, useState } from "react";
import { track, type Agent, type DataEvent } from "@pulsebeam/react";
import { decodeText, encodeText } from "@/lib/data-codec";

const reaction = track.data("reactions", "lossy");

export const reactionEmojis = ["👍", "❤️", "😂", "😮", "👏", "🔥"];
type Reaction = { key: string; emoji: string };

export function useReaction(agent: Agent) {
  const Reactions = agent.track(reaction);
  const [reactions, setReactions] = useState<Reaction[]>([]);
  const localKeys = useRef(0);
  const add = (value: Reaction) =>
    setReactions((current) => [...current, value].slice(-20));

  return {
    Reactions,
    reactions,
    onEvent(event: DataEvent<"lossy">) {
      const emoji = decodeText(event.data);
      if (emoji !== null && reactionEmojis.includes(emoji)) {
        add({ key: `${event.trackId}:${event.sequence}`, emoji });
      }
    },
    dismiss(key: string) {
      setReactions((current) => current.filter((item) => item.key !== key));
    },
    send(emoji: string) {
      if (!agent.participantId || !reactionEmojis.includes(emoji)) return;
      const key = `local:${localKeys.current++}`;
      void Reactions.send(encodeText(emoji))
        .then(() => add({ key, emoji }))
        .catch(() => {});
    },
  };
}
