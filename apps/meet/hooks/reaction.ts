import { useRef, useState } from "react";
import { usePublish, type Agent, type DataEvent } from "@pulsebeam/react";
import { decodeText, encodeText } from "@/lib/data-codec";

export const reactionEmojis = ["👍", "❤️", "😂", "😮", "👏", "🔥"];
type Reaction = { key: string; emoji: string };

export function useReaction(agent: Agent) {
  const Reactions = agent.data("reactions", "lossy");
  const publication = usePublish(Reactions);
  const [reactions, setReactions] = useState<Reaction[]>([]);
  const localKeys = useRef(0);
  const add = (value: Reaction) =>
    setReactions((current) => [...current, value].slice(-20));

  return {
    Reactions,
    reactions,
    onEvent(event: DataEvent<"lossy">) {
      if (event.type !== "message") return;
      const emoji = decodeText(event.data);
      if (emoji !== null && reactionEmojis.includes(emoji)) {
        add({ key: `${event.publicationId}:${event.sequence}`, emoji });
      }
    },
    dismiss(key: string) {
      setReactions((current) => current.filter((item) => item.key !== key));
    },
    send(emoji: string) {
      if (
        !agent.participantId ||
        !publication ||
        !reactionEmojis.includes(emoji)
      )
        return;
      const key = `local:${localKeys.current++}`;
      void publication
        .send(encodeText(emoji))
        .then(() => add({ key, emoji }))
        .catch(() => {});
    },
  };
}
