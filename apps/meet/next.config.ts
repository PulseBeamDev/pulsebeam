import type { NextConfig } from "next";
import { resolve } from "node:path";

const nextConfig: NextConfig = {
  transpilePackages: ["@pulsebeam/react"],
  turbopack: { root: resolve(import.meta.dirname, "../..") },
  reactStrictMode: true,
  output: "export",
  images: { unoptimized: true },
};

export default nextConfig;
