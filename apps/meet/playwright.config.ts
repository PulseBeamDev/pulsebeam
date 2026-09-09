import { defineConfig } from "@playwright/test";

export default defineConfig({ testDir: "./browser", use: { baseURL: process.env.MEET_URL ?? "http://127.0.0.1:3000" } });
