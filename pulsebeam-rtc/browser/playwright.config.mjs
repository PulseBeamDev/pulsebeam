import { defineConfig } from "@playwright/test";

export default defineConfig({
  testDir: ".",
  testMatch: "interop.spec.mjs",
  timeout: 45_000,
  workers: 1,
  projects: [
    { name: "chromium", use: { browserName: "chromium", headless: true } },
    { name: "firefox", use: { browserName: "firefox", headless: true } },
    { name: "webkit", use: { browserName: "webkit", headless: true } },
  ],
});
