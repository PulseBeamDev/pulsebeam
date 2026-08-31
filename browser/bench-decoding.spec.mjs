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
const serverBin = process.env.PULSEBEAM_BROWSER_SERVER ?? path.join(root, "target", "release", "pulsebeam");

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
  await expect.poll(async () => {
    const socket = net.connect({ host: "127.0.0.1", port });
    const connected = await new Promise((resolve) => {
      socket.once("connect", () => resolve(true));
      socket.once("error", () => resolve(false));
    });
    socket.destroy();
    return connected;
  }, { timeout: 10_000, message: `PulseBeam did not bind TCP/${port}` }).toBe(true);
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
    if (request.url !== "/manual-subscriptions.html") {
      response.writeHead(404).end();
      return;
    }
    response.writeHead(200, { "content-type": "text/html" });
    response.end(readFileSync(path.join(root, "browser", "manual-subscriptions.html")));
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
  const pulsebeam = run(serverBin, [
    "--dev",
    "--api-port", String(apiPort),
    "--metrics-port", String(metricsPort),
    "--rtc-port", String(rtcPort),
  ]);
  await waitForPort(apiPort);
  if (pulsebeam.child.exitCode !== null) {
    throw new Error(`PulseBeam exited before browser join: ${pulsebeam.child.exitCode}`);
  }
  return { apiUrl: `http://127.0.0.1:${apiPort}`, pulsebeam };
}

function decodedFrames(stats) {
  return stats.inbound.reduce((total, stream) => total + stream.framesDecoded, 0);
}

function renderedAt(stats, width, height) {
  return stats.rendered.some((video) => video.width === width && video.height === height);
}

function presentsAt(stats, browserName, width, height) {
  if (browserName !== "webkit") return renderedAt(stats, width, height);
  return stats.inbound.some((stream) =>
    stream.framesDecoded > 0 && stream.frameWidth === width && stream.frameHeight === height);
}

function publishesRequiredLayers(stats, browserName) {
  if (browserName === "webkit") {
    return stats.outbound.some((stream) => stream.packetsSent > 0);
  }
  return ["q", "h", "f"].every((rid) =>
    stats.outbound.some((stream) => stream.rid === rid && stream.packetsSent > 0));
}

function feedbackIsHealthy(stats, browserName) {
  const videoReports = stats.remoteInbound.filter((report) => report.kind === "video");
  return videoReports.length > 0 &&
    videoReports.every((report) => report.packetsLost === 0) &&
    videoReports.some((report) => report.roundTripTimeMeasurements > 0) &&
    (browserName === "firefox" || stats.availableOutgoingBitrate >= 1_400_000);
}

