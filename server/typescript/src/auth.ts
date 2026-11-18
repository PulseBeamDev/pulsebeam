import { createPrivateKey, sign } from "node:crypto";
const maximumExpiration = (1n << 64n) - 1n;
const maximumTokenBytes = 8192;

export type Action = "join" | "publish" | "discover" | "subscribe";
export type TrackKind = "audio" | "video" | "data";
export interface Grant {
  rooms: readonly string[];
  allow: readonly Action[];
  participant?: string;
  kind?: TrackKind;
  label?: string;
}

function invalid(field: string): never {
  throw new TypeError(`Invalid ${field}`);
}

function scope(value: unknown): value is string {
  return (
    typeof value === "string" &&
    value.length > 0 &&
    Buffer.byteLength(value, "utf8") <= 256 &&
    !/\p{Cc}/u.test(value) &&
    Buffer.from(value, "utf8").toString("utf8") === value
  );
}

function checkedGrants(value: unknown): Grant[] {
  if (!Array.isArray(value) || value.length > 32) invalid("grants");
  let patterns = 0;
  return value.map((grant: unknown) => {
    if (typeof grant !== "object" || grant === null || Array.isArray(grant))
      invalid("grant");
    if (
      Object.keys(grant).some(
        (key) =>
          !["rooms", "allow", "participant", "kind", "label"].includes(key),
      )
    )
      invalid("grant field");
    const raw = grant as Record<string, unknown>;
    if (!Array.isArray(raw.rooms) || raw.rooms.length === 0) invalid("rooms");
    patterns += raw.rooms.length;
    if (patterns > 64) invalid("room pattern count");
    const rooms = raw.rooms.map((room: unknown) => {
      if (!scope(room)) invalid("room pattern");
      if (
        room.includes("*") &&
        room !== "*" &&
        !(room.endsWith("/*") && room.indexOf("*") === room.length - 1)
      )
        invalid("room pattern");
      return room;
    });
    if (
      !Array.isArray(raw.allow) ||
      raw.allow.length === 0 ||
      raw.allow.length > 4
    )
      invalid("allow");
    const allow = raw.allow.map((action: unknown) => {
      if (
        action !== "join" &&
        action !== "publish" &&
        action !== "discover" &&
        action !== "subscribe"
      )
        invalid("action");
      return action;
    });
    if (new Set(allow).size !== allow.length) invalid("duplicate action");
    const checked: Grant = { rooms, allow };
    if (Object.hasOwn(raw, "participant")) {
      if (!scope(raw.participant)) invalid("participant filter");
      checked.participant = raw.participant;
    }
    if (Object.hasOwn(raw, "kind")) {
      if (raw.kind !== "audio" && raw.kind !== "video" && raw.kind !== "data")
        invalid("kind");
      checked.kind = raw.kind;
    }
    if (Object.hasOwn(raw, "label")) {
      if (!scope(raw.label)) invalid("label");
      checked.label = raw.label;
    }
    if (
      allow.includes("join") &&
      (checked.participant !== undefined ||
        checked.kind !== undefined ||
        checked.label !== undefined)
    )
      invalid("join filters");
    if (allow.includes("publish") && checked.participant !== undefined)
      invalid("publish participant filter");
    return checked;
  });
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
  grants,
  participant,
  expiration,
}: {
  apiKey: string;
  grants: readonly Grant[];
  participant: string;
  /** Absolute Unix seconds. Use bigint beyond Number.MAX_SAFE_INTEGER. */
  expiration: bigint | number;
}): string {
  if (!scope(participant)) invalid("participant");
  const checked = checkedGrants(grants);
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
  });
  const claims = `${baseClaims.slice(0, -1)},"exp":${exp},"grants":${JSON.stringify(checked)}}`;
  const signingInput = `${Buffer.from(header).toString("base64url")}.${Buffer.from(claims).toString("base64url")}`;
  // Ed25519's 64-byte signature occupies 86 unpadded base64url characters.
  if (signingInput.length + 1 + 86 > maximumTokenBytes)
    invalid("participant token size");
  return `${signingInput}.${sign(null, Buffer.from(signingInput), privateKey).toString("base64url")}`;
}
