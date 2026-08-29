import { test, expect, chromium, firefox, webkit } from "@playwright/test";
import { createServer } from "node:http";
import { once } from "node:events";
import { spawn } from "node:child_process";
import dgram from "node:dgram";
import { readFileSync } from "node:fs";
import net from "node:net";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const serverBin = process.env.PULSEBEAM_BROWSER_SERVER ?? path.join(root, "target", "release", "pulsebeam");
const cliBin = path.join(root, "target", "release", "pulsebeam-cli");
const engines = { chromium, firefox, webkit };

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

async function startStaticServer() {
  const server = createServer((request, response) => {
    const pages = {
      "/publisher.html": "publisher.html",
      "/viewer.html": "viewer.html",
      "/manual-subscriptions.html": "manual-subscriptions.html",
    };
    const page = pages[request.url];
    if (!page) {
      response.writeHead(404).end();
      return;
    }
    response.writeHead(200, { "content-type": "text/html" });
    response.end(readFileSync(path.join(root, "browser", page)));
  });
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  return server;
}

async function startPulseBeam() {
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
  await waitForPort(apiPort);
  if (pulsebeam.child.exitCode !== null) {
    throw new Error(`pulsebeam exited before browser join: ${pulsebeam.child.exitCode}`);
  }
  return { apiUrl, pulsebeam };
}

function commonCodecs(publisher, viewer) {
  const codecs = ["video/h264"].filter((codec) =>
    publisher.includes(codec) && viewer.includes(codec));
  return codecs;
}

function decodedFrames(stats) {
  return stats.inbound.reduce((total, stream) => total + stream.framesDecoded, 0);
}

function jitterSamples(stats) {
  return stats.inbound.reduce((total, stream) => total + stream.jitterBufferEmittedCount, 0);
}

function jitterDelay(stats) {
  return stats.inbound.reduce((total, stream) => total + stream.jitterBufferDelay, 0);
}

