import { createPrivateKey, sign } from "node:crypto";

const alphabet = "0123456789ABCDEFGHJKMNPQRSTVWXYZ";
const maximumExpiration = (1n << 64n) - 1n;

function invalid(field: string): never {
  throw new TypeError(`Invalid ${field}`);
}

function payload(value: unknown, prefix: string, length: number): string {
  if (
    typeof value !== "string" ||
    value.length !== prefix.length + 2 + length ||
    !value.startsWith(`${prefix}_`) ||
    !"0Oo".includes(value[prefix.length + 1])
  ) {
    invalid(prefix === "sk" ? "signing secret" : "credential ID");
  }
  return value.slice(prefix.length + 2);
}

function decode(text: string): bigint {
  let n = 0n;
  for (const character of text) {
    if (character.charCodeAt(0) > 127) invalid("credential encoding");
    const c = character.toUpperCase();
    const digit =
      c === "O" ? 0 : c === "I" || c === "L" ? 1 : alphabet.indexOf(c);
    if (digit < 0) invalid("credential encoding");
    n = (n << 5n) | BigInt(digit);
  }
  return n;
}

function bytes(n: bigint, length: number): Buffer {
  return Buffer.from(n.toString(16).padStart(length * 2, "0"), "hex");
}

function canonicalId(value: unknown, prefix: string): string {
  const n = decode(payload(value, prefix, 26));
  if ((n & 3n) !== 0n) invalid("credential ID padding");
  const uuid = bytes(n >> 2n, 16);
  if (uuid[6] >> 4 !== 7 || (uuid[8] & 0xc0) !== 0x80)
    invalid("credential UUID");
  let canonical = "";
  let remaining = n;
  for (let i = 0; i < 26; i++) {
    canonical = alphabet[Number(remaining & 31n)] + canonical;
    remaining >>= 5n;
  }
  return `${prefix}_0${canonical}`;
}

function externalId(value: unknown, field: string): string {
  if (
    typeof value !== "string" ||
    value.length < 1 ||
    value.length > 36 ||
    /[^A-Za-z0-9_-]/.test(value)
  )
    invalid(field);
  return value;
}

/** Sign a server-only participant JWT. Credentials are always explicit. */
export function signParticipantToken({
  projectId,
  keyId,
  secret,
  room,
  participant,
  expiration,
}: {
  projectId: string;
  keyId: string;
  secret: string;
  room: string;
  participant: string;
  /** Absolute Unix seconds. Use bigint beyond Number.MAX_SAFE_INTEGER. */
  expiration: bigint | number;
}): string {
  const project = canonicalId(projectId, "p");
  const key = canonicalId(keyId, "kid");
  const seedNumber = decode(payload(secret, "sk", 52));
  if (seedNumber >= 1n << 256n) invalid("signing secret padding");
  const roomId = externalId(room, "room");
  const participantId = externalId(participant, "participant");
  if (
    (typeof expiration !== "bigint" && typeof expiration !== "number") ||
    (typeof expiration === "number" && !Number.isSafeInteger(expiration))
  )
    invalid("expiration");
  const exp = BigInt(expiration);
  if (exp < 0n || exp > maximumExpiration) invalid("expiration");
  const header = JSON.stringify({ alg: "EdDSA", kid: key, typ: "pb+jwt" });
  const claims = `{"iss":"${project}","aud":"pb","sub":"${participantId}","room":"${roomId}","exp":${exp}}`;
  const signingInput = `${Buffer.from(header).toString("base64url")}.${Buffer.from(claims).toString("base64url")}`;
  // RFC 8410 PKCS#8 wrapper for a raw 32-byte Ed25519 seed.
  const privateKey = createPrivateKey({
    key: Buffer.concat([
      Buffer.from("302e020100300506032b657004220420", "hex"),
      bytes(seedNumber, 32),
    ]),
    format: "der",
    type: "pkcs8",
  });
  return `${signingInput}.${sign(null, Buffer.from(signingInput), privateKey).toString("base64url")}`;
}
