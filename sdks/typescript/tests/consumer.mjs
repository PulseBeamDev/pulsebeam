import assert from "node:assert/strict";
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
const [artifact, fixtures] = process.argv.slice(2).map((p) => resolve(p));
const directory = mkdtempSync(
  join(process.env.TEST_TMPDIR || tmpdir(), "server-consumer-"),
);
const installed = join(directory, "node_modules/@pulsebeam/server");
mkdirSync(installed, { recursive: true });
cpSync(artifact, installed, { recursive: true, dereference: true });
writeFileSync(join(directory, "package.json"), '{"type":"module"}');
writeFileSync(
  join(directory, "consumer.mjs"),
  'export { signParticipantToken } from "@pulsebeam/server";',
);
const { signParticipantToken } = await import(
  pathToFileURL(join(directory, "consumer.mjs"))
);
const document = JSON.parse(readFileSync(fixtures));
const adapt = (i) => ({
  projectId: i.project_id,
  keyId: i.key_id,
  secret: i.secret,
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
const fields = {
  project_id: "projectId",
  key_id: "keyId",
  secret: "secret",
  room: "room",
  participant: "participant",
  expiration: "expiration",
};
for (const v of document.invalid) {
  const input = adapt(document.base);
  const field = fields[v.field];
  assert(field, v.name);
  if (v.value === null) delete input[field];
  else if (v.representation === "number") {
    input[field] =
      v.name === "expiration-overflow" ? BigInt(v.value) : Number(v.value);
  } else input[field] = v.value;
  assert.throws(
    () => signParticipantToken(input),
    (error) => {
      const diagnostic = `${error}\n${inspect(error)}`;
      if (typeof input.secret === "string" && input.secret)
        assert(!diagnostic.includes(input.secret), v.name);
      assert(!diagnostic.includes("4ccd089b28ff96da"));
      return true;
    },
    v.name,
  );
}
assert(!inspect(signParticipantToken).includes(document.base.secret));

// Resolve declarations through the installed package's public export map.
writeFileSync(
  join(directory, "consumer.ts"),
  `import { signParticipantToken } from "@pulsebeam/server";
const input = {projectId:"p", keyId:"k", secret:"s", room:"r", participant:"p", expiration:18446744073709551615n};
const token: string = signParticipantToken(input);
signParticipantToken({...input, expiration: 2000});
// @ts-expect-error expiration is mandatory
signParticipantToken({projectId:"p", keyId:"k", secret:"s", room:"r", participant:"p"});
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
