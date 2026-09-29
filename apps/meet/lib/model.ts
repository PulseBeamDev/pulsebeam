export function normalizeEndpoint(input: string): string | null {
  try {
    const url = new URL(input);
    if (!/^https?:$/.test(url.protocol) || url.search || url.hash) return null;
    const path = url.pathname.replace(/\/+$/, "");
    return `${url.origin}${path}`;
  } catch {
    return null;
  }
}
