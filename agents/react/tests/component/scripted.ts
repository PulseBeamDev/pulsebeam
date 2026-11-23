import {
  RemoteDataTrack,
  type DataEvent,
  type DataSubscription,
} from "@pulsebeam/web";
import { deferred } from "./platform.js";

export class ScriptedReads<T> {
  private pending?: ReturnType<typeof deferred<T | undefined>>;
  private queued: { value: T }[] = [];
  private closed = false;

  next = (): Promise<T | undefined> => {
    if (this.closed) return Promise.resolve(undefined);
    const item = this.queued.shift();
    if (item) return Promise.resolve(item.value);
    if (this.pending) throw new Error("test reader already has a pending read");
    this.pending = deferred<T | undefined>();
    return this.pending.promise;
  };

  push(value: T) {
    if (this.closed) return;
    const pending = this.pending;
    if (!pending) this.queued.push({ value });
    else {
      this.pending = undefined;
      pending.resolve(value);
    }
  }

  fail(error: unknown) {
    if (!this.pending)
      throw new Error("test reader has no pending read to fail");
    const pending = this.pending;
    this.pending = undefined;
    pending.reject(error);
  }

  close = () => {
    this.closed = true;
    this.queued.length = 0;
    this.pending?.resolve(undefined);
    this.pending = undefined;
  };
}

export function captureDataReads() {
  const subscriptions = new Map<DataSubscription, ScriptedReads<DataEvent>>();
  const subscribe = RemoteDataTrack.prototype.subscribe;
  RemoteDataTrack.prototype.subscribe = function (...args) {
    const owner = subscribe.call(this, ...args);
    const reads = new ScriptedReads<DataEvent>();
    owner.next = reads.next;
    const close = owner.close.bind(owner);
    owner.close = async (...args) => {
      reads.close();
      await close(...args);
    };
    subscriptions.set(owner, reads);
    return owner;
  };
  const owners = () =>
    [...subscriptions].filter(([owner]) => !owner.state().closed);
  return {
    owners,
    deliver(trackId: string, event: DataEvent) {
      for (const [owner, reads] of owners())
        if (owner.state().selector.trackId === trackId) reads.push(event);
    },
    restore: () => {
      RemoteDataTrack.prototype.subscribe = subscribe;
    },
  };
}

export function scriptedState<T extends { observationRevision: bigint }>(
  initial: T,
) {
  let current = initial;
  const readers = new Set<ScriptedReads<T>>();
  return {
    state: () => current,
    change(patch: Partial<T>) {
      current = Object.freeze({
        ...current,
        ...patch,
        observationRevision: current.observationRevision + 1n,
      });
      for (const reader of readers) reader.push(current);
    },
    updates() {
      const reader = new ScriptedReads<T>();
      reader.push(current);
      readers.add(reader);
      return {
        next: reader.next,
        close() {
          readers.delete(reader);
          reader.close();
        },
      };
    },
  };
}
