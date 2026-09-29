import { useEffect, useLayoutEffect, useRef } from "react";

const useCommitEffect =
  typeof window === "undefined" ? useEffect : useLayoutEffect;

export function useCommittedRef<T>(value: T) {
  const ref = useRef(value);
  useCommitEffect(() => {
    ref.current = value;
  }, [value]);
  return ref;
}
