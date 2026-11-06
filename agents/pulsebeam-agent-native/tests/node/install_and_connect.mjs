import { execFileSync } from "node:child_process";
import { copyFileSync, mkdtempSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const npm = resolve(process.env.PULSEBEAM_NPM);
const archive = resolve(process.env.PULSEBEAM_NODE_PACKAGE);
const modules = resolve(here, "../../node_modules/@ubjs");
const nodeRuntime = realpathSync(join(modules, "node"));
const dependencies = [
  realpathSync(join(modules, "core")),
  nodeRuntime,
  realpathSync(join(dirname(nodeRuntime), "node-linux-x64-gnu")),
];
const temporary = mkdtempSync(join(tmpdir(), "native-node-consumer-"));
try {
  writeFileSync(join(temporary, "package.json"), '{"private":true,"type":"module"}\n');
  const npmOptions = { cwd: temporary, encoding: "utf8", env: { ...process.env, BAZEL_BINDIR: ".", npm_config_cache: join(temporary, "cache") } };
  const packed = dependencies.map((dependency) => {
    const result = execFileSync(npm, ["pack", dependency, "--json", "--ignore-scripts", "--pack-destination", temporary], npmOptions);
    return join(temporary, JSON.parse(result)[0].filename);
  });
  execFileSync(npm, ["install", "--offline", "--ignore-scripts", "--omit=optional", "--no-audit", "--no-fund", "--package-lock=false", archive, ...packed], npmOptions);
  copyFileSync(join(here, "installed.mjs"), join(temporary, "installed.mjs"));
  copyFileSync(join(here, "typecheck.mts"), join(temporary, "typecheck.mts"));
  const compiler = realpathSync(resolve(here, "../../node_modules/typescript/bin/tsc"));
  execFileSync(process.execPath, [compiler, "--noEmit", "--strict", "--skipLibCheck", "--target", "ES2022", "--module", "NodeNext", "--moduleResolution", "NodeNext", "--lib", "ES2022,DOM,ESNext.Disposable", "typecheck.mts"], { cwd: temporary, stdio: "inherit" });
  const env = { ...process.env };
  for (const variable of ["NODE_PATH", "NODE_OPTIONS", "LD_LIBRARY_PATH", "LD_PRELOAD", "RUNFILES_DIR", "RUNFILES_MANIFEST_FILE", "TEST_SRCDIR"]) delete env[variable];
  const baseline = process.env.PULSEBEAM_BASELINE_ROOT;
  if (baseline) {
    copyFileSync(process.env.PULSEBEAM_BASELINE_CA_FILE, join(temporary, "ca.pem"));
    copyFileSync(process.env.JS_BINARY__NODE_BINARY, join(temporary, "node"));
  }
  const executable = baseline ? "/usr/bin/bwrap" : process.execPath;
  const args = baseline ? [
    "--die-with-parent", "--ro-bind", baseline, "/", "--dev", "/dev", "--proc", "/proc",
    "--ro-bind", temporary, "/home",
    "--chdir", "/home", "--clearenv", "--setenv", "PATH", "/usr/bin:/bin",
    "--setenv", "SSL_CERT_FILE", "/home/ca.pem", "--",
    "/home/node", "/home/installed.mjs", ...process.argv.slice(2),
  ] : [join(temporary, "installed.mjs"), ...process.argv.slice(2)];
  console.log("installed Node package and types verified; starting RTC witness");
  execFileSync(executable, args, { cwd: temporary, env, stdio: "inherit", timeout: 30000 });
} finally {
  rmSync(temporary, { recursive: true, force: true });
}
