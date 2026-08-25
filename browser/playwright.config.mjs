import { defineConfig } from "@playwright/test";

export default defineConfig({
  testDir: ".",
  testMatch: "bench-decoding.spec.mjs",
  timeout: 45_000,
  workers: 1,
  use: {
    browserName: "chromium",
    headless: true,
  },
});
