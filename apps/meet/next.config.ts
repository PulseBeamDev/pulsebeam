import type { NextConfig } from "next";
import { resolve } from "node:path";

const apiUrl = process.env.NEXT_PUBLIC_PULSEBEAM_API_URL;
if (process.env.NODE_ENV === "production") {
  if (!apiUrl) {
    throw new Error(
      "NEXT_PUBLIC_PULSEBEAM_API_URL must be set for a production Meet build",
    );
  }
  let endpoint: URL;
  try {
    endpoint = new URL(apiUrl);
  } catch {
    throw new Error(
      "NEXT_PUBLIC_PULSEBEAM_API_URL must be an absolute HTTPS URL",
    );
  }
  if (endpoint.protocol !== "https:" || endpoint.search || endpoint.hash) {
    throw new Error(
      "NEXT_PUBLIC_PULSEBEAM_API_URL must use HTTPS without a query or fragment",
    );
  }
}

const nextConfig: NextConfig = {
  transpilePackages: ["@pulsebeam/react"],
  turbopack: { root: resolve(import.meta.dirname, "../..") },
  reactStrictMode: true,
  output: "export",
  images: { unoptimized: true },
};

export default nextConfig;
