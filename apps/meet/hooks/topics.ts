import { useEffect, useRef, useState } from "react";
import { useAgent } from "@pulsebeam/react";

const encoder = new TextEncoder();
const decoder = new TextDecoder();
const emojis = new Set(["👍", "❤️", "😂", "😮", "👏", "🔥"]);
type Message = { id: string; sender: string; text: string; self: boolean };
type Reaction = { id: string; emoji: string };
type Pending = { topic: "chat" | "reactions"; id: string; sender: string; text?: string; emoji?: string };

export function useTopics() {
  const { subscribeEvents, sendTopic, participantId } = useAgent();
  const [messages, setMessages] = useState<Message[]>([]);
  const [reactions, setReactions] = useState<Reaction[]>([]);
  const [error, setError] = useState<string | null>(null);
  const pending = useRef<Pending[]>([]);
  const timers = useRef(new Map<string, ReturnType<typeof setTimeout>>());
  const addReaction = (reaction: Reaction) => {
    setReactions((old) => old.some((item) => item.id === reaction.id) ? old : [...old, reaction].slice(-20));
    if (!timers.current.has(reaction.id)) timers.current.set(reaction.id, setTimeout(() => {
      timers.current.delete(reaction.id);
      setReactions((old) => old.filter((item) => item.id !== reaction.id));
    }, 3000));
  };
  useEffect(() => {
    const unsubscribe = subscribeEvents((event) => {
      if (event.type === "topic-send-dropped" || event.type === "topic-channel-failed") {
        setError(event.type === "topic-send-dropped" ? `Unable to send ${event.topic}: ${event.reason}` : event.message);
        return;
      }
      if (event.type === "topic-send-admitted") {
        const index = pending.current.findIndex((item) => item.topic === event.topic);
        const local = index < 0 ? undefined : pending.current.splice(index, 1)[0];
        const localText = local?.text;
        if (local?.topic === "chat" && typeof localText === "string") setMessages((old) => [...old, { id: local.id, sender: local.sender, text: localText, self: true }]);
        if (local?.topic === "reactions" && local.emoji) addReaction({ id: local.id, emoji: local.emoji });
        return;
      }
      if (event.type !== "topic-message") return;
      try {
        const decoded: unknown = JSON.parse(decoder.decode(event.payload));
        if (!decoded || typeof decoded !== "object") return;
        const payload = decoded as Record<string, unknown>;
        const remoteSender = payload.sender; const remoteText = payload.text; const timestamp = payload.ts;
        if (event.topic === "chat" && event.mode === "ordered" && typeof remoteSender === "string" && typeof remoteText === "string" && typeof timestamp === "number") {
          const id = `${event.publisherId}-${event.streamId}-${event.sequence}`;
          setMessages((old) => old.some((message) => message.id === id) ? old : [...old, { id, sender: remoteSender, text: remoteText, self: false }]);
        }
        if (event.topic === "reactions" && typeof payload.id === "string" && typeof payload.emoji === "string" && emojis.has(payload.emoji) && typeof payload.sender === "string" && typeof payload.ts === "number") addReaction({ id: payload.id, emoji: payload.emoji });
      } catch { /* ignore malformed remote data */ }
    });
    return () => { unsubscribe(); pending.current = []; for (const timer of timers.current.values()) clearTimeout(timer); timers.current.clear(); };
  }, [subscribeEvents]);
  const enqueue = (item: Pending, payload: object, mode: "ordered" | "latest") => {
    pending.current.push(item);
    try { sendTopic(item.topic, mode, encoder.encode(JSON.stringify(payload))); }
    catch (reason) { pending.current = pending.current.filter((entry) => entry.id !== item.id); setError(reason instanceof Error ? reason.message : "Unable to send message"); }
  };
  return {
    participantId, messages, reactions, error, clearError: () => setError(null),
    sendChat(text: string) { const value = text.trim(); if (!value || !participantId) return; const ts = Date.now(); enqueue({ topic: "chat", id: `self-${ts}`, sender: participantId, text: value }, { sender: participantId, text: value, ts }, "ordered"); },
    sendReaction(emoji: string) { if (!participantId || !emojis.has(emoji)) return; const ts = Date.now(); const id = `${participantId}-${ts}`; enqueue({ topic: "reactions", id, sender: participantId, emoji }, { id, emoji, sender: participantId, ts }, "latest"); },
  };
}
