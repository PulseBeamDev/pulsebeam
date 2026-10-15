import assert from "node:assert/strict";
import { test } from "node:test";
import { sessionKey } from "../unit-javascript/session-key.js";

const config = { endpoint: "https://example.test", token: "first" };

test("credentials and logging update the existing session", () => {
  const key = sessionKey(config);
  assert.equal(sessionKey({ ...config, token: "renewed" }), key);
  assert.equal(sessionKey({ ...config, logging: { level: "debug" } }), key);
  assert.equal(
    sessionKey({
      ...config,
      topology: {
        localVideos: 2,
        localAudios: 2,
        remoteVideos: 16,
        remoteAudios: 8,
      },
    }),
    key,
  );
});

test("endpoint and every capacity are construction identity, including zero", () => {
  const key = sessionKey(config);
  assert.notEqual(
    sessionKey({ ...config, endpoint: "https://other.test" }),
    key,
  );
  for (const capacity of [
    "localVideos",
    "localAudios",
    "remoteVideos",
    "remoteAudios",
  ])
    for (const value of [0, 1, 3])
      assert.notEqual(
        sessionKey({ ...config, topology: { [capacity]: value } }),
        key,
      );
});
