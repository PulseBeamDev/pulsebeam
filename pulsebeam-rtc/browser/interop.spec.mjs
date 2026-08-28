import { test, expect } from "@playwright/test";
import { createServer } from "node:http";
import { once } from "node:events";
import { spawn } from "node:child_process";
import { readFileSync } from "node:fs";
import net from "node:net";
import { networkInterfaces } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..");
const serverBin = path.join(root, "target", "release", "examples", "browser_interop");

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

async function unusedAddress() {
  const host = localAddress();
  const server = net.createServer();
  server.listen(0, host);
  await once(server, "listening");
  const address = server.address();
  server.close();
  await once(server, "close");
  return `${host}:${address.port}`;
}

function localAddress() {
  for (const addresses of Object.values(networkInterfaces())) {
    for (const address of addresses ?? []) {
      if (address.family === "IPv4" && !address.internal) return address.address;
    }
  }
  throw new Error("browser interoperability test requires a non-loopback IPv4 interface");
}

async function waitForReady(process) {
  await expect.poll(() => process.output().includes("READY"), {
    timeout: 10_000,
    message: "RTC compatibility server did not become ready",
  }).toBe(true);
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

test("browser publisher completes non-trickle ICE, DTLS, and RTP", async ({ page }) => {
  const host = localAddress();
  const staticServer = createServer((request, response) => {
    if (request.url !== "/publisher.html") {
      response.writeHead(404).end();
      return;
    }
    response.writeHead(200, { "content-type": "text/html" });
    response.end(readFileSync(path.join(root, "pulsebeam-rtc", "browser", "publisher.html")));
  });
  staticServer.listen(0, host);
  await once(staticServer, "listening");
  const staticAddress = staticServer.address();
  const httpAddress = await unusedAddress();
  const udpAddress = await unusedAddress();
  const rtc = run(serverBin, ["--http-address", httpAddress, "--udp-address", udpAddress]);

  try {
    await waitForReady(rtc);
    await page.goto(`http://${host}:${staticAddress.port}/publisher.html`);
    await page.evaluate((url) => window.pulsebeamRtcPublisher.connect(url), `http://${httpAddress}`);
    await expect.poll(async () => {
      const stats = await page.evaluate(() => window.pulsebeamRtcPublisher.stats());
      return stats.connectionState === "connected" &&
        stats.outbound.some((stream) => stream.bytesSent > 0 && stream.packetsSent > 0);
    }, { timeout: 20_000, message: "browser never connected and sent RTP" }).toBe(true);
    await expect.poll(() => rtc.output().includes("AUTHENTICATED"), {
      timeout: 10_000,
      message: "RTC server never authenticated browser RTP",
    }).toBe(true);
  } catch (error) {
    const stats = await page.evaluate(() => window.pulsebeamRtcPublisher?.stats?.()).catch(() => null);
    const sdp = await page.evaluate(() => window.pulsebeamRtcPublisher?.sdp?.()).catch(() => null);
    throw new Error(`${error.message}\nBrowser stats: ${JSON.stringify(stats)}\nSDP: ${JSON.stringify(sdp)}\nRTC server log:\n${rtc.output()}`);
  } finally {
    staticServer.close();
    await stop(rtc.child);
  }
});
