import { useRef, useState } from "react";
import type { Agent, TopicConsumerError, TopicEvent } from "@pulsebeam/react";

export const reactionEmojis = ["👍", "❤️", "😂", "😮", "👏", "🔥"];
type ChatPayload = { text: string };
type Message = ChatPayload & {
  type: "message";
  key: string;
  sender: string;
  pending?: true;
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

function appendUnique<T extends { key: string }>(
  items: T[],
  item: T,
  limit?: number,
): T[] {
  if (items.some(({ key }) => key === item.key)) return items;
  const next = [...items, item];
  return limit ? next.slice(-limit) : next;
}

export function useTopics(agent: Agent) {
  const { participantId } = agent;
  const Chat = agent.topic<ChatPayload>("chat", { mode: "ordered" });
  const Reactions = agent.topic<ReactionPayload>("reactions", {
    mode: "lossy",
  });
  const [messages, setMessages] = useState<(Message | ChatGap)[]>([]);
  const [reactions, setReactions] = useState<Reaction[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [subscriptionRevision, setSubscriptionRevision] = useState(0);
  const localKeys = useRef(0);
  const addReaction = (reaction: Reaction) =>
    setReactions((current) => appendUnique(current, reaction, 20));

  const receiveChat = (event: TopicEvent<ChatPayload>) => {
    let entry: Message | ChatGap;
    if (event.type === "gap") {
      entry = {
        type: "gap",
        key: `gap:${event.dataTrackId}:${event.expectedSequence}:${event.availableSequence}`,
        sender: event.participantId,
        expectedSequence: event.expectedSequence,
        availableSequence: event.availableSequence,
      };
    } else {
      if (typeof event.value?.text !== "string") return;
      entry = {
        type: "message",
        key: `${event.dataTrackId}:${event.sequence}`,
        text: event.value.text,
        sender: event.participantId,
      };
    }
    setMessages((current) => appendUnique(current, entry));
  };
  const receiveReaction = (event: TopicEvent<ReactionPayload>) => {
    if (
      event.type === "message" &&
      typeof event.value?.emoji === "string" &&
      reactionEmojis.includes(event.value.emoji)
    ) {
      addReaction({
        key: `${event.dataTrackId}:${event.sequence}`,
        emoji: event.value.emoji,
      });
    }
  };
  const onTopicError = (failure: TopicConsumerError) =>
    setError(
      failure.code === "decode-failed"
        ? "Invalid topic message. Retry chat."
        : failure.event.message,
    );

  return {
    Chat,
    Reactions,
    receiveChat,
    receiveReaction,
    onTopicError,
    subscriptionRevision,
    messages,
    reactions,
    dismissReaction: (key: string) =>
      setReactions((current) => current.filter((item) => item.key !== key)),
    error,
    clearError: () => setError(null),
    retrySubscriptions: () => {
      setError(null);
      setSubscriptionRevision((revision) => revision + 1);
    },
    async sendChat(text: string): Promise<boolean> {
      const value = text.trim();
      if (!value || !participantId) return false;
      const key = `local:${localKeys.current++}`;
      setMessages((current) => [
        ...current,
        {
          type: "message",
          key,
          text: value,
          sender: participantId,
          pending: true,
        },
      ]);
      try {
        await Chat.publish({ text: value });
        setMessages((current) =>
          current.map((message) =>
            message.type === "message" && message.key === key
              ? { ...message, pending: undefined }
              : message,
          ),
        );
        return true;
      } catch (reason) {
        setMessages((current) =>
          current.filter((message) => message.key !== key),
        );
        setError(
          reason instanceof Error
            ? reason.message
            : "Message was not accepted. Retry sending.",
        );
        return false;
      }
    },
    sendReaction(emoji: string) {
      if (!participantId || !reactionEmojis.includes(emoji)) return;
      const key = `local:${localKeys.current++}`;
      void Reactions.publish({ emoji })
        .then(() => addReaction({ key, emoji }))
        .catch(() => setError("Reaction could not be sent"));
    },
  };
}
