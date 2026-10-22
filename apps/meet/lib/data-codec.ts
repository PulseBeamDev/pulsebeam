const encoder = new TextEncoder();
const decoder = new TextDecoder("utf-8", { fatal: true });

export function encodeText(value: string): Uint8Array {
  return encoder.encode(JSON.stringify(value));
}

export function decodeText(data: Uint8Array): string | null {
  try {
    const value: unknown = JSON.parse(decoder.decode(data));
    return typeof value === "string" ? value : null;
  } catch {
    return null;
  }
}
