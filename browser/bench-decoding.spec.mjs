import { test, expect } from "@playwright/test";
import { createServer } from "node:http";
import { once } from "node:events";
import { spawn } from "node:child_process";
import dgram from "node:dgram";
import { readFileSync } from "node:fs";
import net from "node:net";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const serverBin = path.join(root, "target", "release", "pulsebeam");
const cliBin = path.join(root, "target", "release", "pulsebeam-cli");

function run(command, args) {
  const child = spawn(command, args, { cwd: root, stdio: ["ignore", "pipe", "pipe"] });
  let output = "";
  for (const stream of [child.stdout, child.stderr]) {
    stream.on("data", (chunk) => {
      output = `${output}${chunk}`.slice(-20_000);
    });
  }
  return { child, output: () => output };
}

async function freePort() {
  const server = net.createServer();
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  const { port } = server.address();
  await new Promise((resolve) => server.close(resolve));
  return port;
}

async function freeUdpPort() {
  const socket = dgram.createSocket("udp4");
  socket.bind(0, "127.0.0.1");
  await once(socket, "listening");
  const { port } = socket.address();
  await new Promise((resolve) => socket.close(resolve));
  return port;
}

async function waitForPort(port) {
  const deadline = Date.now() + 10_000;
  while (Date.now() < deadline) {
    const socket = net.connect({ host: "127.0.0.1", port });
    const connected = await new Promise((resolve) => {
      socket.once("connect", () => resolve(true));
      socket.once("error", () => resolve(false));
    });
    socket.destroy();
    if (connected) return;
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
  throw new Error(`pulsebeam did not bind TCP/${port}`);
}

async function stop(process) {
  if (!process || process.exitCode !== null) return;
  process.kill("SIGINT");
  await Promise.race([
    once(process, "exit"),
    new Promise((resolve) => setTimeout(resolve, 5_000)),
  ]);
  if (process.exitCode === null) process.kill("SIGKILL");
}

test("a late meet-shaped browser decodes bench H.264 video", async ({ page }) => {
  const browserConsole = [];
  page.on("console", (message) => {
    browserConsole.push(`${message.type()}: ${message.text()}`);
    if (browserConsole.length > 100) browserConsole.shift();
  });
  const staticServer = createServer((request, response) => {
    if (request.url !== "/viewer.html") {
      response.writeHead(404).end();
      return;
    }
    response.writeHead(200, { "content-type": "text/html" });
    response.end(readFileSync(path.join(root, "browser", "viewer.html")));
  });
  staticServer.listen(0, "127.0.0.1");
  await once(staticServer, "listening");
  const viewerPort = staticServer.address().port;

  const [apiPort, metricsPort, rtcPort] = await Promise.all([
    freePort(),
    freePort(),
    freeUdpPort(),
  ]);
  const apiUrl = `http://127.0.0.1:${apiPort}`;
  const pulsebeam = run(serverBin, [
    "--dev",
    "--api-port", String(apiPort),
    "--metrics-port", String(metricsPort),
    "--rtc-port", String(rtcPort),
  ]);
  let bench;
  try {
    await waitForPort(apiPort);
    if (pulsebeam.child.exitCode !== null) {
      throw new Error(`pulsebeam exited before the browser joined: ${pulsebeam.child.exitCode}`);
    }
    bench = run(cliBin, [
      "--api-url", apiUrl,
      "bench",
      "--rooms", "1",
      "--users-per-room", "4",
      "--arrival-rate", "1",
      "--join-spread-secs", "1",
      "--max-rooms", "1",
      "--session-duration", "60",
      "--latency-file", "/tmp/pulsebeam-browser-latency.csv",
      "--snapshots-file", "/tmp/pulsebeam-browser-snapshots.csv",
    ]);
    await new Promise((resolve) => setTimeout(resolve, 3_000));

    await page.goto(`http://127.0.0.1:${viewerPort}/viewer.html`);
    await page.evaluate((apiUrl) => window.pulsebeamViewer.connect({
      apiUrl,
      room: "bench-room-0",
      meetShape: true,
    }), apiUrl);

    await expect.poll(async () => {
      const stats = await page.evaluate(() => window.pulsebeamViewer.stats());
      return stats.inbound.some((stream) => stream.framesDecoded > 0) &&
        stats.rendered.some((video) => video.width > 0 && video.height > 0);
    }, { timeout: 20_000, message: "browser never decoded bench video" }).toBe(true);
  } catch (error) {
    const browserStats = await page.evaluate(() => window.pulsebeamViewer?.stats?.()).catch(() => null);
    const sdp = await page.evaluate(() => window.pulsebeamViewer?.sdp?.()).catch(() => null);
    throw new Error(`${error.message}\nBrowser stats: ${JSON.stringify(browserStats)}\nSDP: ${JSON.stringify(sdp)}\nBrowser console:\n${browserConsole.join("\n")}\nServer log:\n${pulsebeam.output()}\nBench log:\n${bench?.output() ?? "not started"}`);
  } finally {
    staticServer.close();
    await stop(bench?.child);
    await stop(pulsebeam.child);
  }
});
