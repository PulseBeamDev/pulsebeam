// Bazel's fixed env values cannot express a caller-overridable default.
process.env.NEXT_PUBLIC_PULSEBEAM_SERVER_URL ??= "http://localhost:7070";
process.env.NEXT_TELEMETRY_DISABLED ??= "1";
// Match production bundling: Turbopack cannot follow Bazel's source symlinks.
process.argv.splice(1, 1, "next", "dev", "--webpack");
await import("../node_modules/next/dist/bin/next");
