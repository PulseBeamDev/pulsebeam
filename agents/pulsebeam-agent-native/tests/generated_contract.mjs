import { cpSync, mkdtempSync, readdirSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const runfiles = resolve(here, "../../..");
const temporary = mkdtempSync(join(tmpdir(), "pulsebeam-node-contract-"));
try {
  cpSync(resolve(here, "../contract_runtime"), join(temporary, "contract"), { recursive: true });
  writeFileSync(join(temporary, "package.json"), '{"private":true,"type":"module"}\n');
  symlinkSync(resolve(here, "../node_modules"), join(temporary, "node_modules"), "dir");
  // Keep the copied library two levels below its declared solib runtime root.
  for (const name of readdirSync(runfiles).filter((name) => name.startsWith("_solib_"))) {
    symlinkSync(join(runfiles, name), join(temporary, name), "dir");
  }
  await import(pathToFileURL(join(temporary, "contract/tests/generated_contract.js")).href);
} finally {
  rmSync(temporary, { recursive: true, force: true });
}
