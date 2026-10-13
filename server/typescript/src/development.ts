import { readFileSync } from "node:fs";

const credentials = JSON.parse(
  readFileSync(new URL("./development.json", import.meta.url), "utf8"),
) as { projectId: string; keyId: string; secret: string };

/** Publicly known test keys for --dev servers. Never use in production. */
export const INSECURE_DEV_CREDENTIALS = Object.freeze({
  projectId: credentials.projectId,
  keyId: credentials.keyId,
  secret: credentials.secret,
});
