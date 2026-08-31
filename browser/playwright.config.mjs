import { defineConfig } from "@playwright/test";

export default defineConfig({
  testDir: ".",
  testMatch: "bench-decoding.spec.mjs",
  timeout: 45_000,
  workers: 1,
  projects: [
    { name: "chromium", use: { browserName: "chromium", headless: true } },
    {
      name: "firefox",
      use: {
        browserName: "firefox",
        headless: true,
        firefoxUserPrefs: {
          "media.gmp-gmpopenh264.enabled": true,
          "media.webrtc.simulcast.h264.enabled": true,
        },
      },
    },
    { name: "webkit", use: { browserName: "webkit", headless: true } },
  ],
});
