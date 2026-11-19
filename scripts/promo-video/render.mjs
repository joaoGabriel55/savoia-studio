#!/usr/bin/env node
// Renders scene.html into an MP4: serves the repo over HTTP, drives headless
// Chrome through the DevTools protocol (Node's built-in WebSocket, no npm
// dependencies), captures one PNG per frame and pipes them into ffmpeg.
//
//   node scripts/promo-video/render.mjs [--fps 30] [--from 0] [--to 47] [--out file.mp4]
//   node scripts/promo-video/render.mjs --still 18.5        # one PNG, for checking a moment
//   node scripts/promo-video/soundtrack.mjs                 # rebuild only the audio and remux it
//
// Needs Google Chrome (or CHROME=/path/to/chrome) and ffmpeg on PATH.

import { spawn, spawnSync } from "node:child_process";
import { createServer } from "node:http";
import { readFile, mkdtemp, rm, mkdir, writeFile } from "node:fs/promises";
import { existsSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, extname, normalize, dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { parseArgs } from "node:util";

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, "../..");

const { values: opts } = parseArgs({
  options: {
    fps: { type: "string", default: "30" },
    from: { type: "string" },
    to: { type: "string" },
    out: { type: "string", default: join(here, "out", "savoia-studio-promo.mp4") },
    "no-audio": { type: "boolean", default: false },
    still: { type: "string" },
  },
});

const W = 1920, H = 1080;
const MIME = { ".html": "text/html", ".svg": "image/svg+xml", ".webp": "image/webp", ".mp4": "video/mp4", ".woff2": "font/woff2", ".js": "text/javascript", ".css": "text/css", ".png": "image/png" };

// Static server with byte ranges, which <video> seeking needs.
const server = createServer(async (req, res) => {
  const path = normalize(join(root, decodeURIComponent(new URL(req.url, "http://x").pathname)));
  if (!path.startsWith(root) || !existsSync(path)) { res.writeHead(404).end(); return; }
  const body = await readFile(path);
  const type = MIME[extname(path)] || "application/octet-stream";
  const m = /bytes=(\d*)-(\d*)/.exec(req.headers.range || "");
  if (m) {
    const start = Number(m[1] || 0), end = m[2] ? Number(m[2]) : body.length - 1;
    res.writeHead(206, { "Content-Type": type, "Accept-Ranges": "bytes", "Content-Range": `bytes ${start}-${end}/${body.length}`, "Content-Length": end - start + 1 });
    res.end(body.subarray(start, end + 1));
  } else {
    res.writeHead(200, { "Content-Type": type, "Accept-Ranges": "bytes", "Content-Length": body.length });
    res.end(body);
  }
});
await new Promise((r) => server.listen(0, "127.0.0.1", r));
const base = `http://127.0.0.1:${server.address().port}`;

const chromePath = process.env.CHROME || "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const profile = await mkdtemp(join(tmpdir(), "savoia-promo-"));
const chrome = spawn(chromePath, [
  "--headless=new", "--remote-debugging-port=0", `--user-data-dir=${profile}`,
  "--hide-scrollbars", "--mute-audio", "--autoplay-policy=no-user-gesture-required",
  "--force-color-profile=srgb", "--font-render-hinting=none", `--window-size=${W},${H}`, "about:blank",
], { stdio: ["ignore", "ignore", "pipe"] });

const wsUrl = await new Promise((res, rej) => {
  let buf = "";
  chrome.stderr.on("data", (d) => { buf += d; const m = /DevTools listening on (ws:\/\/\S+)/.exec(buf); if (m) res(m[1]); });
  chrome.on("exit", (c) => rej(new Error(`Chrome exited (${c}) before DevTools was ready`)));
});

// Minimal CDP client.
const ws = new WebSocket(wsUrl);
await new Promise((r, j) => { ws.onopen = r; ws.onerror = j; });
let nextId = 0;
const pending = new Map();
ws.onmessage = (e) => {
  const msg = JSON.parse(e.data);
  if (msg.id && pending.has(msg.id)) {
    const { res, rej } = pending.get(msg.id);
    pending.delete(msg.id);
    msg.error ? rej(new Error(msg.error.message)) : res(msg.result);
  }
};
const send = (method, params = {}, sessionId) => new Promise((res, rej) => {
  const id = ++nextId;
  pending.set(id, { res, rej });
  ws.send(JSON.stringify({ id, method, params, sessionId }));
});

const { targetId } = await send("Target.createTarget", { url: "about:blank" });
const { sessionId } = await send("Target.attachToTarget", { targetId, flatten: true });
const page = (m, p) => send(m, p, sessionId);
await page("Emulation.setDeviceMetricsOverride", { width: W, height: H, deviceScaleFactor: 1, mobile: false });
await page("Page.enable");
await page("Page.navigate", { url: `${base}/scripts/promo-video/scene.html` });

async function evaluate(expression) {
  const r = await page("Runtime.evaluate", { expression, awaitPromise: true, returnByValue: true });
  if (r.exceptionDetails) throw new Error(r.exceptionDetails.exception?.description || r.exceptionDetails.text);
  return r.result.value;
}
for (let i = 0; i < 100 && !(await evaluate("typeof window.ready === 'object'")); i++) await new Promise((r) => setTimeout(r, 100));
const duration = await evaluate("window.ready");

async function frame(t) {
  await evaluate(`window.renderFrame(${t})`);
  const { data } = await page("Page.captureScreenshot", { format: "png", clip: { x: 0, y: 0, width: W, height: H, scale: 1 } });
  return Buffer.from(data, "base64");
}

try {
  await mkdir(dirname(opts.out), { recursive: true });
  if (opts.still) {
    const file = opts.out.replace(/\.mp4$/, `-${opts.still}s.png`);
    await writeFile(file, await frame(Number(opts.still)));
    console.log(file);
  } else {
    const fps = Number(opts.fps);
    const from = Number(opts.from ?? 0), to = Number(opts.to ?? duration);
    const total = Math.round((to - from) * fps);
    const ff = spawn("ffmpeg", [
      "-y", "-v", "error", "-f", "image2pipe", "-c:v", "png", "-framerate", String(fps), "-i", "-",
      "-c:v", "libx264", "-preset", "slow", "-crf", "16", "-pix_fmt", "yuv420p",
      "-color_primaries", "bt709", "-color_trc", "bt709", "-colorspace", "bt709",
      "-movflags", "+faststart", opts.out,
    ], { stdio: ["pipe", "inherit", "inherit"] });
    const done = new Promise((r, j) => ff.on("exit", (c) => (c === 0 ? r() : j(new Error(`ffmpeg exited ${c}`)))));
    const started = Date.now();
    for (let i = 0; i < total; i++) {
      const png = await frame(from + i / fps);
      if (!ff.stdin.write(png)) await new Promise((r) => ff.stdin.once("drain", r));
      if (i % fps === 0) process.stdout.write(`\r${i}/${total} frames, ${((Date.now() - started) / 1000).toFixed(0)} s`);
    }
    ff.stdin.end();
    await done;
    console.log(`\n${opts.out}`);
    // soundtrack.mjs builds the score and muxes it into the default output file.
    if (!opts["no-audio"] && opts.out === join(here, "out", "savoia-studio-promo.mp4")) {
      spawnSync(process.execPath, [join(here, "soundtrack.mjs")], { stdio: "inherit" });
    }
  }
} finally {
  ws.close();
  chrome.kill();
  server.close();
  await rm(profile, { recursive: true, force: true }).catch(() => {});
}
