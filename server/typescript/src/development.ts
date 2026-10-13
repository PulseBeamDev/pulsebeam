import credentials from "./development.json" with { type: "json" };

/** Publicly known test keys for --dev servers. Never use in production. */
export const INSECURE_DEV_CREDENTIALS = Object.freeze({
  projectId: credentials.projectId,
  keyId: credentials.keyId,
  secret: credentials.secret,
});