test("a late meet-shaped browser decodes bench H.264 video", async ({ page }) => {
  const browserConsole = [];
  page.on("console", (message) => {
    browserConsole.push(`${message.type()}: ${message.text()}`);
    if (browserConsole.length > 100) browserConsole.shift();
  });
  const staticServer = await startStaticServer();
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

test("manual browser subscriptions sustain decoded H.264 in both directions", async () => {
  test.setTimeout(90_000);
  const fixtureServer = await startStaticServer();
  const port = fixtureServer.address().port;
  const leftBrowser = await chromium.launch({ headless: true });
  const rightBrowser = await chromium.launch({ headless: true });
  const left = await leftBrowser.newPage();
  const right = await rightBrowser.newPage();
  let pulsebeam;
  try {
    await Promise.all([
      left.goto(`http://127.0.0.1:${port}/manual-subscriptions.html`),
      right.goto(`http://127.0.0.1:${port}/manual-subscriptions.html`),
    ]);
    const started = await startPulseBeam();
    pulsebeam = started.pulsebeam;
    await left.evaluate((apiUrl) => window.pulsebeamManual.connect({
      apiUrl,
      room: "manual-browser-subscriptions",
    }), started.apiUrl);
    await right.evaluate((apiUrl) => window.pulsebeamManual.connect({
      apiUrl,
      room: "manual-browser-subscriptions",
    }), started.apiUrl);
    await expect.poll(async () => {
      const [leftStats, rightStats] = await Promise.all([
        left.evaluate(() => window.pulsebeamManual.stats()),
        right.evaluate(() => window.pulsebeamManual.stats()),
      ]);
      return [leftStats, rightStats].every((stats) =>
        stats.connectionState === "connected" &&
        stats.outbound.some((stream) => stream.packetsSent > 0) &&
        decodedFrames(stats) >= 20 &&
        stats.rendered.some((video) => video.width > 0 && video.height > 0));
    }, { timeout: 20_000, message: "manual browser subscriptions never produced rendered video" }).toBe(true);

    const before = await Promise.all([
      left.evaluate(() => window.pulsebeamManual.stats()),
      right.evaluate(() => window.pulsebeamManual.stats()),
    ]);
    await expect.poll(async () => {
      const after = await Promise.all([
        left.evaluate(() => window.pulsebeamManual.stats()),
        right.evaluate(() => window.pulsebeamManual.stats()),
      ]);
      return after.every((stats, index) => {
        const frames = decodedFrames(stats) - decodedFrames(before[index]);
        const samples = jitterSamples(stats) - jitterSamples(before[index]);
        const delay = jitterDelay(stats) - jitterDelay(before[index]);
        return frames >= 20 && (samples === 0 || delay / samples <= 0.5);
      });
    }, { timeout: 8_000, message: "manual browser video froze or accumulated excessive jitter-buffer delay" }).toBe(true);
  } catch (error) {
    const [leftStats, rightStats, leftSdp, rightSdp] = await Promise.all([
      left.evaluate(() => window.pulsebeamManual?.stats?.()).catch(() => null),
      right.evaluate(() => window.pulsebeamManual?.stats?.()).catch(() => null),
      left.evaluate(() => window.pulsebeamManual?.sdp?.()).catch(() => null),
      right.evaluate(() => window.pulsebeamManual?.sdp?.()).catch(() => null),
    ]);
    throw new Error(`${error.message}\nLeft browser: ${JSON.stringify(leftStats)}\nRight browser: ${JSON.stringify(rightStats)}\nLeft SDP: ${JSON.stringify(leftSdp)}\nRight SDP: ${JSON.stringify(rightSdp)}\nPulseBeam:\n${pulsebeam?.output() ?? "not started"}`);
  } finally {
    fixtureServer.close();
    await stop(pulsebeam?.child);
    await leftBrowser.close();
    await rightBrowser.close();
  }
});

for (const [publisherName, publisherType] of Object.entries(engines)) {
  for (const [viewerName, viewerType] of Object.entries(engines)) {
    test(`${publisherName} publisher renders on ${viewerName}`, async () => {
      test.setTimeout(120_000);
      const fixtureServer = await startStaticServer();
      const port = fixtureServer.address().port;
      const publisherBrowser = await publisherType.launch({ headless: true });
      const viewerBrowser = await viewerType.launch({ headless: true });
      const publisherPage = await publisherBrowser.newPage();
      const viewerPage = await viewerBrowser.newPage();
      let pulsebeam;
      try {
        await Promise.all([
          publisherPage.goto(`http://127.0.0.1:${port}/publisher.html`),
          viewerPage.goto(`http://127.0.0.1:${port}/viewer.html`),
        ]);
        const [publisherCodecs, viewerCodecs] = await Promise.all([
          publisherPage.evaluate(() => window.pulsebeamPublisher.codecs()),
          viewerPage.evaluate(() => window.pulsebeamViewer.codecs()),
        ]);
        test.skip(
          commonCodecs(publisherCodecs, viewerCodecs).length === 0,
          "browser pair has no common H.264 codec",
        );
        for (const codec of commonCodecs(publisherCodecs, viewerCodecs)) {
          await Promise.all([
            publisherPage.goto(`http://127.0.0.1:${port}/publisher.html`),
            viewerPage.goto(`http://127.0.0.1:${port}/viewer.html`),
          ]);
          const started = await startPulseBeam();
          pulsebeam = started.pulsebeam;
          await publisherPage.evaluate(({ apiUrl, codec }) => window.pulsebeamPublisher.connect({
            apiUrl,
            room: "browser-matrix",
            codec,
          }), { apiUrl: started.apiUrl, codec });
          await expect.poll(async () => {
            const stats = await publisherPage.evaluate(() => window.pulsebeamPublisher.stats());
            return stats.connectionState === "connected" &&
              stats.outbound.some((stream) => stream.bytesSent > 0 && stream.packetsSent > 0);
          }, { timeout: 20_000, message: `browser publisher never sent ${codec} RTP` }).toBe(true);
          await viewerPage.evaluate(({ apiUrl, codec }) => window.pulsebeamViewer.connect({
            apiUrl,
            room: "browser-matrix",
            codec,
          }), { apiUrl: started.apiUrl, codec });
          await expect.poll(async () => {
            const stats = await viewerPage.evaluate(() => window.pulsebeamViewer.stats());
            return stats.connectionState === "connected" &&
              stats.inbound.some((stream) => stream.codec?.toLowerCase() === codec && stream.framesDecoded > 0) &&
              stats.rendered.some((video) => video.width > 0 && video.height > 0);
          }, { timeout: 20_000, message: `browser viewer never decoded ${codec}` }).toBe(true);
          await stop(pulsebeam.child);
          pulsebeam = undefined;
        }
      } catch (error) {
        const [publisherStats, viewerStats] = await Promise.all([
          publisherPage.evaluate(() => window.pulsebeamPublisher?.stats?.()).catch(() => null),
          viewerPage.evaluate(() => window.pulsebeamViewer?.stats?.()).catch(() => null),
        ]);
        const [publisherSdp, viewerSdp] = await Promise.all([
          publisherPage.evaluate(() => window.pulsebeamPublisher?.sdp?.()).catch(() => null),
          viewerPage.evaluate(() => window.pulsebeamViewer?.sdp?.()).catch(() => null),
        ]);
        throw new Error(`${error.message}\nPublisher: ${JSON.stringify(publisherStats)}\nViewer: ${JSON.stringify(viewerStats)}\nPublisher SDP: ${JSON.stringify(publisherSdp)}\nViewer SDP: ${JSON.stringify(viewerSdp)}\nPulseBeam:\n${pulsebeam?.output() ?? "not started"}`);
      } finally {
        fixtureServer.close();
        await stop(pulsebeam?.child);
        await publisherBrowser.close();
        await viewerBrowser.close();
      }
    });
  }
}
