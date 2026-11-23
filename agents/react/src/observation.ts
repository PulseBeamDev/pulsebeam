import { useMemo, useSyncExternalStore } from "react";
import { useCommittedRef } from "./committed-ref.js";

export interface ObservableHandle {
  state(): object;
  updates(): {
    next(): Promise<object | undefined>;
    close(): void;
  };
}

export function useObservation(
  handle: ObservableHandle | null,
  onFailure: (handle: ObservableHandle, error: unknown) => void,
): void {
  const failure = useCommittedRef(onFailure);
  const store = useMemo(
    () => ({
      snapshot: () => handle?.state() ?? null,
      subscribe: (changed: () => void) => {
        if (!handle) return () => {};
        let cursor: ReturnType<ObservableHandle["updates"]>;
        try {
          cursor = handle.updates();
        } catch (error) {
          failure.current(handle, error);
          return () => {};
        }
        let active = true;
        void (async () => {
          while (active) {
            const state = await cursor.next();
            if (!active || state === undefined) return;
            changed();
          }
        })().catch((error: unknown) => {
          if (active) failure.current(handle, error);
        });
        return () => {
          active = false;
          try {
            cursor.close();
          } catch (error) {
            console.error("PulseBeam observation cleanup failed", error);
          }
        };
      },
    }),
    [handle, failure],
  );
  useSyncExternalStore(store.subscribe, store.snapshot, store.snapshot);
}