test("production SFU browser contract", async ({ browser, browserName }) => {
  test.setTimeout(90_000);
  const staticServer = await startStaticServer();
  const pageUrl = `http://127.0.0.1:${staticServer.address().port}/manual-subscriptions.html`;
  const leftContext = await browser.newContext();
  const rightContext = await browser.newContext();
  const left = await leftContext.newPage();
  const right = await rightContext.newPage();
  let pulsebeam;

  try {
    await Promise.all([left.goto(pageUrl), right.goto(pageUrl)]);
    await Promise.all([
      left.locator("#enable-media").click(),
      right.locator("#enable-media").click(),
    ]);
    await expect.poll(async () => Promise.all([
      left.evaluate(() => window.pulsebeamManual.supportsH264()),
      right.evaluate(() => window.pulsebeamManual.supportsH264()),
    ]), { timeout: 30_000, message: `${browserName} must provide H.264 send and receive` })
      .toEqual([true, true]);

    const started = await startPulseBeam();
    pulsebeam = started.pulsebeam;
    await Promise.all([
      left.evaluate((apiUrl) => window.pulsebeamManual.connect({ apiUrl, room: "browser-contract" }), started.apiUrl),
      right.evaluate((apiUrl) => window.pulsebeamManual.connect({ apiUrl, room: "browser-contract" }), started.apiUrl),
    ]);

    await expect.poll(async () => {
      const stats = await Promise.all([
        left.evaluate(() => window.pulsebeamManual.stats()),
        right.evaluate(() => window.pulsebeamManual.stats()),
      ]);
      return stats.every((value) =>
        value.connectionState === "connected" &&
        publishesRequiredLayers(value, browserName) &&
        feedbackIsHealthy(value, browserName) &&
        decodedFrames(value) >= 20 &&
        value.inbound.some((stream) => stream.keyFramesDecoded >= 1) &&
        presentsAt(value, browserName, 1280, 720) &&
        value.outboundAudio.some((stream) =>
          stream.packetsSent > 20 && stream.codec?.toLowerCase() === "audio/opus") &&
        value.inboundAudio.some((stream) =>
          stream.packetsReceived > 20 && stream.codec?.toLowerCase() === "audio/opus") &&
        value.dataChannel.readyState === "open" &&
        value.dataChannel.intentsSent > 0 &&
        value.dataChannel.messagesReceived > 0);
    }, { timeout: 25_000, message: `${browserName} did not establish production media and signaling` }).toBe(true);

    const before = await Promise.all([
      left.evaluate(() => window.pulsebeamManual.stats()),
      right.evaluate(() => window.pulsebeamManual.stats()),
    ]);
    await expect.poll(async () => {
      const after = await Promise.all([
        left.evaluate(() => window.pulsebeamManual.stats()),
        right.evaluate(() => window.pulsebeamManual.stats()),
      ]);
      return after.every((value, index) =>
        decodedFrames(value) - decodedFrames(before[index]) >= 20 &&
        value.inboundAudio.reduce((sum, stream) => sum + stream.packetsReceived, 0) -
          before[index].inboundAudio.reduce((sum, stream) => sum + stream.packetsReceived, 0) >= 20);
    }, { timeout: 8_000, message: `${browserName} video froze after first render` }).toBe(true);

    const beforeLow = await Promise.all([
      left.evaluate(() => window.pulsebeamManual.stats()),
      right.evaluate(() => window.pulsebeamManual.stats()),
    ]);
    await Promise.all([
      left.evaluate(() => window.pulsebeamManual.setSubscriptionHeight(180)),
      right.evaluate(() => window.pulsebeamManual.setSubscriptionHeight(180)),
    ]);
    await expect.poll(async () => {
      const stats = await Promise.all([
        left.evaluate(() => window.pulsebeamManual.stats()),
        right.evaluate(() => window.pulsebeamManual.stats()),
      ]);
      return stats.every((value, index) => browserName === "webkit"
        ? decodedFrames(value) - decodedFrames(beforeLow[index]) >= 5 &&
          presentsAt(value, browserName, 1280, 720)
        : renderedAt(value, 320, 180));
    }, { timeout: 15_000, message: `${browserName} did not render the selected low layer` }).toBe(true);

    await Promise.all([
      left.evaluate(() => window.pulsebeamManual.setSubscriptionHeight(720)),
      right.evaluate(() => window.pulsebeamManual.setSubscriptionHeight(720)),
    ]);
    await expect.poll(async () => {
      const stats = await Promise.all([
        left.evaluate(() => window.pulsebeamManual.stats()),
        right.evaluate(() => window.pulsebeamManual.stats()),
      ]);
      return stats.every((value) => presentsAt(value, browserName, 1280, 720));
    }, { timeout: 15_000, message: `${browserName} did not recover the selected high layer` }).toBe(true);

    await Promise.all([
      left.evaluate(() => window.pulsebeamManual.setPublishing(false)),
      right.evaluate(() => window.pulsebeamManual.setPublishing(false)),
    ]);
    await new Promise((resolve) => setTimeout(resolve, 6_000));
    const paused = await Promise.all([
      left.evaluate(() => window.pulsebeamManual.stats()),
      right.evaluate(() => window.pulsebeamManual.stats()),
    ]);
    await Promise.all([
      left.evaluate(() => window.pulsebeamManual.setPublishing(true)),
      right.evaluate(() => window.pulsebeamManual.setPublishing(true)),
    ]);
    await expect.poll(async () => {
      const resumed = await Promise.all([
        left.evaluate(() => window.pulsebeamManual.stats()),
        right.evaluate(() => window.pulsebeamManual.stats()),
      ]);
      return resumed.every((value, index) =>
        decodedFrames(value) - decodedFrames(paused[index]) >= 10 &&
        value.inbound.some((stream, streamIndex) =>
          stream.keyFramesDecoded > (paused[index].inbound[streamIndex]?.keyFramesDecoded ?? 0)) &&
        presentsAt(value, browserName, 1280, 720));
    }, { timeout: 15_000, message: `${browserName} did not recover renderable video after a paused source resumed` }).toBe(true);

    const backpressure = await Promise.all([
      left.evaluate(() => window.pulsebeamManual.exerciseDataBackpressure()),
      right.evaluate(() => window.pulsebeamManual.exerciseDataBackpressure()),
    ]);
    expect(backpressure.every((result) =>
      result.peakBufferedAmount > 0 && result.finalBufferedAmount === 0),
    `${browserName} data channel did not expose and drain backpressure`).toBe(true);
    const beforeDataControl = await Promise.all([
      left.evaluate(() => window.pulsebeamManual.stats()),
      right.evaluate(() => window.pulsebeamManual.stats()),
    ]);
    await Promise.all([
      left.evaluate(() => window.pulsebeamManual.setSubscriptionHeight(360)),
      right.evaluate(() => window.pulsebeamManual.setSubscriptionHeight(360)),
    ]);
    await expect.poll(async () => {
      const stats = await Promise.all([
        left.evaluate(() => window.pulsebeamManual.stats()),
        right.evaluate(() => window.pulsebeamManual.stats()),
      ]);
      return stats.every((value, index) => browserName === "webkit"
        ? decodedFrames(value) - decodedFrames(beforeDataControl[index]) >= 5 &&
          presentsAt(value, browserName, 1280, 720)
        : renderedAt(value, 640, 360));
    }, { timeout: 10_000, message: `${browserName} data-channel round trip stopped under backpressure` }).toBe(true);
    await Promise.all([
      left.evaluate(() => window.pulsebeamManual.setSubscriptionHeight(720)),
      right.evaluate(() => window.pulsebeamManual.setSubscriptionHeight(720)),
    ]);
    await expect.poll(async () => {
      const stats = await Promise.all([
        left.evaluate(() => window.pulsebeamManual.stats()),
        right.evaluate(() => window.pulsebeamManual.stats()),
      ]);
      return stats.every((value) => presentsAt(value, browserName, 1280, 720));
    }, { timeout: 10_000, message: `${browserName} did not recover high video after data-channel backpressure` }).toBe(true);
  } catch (error) {
    const diagnostics = await Promise.all([
      left.evaluate(async () => ({ stats: await window.pulsebeamManual?.stats?.(), sdp: window.pulsebeamManual?.sdp?.() })).catch(() => null),
      right.evaluate(async () => ({ stats: await window.pulsebeamManual?.stats?.(), sdp: window.pulsebeamManual?.sdp?.() })).catch(() => null),
    ]);
    throw new Error(`${error.message}\nBrowser diagnostics: ${JSON.stringify(diagnostics)}\nPulseBeam:\n${pulsebeam?.output() ?? "not started"}`);
  } finally {
    staticServer.close();
    await stop(pulsebeam?.child);
    await Promise.all([leftContext.close(), rightContext.close()]);
  }
});
