import { useMemo, useSyncExternalStore } from "react";

export interface ObservableHandle {
  state(): object;
  updates(): {
    next(): Promise<object | undefined>;
    close(): void;
  };
}

export function useObservation(handle: ObservableHandle | null): void {
  const store = useMemo(() => {
    let failure: { error: unknown } | undefined;
    return {
      snapshot: () => {
        if (failure) throw failure.error;
        return handle?.state() ?? null;
      },
      subscribe: (changed: () => void) => {
        if (!handle) return () => {};
        const cursor = handle.updates();
        let active = true;
        void (async () => {
          while (active) {
            const state = await cursor.next();
            if (!active || state === undefined) return;
            changed();
          }
        })().catch((error: unknown) => {
          if (!active) return;
          failure = { error };
          changed();
        });
        return () => {
          active = false;
          cursor.close();
        };
      },
    };
  }, [handle]);
  useSyncExternalStore(store.subscribe, store.snapshot, store.snapshot);
}
