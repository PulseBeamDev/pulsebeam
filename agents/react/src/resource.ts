import { useLayoutEffect, useRef, useState } from "react";
import { useCommittedRef } from "./committed-ref.js";
import { useObservation, type ObservableHandle } from "./observation.js";

interface Resource extends ObservableHandle {
  close(): Promise<void>;
}
interface Lifetime<T> {
  readonly target: object;
  value?: T;
  active: boolean;
  failed: boolean;
}

export function useResource<T extends Resource>(
  target: object | null,
  create: () => T,
): { value: T | null; fail(error: unknown): void } {
  const committed = useCommittedRef(create);
  const context = useRef<Lifetime<T> | null>(null);
  const cleanup = useRef<Promise<void>>(Promise.resolve());
  const [owned, setOwned] = useState<Lifetime<T> | null>(null);
  const [failure, setFailure] = useState<{
    lifetime: Lifetime<T>;
    error: unknown;
  } | null>(null);
  const retire = (lifetime: Lifetime<T>) => {
    const value = lifetime.value;
    lifetime.value = undefined;
    let closing: Promise<void>;
    try {
      closing = value?.close() ?? Promise.resolve();
    } catch (error) {
      closing = Promise.reject(error);
    }
    cleanup.current = Promise.all([cleanup.current, closing])
      .then(() => {})
      .catch((error: unknown) => {
        console.error("PulseBeam resource cleanup failed", error);
      });
    return cleanup.current;
  };
  const fail = (lifetime: Lifetime<T>, error: unknown) => {
    if (context.current !== lifetime || !lifetime.active || lifetime.failed)
      return;
    lifetime.failed = true;
    void retire(lifetime).then(() => {
      if (!lifetime.active || context.current !== lifetime) return;
      setFailure({ lifetime, error });
    });
  };
  useLayoutEffect(() => {
    if (!target) {
      context.current = null;
      return;
    }
    const lifetime: Lifetime<T> = { target, active: true, failed: false };
    context.current = lifetime;
    void cleanup.current.then(() => {
      if (!lifetime.active) return;
      try {
        lifetime.value = committed.current();
        lifetime.value.state();
        setOwned(lifetime);
      } catch (error) {
        fail(lifetime, error);
      }
    });
    return () => {
      lifetime.active = false;
      void retire(lifetime);
    };
  }, [target]);
  const current =
    owned?.target === target && owned.active && !owned.failed ? owned : null;
  const value = current?.value ?? null;
  useObservation(value, (observed, error) => {
    const lifetime = context.current;
    if (lifetime?.value === observed) fail(lifetime, error);
  });
  if (
    failure?.lifetime === context.current &&
    failure.lifetime.target === target
  )
    throw failure.error;
  return {
    value,
    fail: (error: unknown) => {
      if (current) fail(current, error);
    },
  };
}
