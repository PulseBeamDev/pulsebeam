import assert from "node:assert/strict";
import { createPublicKey, verify } from "node:crypto";
import {
  cpSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { inspect } from "node:util";
import ts from "typescript";

// Exercise the publishable tarball, isolated from source and private imports.
const [artifact, fixtures, developmentFixture] = process.argv
  .slice(2)
  .map((p) => resolve(p));
const directory = mkdtempSync(
  join(process.env.TEST_TMPDIR || tmpdir(), "server-consumer-"),
);
const installed = join(directory, "node_modules/@pulsebeam/server");
mkdirSync(installed, { recursive: true });
cpSync(artifact, installed, { recursive: true, dereference: true });
writeFileSync(join(directory, "package.json"), '{"type":"module"}');
writeFileSync(
  join(directory, "consumer.mjs"),
  'export { signParticipantToken, INSECURE_DEV_API_KEY } from "@pulsebeam/server";',
);
const { signParticipantToken, INSECURE_DEV_API_KEY } = await import(
  pathToFileURL(join(directory, "consumer.mjs"))
);
const development = JSON.parse(readFileSync(developmentFixture));
assert.equal(INSECURE_DEV_API_KEY, development.apiKey);
const devToken = signParticipantToken({
  apiKey: INSECURE_DEV_API_KEY,
  room: "local",
  participant: "test",
  expiration: 2000000000,
});
assert.equal(
  JSON.parse(Buffer.from(devToken.split(".")[1], "base64url")).iss,
  development.projectId,
);

const publicKey = createPublicKey({
  key: Buffer.from(development.publicKey, "base64url"),
  format: "der",
  type: "spki",
});
assert.equal(publicKey.asymmetricKeyType, "ed25519");
const devParts = devToken.split(".");
assert(
  verify(
    null,
    Buffer.from(devParts.slice(0, 2).join(".")),
    publicKey,
    Buffer.from(devParts[2], "base64url"),
  ),
);

const document = JSON.parse(readFileSync(fixtures));
const fixtureApiKey =
  "p_0067MYZ1001R010000000000064.kid_0067MYZ1005R030010000000068.MC4CAQAwBQYDK2VwBCIEIEzNCJso_5banbbDRuwRTg9bijGfNaumJNqM9u1PuKb7";
const adapt = (i) => ({
  apiKey: fixtureApiKey,
  room: i.room,
  participant: i.participant,
  expiration: BigInt(i.expiration),
});
for (const v of document.valid) {
  const token = signParticipantToken(adapt(v.input));
  assert.equal(token, v.token, v.name);
  const [header, claims, signature] = token.split(".");
  assert.equal(Buffer.from(header, "base64url").toString(), v.header);
  assert.equal(Buffer.from(claims, "base64url").toString(), v.claims);
  assert.equal(header + "." + claims, v.signing_input);
  assert.equal(signature, v.signature);
  if (BigInt(v.input.expiration) <= BigInt(Number.MAX_SAFE_INTEGER)) {
    assert.equal(
      signParticipantToken({
        ...adapt(v.input),
        expiration: Number(v.input.expiration),
      }),
      v.token,
    );
  }
}
for (const v of document.invalid) {
  // Only expiration must be validated locally; the SFU validates IDs.
  if (v.field !== "expiration") continue;
  const input = adapt(document.base);
  if (v.value === null) delete input[v.field];
  else if (v.representation === "number") {
    input[v.field] =
      v.name === "expiration-overflow" ? BigInt(v.value) : Number(v.value);
  } else input[v.field] = v.value;
  assert.throws(
    () => signParticipantToken(input),
    (error) => {
      const diagnostic = `${error}\n${inspect(error)}`;
      assert(!diagnostic.includes(fixtureApiKey), v.name);
      assert(!diagnostic.includes("4ccd089b28ff96da"));
      return true;
    },
    v.name,
  );
}

// Identity strings are copied without parsing or reconstructing PulseBeam IDs.
const arbitraryKey = INSECURE_DEV_API_KEY.split(".");
arbitraryKey[0] = "unrecognized-issuer";
arbitraryKey[1] = "unrecognized-kid";
const opaqueToken = signParticipantToken({
  apiKey: arbitraryKey.join("."),
  room: "room with spaces",
  participant: "participant/with/slashes",
  expiration: 2000,
});
const opaqueHeader = JSON.parse(
  Buffer.from(opaqueToken.split(".")[0], "base64url"),
);
const opaqueClaims = JSON.parse(
  Buffer.from(opaqueToken.split(".")[1], "base64url"),
);
assert.equal(opaqueHeader.kid, arbitraryKey[1]);
assert.equal(opaqueClaims.iss, arbitraryKey[0]);
assert.equal(opaqueClaims.room, "room with spaces");
assert.equal(opaqueClaims.sub, "participant/with/slashes");

// Resolve declarations through the installed package's public export map.
writeFileSync(
  join(directory, "consumer.ts"),
  `import { signParticipantToken, INSECURE_DEV_API_KEY } from "@pulsebeam/server";
const compactKey: string = INSECURE_DEV_API_KEY;
const input = {apiKey:compactKey, room:"r", participant:"p", expiration:18446744073709551615n};
const token: string = signParticipantToken(input);
signParticipantToken({...input, expiration: 2000});
// @ts-expect-error cannot use legacy credential fields
signParticipantToken({projectId:"p", keyId:"k", secret:"s", room:"r", participant:"p", expiration:2000});
// @ts-expect-error expiration is mandatory
signParticipantToken({apiKey:compactKey, room:"r", participant:"p"});
// @ts-expect-error strings are not integer representations in the public API
signParticipantToken({...input, expiration:"2000"});
// @ts-expect-error booleans are not integer representations
signParticipantToken({...input, expiration:true});
`,
);
const program = ts.createProgram([join(directory, "consumer.ts")], {
  strict: true,
  noEmit: true,
  types: [],
  target: ts.ScriptTarget.ES2022,
  module: ts.ModuleKind.NodeNext,
  moduleResolution: ts.ModuleResolutionKind.NodeNext,
});
const diagnostics = ts.getPreEmitDiagnostics(program);
assert.equal(
  diagnostics.length,
  0,
  ts.formatDiagnosticsWithColorAndContext(diagnostics, {
    getCanonicalFileName: (f) => f,
    getCurrentDirectory: () => directory,
    getNewLine: () => "\n",
  }),
);

for (const invalid of [
  INSECURE_DEV_API_KEY.replace(".MC4CAQ", ".invalid"),
  INSECURE_DEV_API_KEY.slice(0, -1),
  INSECURE_DEV_API_KEY + ".extra",
  INSECURE_DEV_API_KEY.replace(".MC4CAQ", ".MCowBQ"),
]) {
  assert.throws(
    () =>
      signParticipantToken({
        apiKey: invalid,
        room: "local",
        participant: "test",
        expiration: 2000,
      }),
    /API key|credential/i,
  );
}
