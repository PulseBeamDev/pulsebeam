import { readFileSync, readdirSync } from "node:fs";
import { join } from "node:path";

function files(directory) {
  return readdirSync(directory, { withFileTypes: true }).flatMap((entry) =>
    entry.isDirectory() ? files(join(directory, entry.name)) : [join(directory, entry.name)],
  );
}

const forbidden = /@pulsebeam\/(?:web|core)|pulsebeam-js|agents\/pulsebeam-agent-web/;
for (const file of files("app").concat(files("components"), files("hooks"))) {
  if (/\.[cm]?[jt]sx?$/.test(file) && forbidden.test(readFileSync(file, "utf8"))) {
    throw new Error(`PulseBeam boundary violation in ${file}`);
  }
}
const manifest = JSON.parse(readFileSync("package.json", "utf8"));
const pulsebeam = Object.keys(manifest.dependencies).filter((name) => name.startsWith("@pulsebeam/"));
if (pulsebeam.length !== 1 || pulsebeam[0] !== "@pulsebeam/react") {
  throw new Error("Meet must directly depend only on @pulsebeam/react");
}
