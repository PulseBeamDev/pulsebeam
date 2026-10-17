import { createPrivateKey, sign } from "node:crypto";
const maximumExpiration = (1n << 64n) - 1n;

function invalid(field: string): never {
  throw new TypeError(`Invalid ${field}`);
}

function parseApiKey(value: unknown): {
  project: string;
  key: string;
  privateKey: ReturnType<typeof createPrivateKey>;
} {
  if (typeof value !== "string") invalid("API key");
  const parts = value.split(".");
  if (parts.length !== 3) invalid("API key");
  const [project, key, encodedPrivateKey] = parts;
  try {
    const privateKey = createPrivateKey({
      key: Buffer.from(encodedPrivateKey, "base64url"),
      format: "der",
      type: "pkcs8",
    });
    if (privateKey.asymmetricKeyType !== "ed25519") invalid("API key");
    return { project, key, privateKey };
  } catch {
    invalid("API key");
  }
}

/** Sign a participant JWT using one opaque server-only API key. */
export function signParticipantToken({
  apiKey,
  room,
  participant,
  expiration,
}: {
  apiKey: string;
  room: string;
  participant: string;
  /** Absolute Unix seconds. Use bigint beyond Number.MAX_SAFE_INTEGER. */
  expiration: bigint | number;
}): string {
  const { project, key, privateKey } = parseApiKey(apiKey);
  if (
    (typeof expiration !== "bigint" && typeof expiration !== "number") ||
    (typeof expiration === "number" && !Number.isSafeInteger(expiration))
  )
    invalid("expiration");
  const exp = BigInt(expiration);
  if (exp < 0n || exp > maximumExpiration) invalid("expiration");
  const header = JSON.stringify({ alg: "EdDSA", kid: key, typ: "pb+jwt" });
  const baseClaims = JSON.stringify({
    iss: project,
    aud: "pb",
    sub: participant,
    room,
  });
  const claims = `${baseClaims.slice(0, -1)},"exp":${exp}}`;
  const signingInput = `${Buffer.from(header).toString("base64url")}.${Buffer.from(claims).toString("base64url")}`;
  return `${signingInput}.${sign(null, Buffer.from(signingInput), privateKey).toString("base64url")}`;
}
