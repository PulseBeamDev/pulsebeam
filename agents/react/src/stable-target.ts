import { useState } from "react";

export function useStableTarget<T>(
  current: T,
  equivalent: (previous: T, current: T) => boolean,
): T {
  const [previous, setPrevious] = useState(current);
  if (previous === current || equivalent(previous, current)) return previous;
  setPrevious(current);
  return current;
}
