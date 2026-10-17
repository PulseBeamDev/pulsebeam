import { readFileSync } from "node:fs";

const development = JSON.parse(
  readFileSync(new URL("./development.json", import.meta.url), "utf8"),
) as { apiKey: string };

/** Publicly known API key for local --dev servers. Never use in production. */
export const INSECURE_DEV_API_KEY: string = development.apiKey;
