#!/usr/bin/env node
// Synthesizes the promo's soundtrack and muxes it into the rendered video.
// Everything is generated here (no samples, no licensed music): JavaScript
// renders a dry stem, a reverb-send stem and a reverb impulse response;
// ffmpeg convolves the send, mixes, and masters to -14 LUFS.
//
//   node scripts/promo-video/soundtrack.mjs            # build out/soundtrack.wav and mux it into the video
//   node scripts/promo-video/soundtrack.mjs --no-mux   # only build the WAV
//
// Cue times mirror the timeline in scene.html; change them together.

import { spawnSync } from "node:child_process";
import { writeFileSync, mkdirSync, existsSync, renameSync, mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const outDir = join(here, "out");
const SR = 48000;
const DUR = 47;
const N = Math.ceil(DUR * SR);

// ---------------------------------------------------------------- utilities

let seed = 0x5a701a;
const rand = () => { seed = (Math.imul(seed, 1664525) + 1013904223) >>> 0; return seed / 4294967296; };
const noise = () => rand() * 2 - 1;
const mtof = (m) => 440 * Math.pow(2, (m - 69) / 12);
const clamp = (x, a = 0, b = 1) => Math.min(b, Math.max(a, x));
const TAU = Math.PI * 2;

const bus = () => [new Float32Array(N), new Float32Array(N)];
const dry = bus(), wet = bus();

// Writes a mono buffer at time t (seconds) with equal-power pan (-1..1) into one or both buses.
function place(buf, t, { gain = 1, pan = 0, send = 0, dryGain = 1, panFn } = {}) {
  const start = Math.round(t * SR);
  for (let i = 0; i < buf.length; i++) {
    const j = start + i;
    if (j < 0) continue;
    if (j >= N) break;
    const p = panFn ? panFn(i / buf.length) : pan;
    const a = (p + 1) * Math.PI / 4;
    const l = Math.cos(a) * buf[i] * gain, r = Math.sin(a) * buf[i] * gain;
    dry[0][j] += l * dryGain; dry[1][j] += r * dryGain;
    if (send) { wet[0][j] += l * send; wet[1][j] += r * send; }
  }
}

// Zero-delay-feedback state-variable filter. cutoff may be a function of the sample index.
function svf(input, cutoff, q = 0.707, mode = "lp") {
  const out = new Float32Array(input.length);
  const k = 1 / q;
  let ic1 = 0, ic2 = 0, a1 = 0, a2 = 0, a3 = 0;
  for (let i = 0; i < input.length; i++) {
    if ((i & 15) === 0) {
      const fc = Math.min(typeof cutoff === "function" ? cutoff(i) : cutoff, SR * 0.45);
      const g = Math.tan(Math.PI * fc / SR);
      a1 = 1 / (1 + g * (g + k)); a2 = g * a1; a3 = g * a2;
    }
    const v0 = input[i], v3 = v0 - ic2;
    const v1 = a1 * ic1 + a2 * v3, v2 = ic2 + a2 * ic1 + a3 * v3;
    ic1 = 2 * v1 - ic1; ic2 = 2 * v2 - ic2;
    out[i] = mode === "lp" ? v2 : mode === "bp" ? v1 : v0 - k * v1 - v2;
  }
  return out;
}

const blep = (t, dt) => {
  if (t < dt) { t /= dt; return t + t - t * t - 1; }
  if (t > 1 - dt) { t = (t - 1) / dt; return t * t + t + t + 1; }
  return 0;
};
function saw(freq, len, detune = 0) {
  const out = new Float32Array(len);
  const dt = freq * Math.pow(2, detune / 1200) / SR;
  let ph = rand();
  for (let i = 0; i < len; i++) { out[i] = 2 * ph - 1 - blep(ph, dt); ph += dt; if (ph >= 1) ph -= 1; }
  return out;
}

// ---------------------------------------------------------------- instruments

function kick(gain = 1) {
  const len = Math.round(0.55 * SR), out = new Float32Array(len);
  let ph = 0;
  for (let i = 0; i < len; i++) {
    const t = i / SR;
    ph += (46 + 110 * Math.exp(-t * 30)) / SR;
    out[i] = Math.tanh(1.6 * Math.sin(TAU * ph) * Math.exp(-t * 6.5)) + noise() * 0.25 * Math.exp(-t * 500);
  }
  return out.map((x) => x * gain);
}

function clap() {
  const len = Math.round(0.35 * SR), src = new Float32Array(len);
  for (let i = 0; i < len; i++) {
    const t = i / SR;
    const bursts = [0, 0.011, 0.022].reduce((s, b) => s + (t >= b ? Math.exp(-(t - b) * 160) : 0), 0);
    src[i] = noise() * (bursts * 0.6 + Math.exp(-t * 16) * 0.5);
  }
  return svf(src, 1400, 1.2, "bp");
}

function hat(decay = 45) {
  const len = Math.round(0.12 * SR), src = new Float32Array(len);
  for (let i = 0; i < len; i++) src[i] = noise() * Math.exp(-(i / SR) * decay);
  return svf(src, 8000, 0.8, "hp");
}

function tick(freq = 2600) {
  const len = Math.round(0.05 * SR), out = new Float32Array(len);
  for (let i = 0; i < len; i++) {
    const t = i / SR;
    out[i] = Math.sin(TAU * freq * t) * Math.exp(-t * 170) * 0.6 + noise() * Math.exp(-t * 1200) * 0.35;
  }
  return out;
}

function tap(freq = 700) {
  const len = Math.round(0.12 * SR), out = new Float32Array(len);
  let ph = 0;
  for (let i = 0; i < len; i++) {
    const t = i / SR;
    ph += freq * (1 + 0.5 * Math.exp(-t * 90)) / SR;
    out[i] = Math.sin(TAU * ph) * Math.exp(-t * 55);
  }
  return out;
}

function impact(len = 2.6) {
  const n = Math.round(len * SR), out = new Float32Array(n), nz = new Float32Array(n);
  let ph = 0;
  for (let i = 0; i < n; i++) {
    const t = i / SR;
    ph += (30 + 60 * Math.exp(-t * 5)) / SR;
    out[i] = Math.tanh(1.4 * Math.sin(TAU * ph)) * Math.exp(-t * 1.9);
    nz[i] = noise() * Math.exp(-t * 7);
  }
  const body = svf(nz, (i) => 300 + 2200 * Math.exp(-(i / SR) * 9), 0.7);
  return out.map((x, i) => x * 0.9 + body[i] * 0.5);
}

// Filtered noise that climbs into a hit at its end.
function riser(len, { lo = 250, hi = 7000, q = 3 } = {}) {
  const n = Math.round(len * SR), src = new Float32Array(n);
  for (let i = 0; i < n; i++) src[i] = noise();
  const f = svf(src, (i) => lo * Math.pow(hi / lo, Math.pow(i / n, 1.6)), q, "bp");
  const tone = new Float32Array(n);
  let ph = 0;
  for (let i = 0; i < n; i++) { const p = i / n; ph += (110 * Math.pow(8, p * p)) / SR; tone[i] = Math.sin(TAU * ph) * 0.25; }
  return f.map((x, i) => { const p = i / n; return (x + tone[i]) * Math.pow(p, 2.2) * Math.min(1, (n - i) / (0.01 * SR)); });
}

function whoosh(len = 0.7, lo = 350, hi = 2600) {
  const n = Math.round(len * SR), src = new Float32Array(n);
  for (let i = 0; i < n; i++) src[i] = noise();
  const f = svf(src, (i) => lo + (hi - lo) * Math.sin(Math.PI * i / n), 1.6, "bp");
  return f.map((x, i) => x * Math.pow(Math.sin(Math.PI * i / n), 2));
}

function reverseCymbal(len = 1.3) {
  const n = Math.round(len * SR), src = new Float32Array(n);
  for (let i = 0; i < n; i++) src[i] = noise() * Math.pow(i / n, 4);
  return svf(src, 6000, 0.7, "hp");
}

// Two-operator FM bell.
function bell(freq, len = 4, ratio = 3.5, index = 2.6) {
  const n = Math.round(len * SR), out = new Float32Array(n);
  for (let i = 0; i < n; i++) {
    const t = i / SR;
    const mod = Math.sin(TAU * freq * ratio * t) * index * Math.exp(-t * 2.5);
    const env = Math.min(1, t / 0.003) * Math.exp(-t * 1.3);
    out[i] = (Math.sin(TAU * freq * t + mod) + 0.3 * Math.sin(TAU * freq * 2.01 * t) * Math.exp(-t * 3)) * env;
  }
  return out;
}

function pluck(freq, len = 0.7) {
  const n = Math.round(len * SR);
  const a = saw(freq, n, -6), b = saw(freq, n, 6);
  const mix = a.map((x, i) => (x + b[i]) * 0.5);
  return svf(mix, (i) => 220 + 3800 * Math.exp(-(i / SR) * 16), 0.9).map((x, i) => x * Math.exp(-(i / SR) * 5.5) * Math.min(1, i / 60));
}

// Detuned-saw pad over [t0, t1) with a slow filter and its own envelope; duck() may shape the level.
function pad(notes, t0, t1, { gain = 0.08, attack = 1.2, release = 1.6, cutLo = 500, cutHi = 2200, cutRise = 2, duck } = {}) {
  const len = Math.round((t1 - t0 + release) * SR), hold = (t1 - t0) * SR;
  notes.forEach((m, v) => {
    [-9, 0, 9].forEach((cents, o) => {
      const raw = saw(mtof(m), len, cents + (v - 2) * 1.5);
      const f = svf(raw, (i) => cutLo + (cutHi - cutLo) * clamp(i / (cutRise * SR)), 0.8);
      for (let i = 0; i < len; i++) {
        const env = Math.min(1, i / (attack * SR)) * (i < hold ? 1 : Math.max(0, 1 - (i - hold) / (release * SR)));
        f[i] *= env * (duck ? duck(t0 + i / SR) : 1);
      }
      place(f, t0, { gain, pan: [-0.7, 0, 0.7][o] * (0.6 + 0.1 * v), send: 0.9, dryGain: 0.55 });
    });
  });
}

// ---------------------------------------------------------------- arrangement

const AM9 = [57, 60, 64, 67, 71], FMAJ9 = [53, 57, 60, 64, 67], CADD9 = [55, 60, 62, 64, 67], G69 = [55, 59, 62, 64, 69];
const PROG = [[AM9, 33], [FMAJ9, 29], [CADD9, 36], [G69, 31]];

const T0 = 10.6, BEAT = 0.5, BAR = 2, END = 40.6; // the groove: 120 bpm from the "no webview" drop to the call to action
const kicks = [];
for (let t = T0; t < END - 0.01; t += BEAT) kicks.push(t);
const duck = (t) => {
  if (t < T0 || t >= END) return 1;
  const last = T0 + Math.floor((t - T0) / BEAT) * BEAT;
  return 1 - 0.55 * Math.exp(-(t - last) * 10);
};

// Intro, 0 – 5.6: a low drone and a closed pad, opening up toward the reveal.
pad([52, ...AM9.slice(0, 3)], 0.1, 5.6, { gain: 0.05, attack: 2.5, release: 0.8, cutLo: 250, cutHi: 1300, cutRise: 5.5 });

// The question, word by word (scene.html: 0.25 + i * 0.09).
for (let i = 0; i < 8; i++) place(tap(620 + i * 40), 0.25 + i * 0.09, { gain: 0.12, pan: -0.5 + i * 0.14, send: 0.4 });

// The RAM counter scramble, 2.2 – 3.5: one tick per digit change (24 per second), then the landing on 69 MB.
for (let t = 2.2, i = 0; t < 3.5; t += 1 / 24, i++) place(tick(2200 + (i % 5) * 180), t, { gain: 0.22 * (0.6 + 0.4 * (1 - (t - 2.2) / 1.3)), pan: (i % 2 ? 0.25 : -0.25), send: 0.2 });
place(impact(), 3.5, { gain: 0.38, send: 0.35 });
place(bell(mtof(81), 4), 3.5, { gain: 0.12, pan: -0.2, send: 0.8 });
place(bell(mtof(88), 4), 3.52, { gain: 0.07, pan: 0.3, send: 0.8 });
place(riser(1.0), 4.6, { gain: 0.16, send: 0.5, panFn: (p) => -0.6 + 1.2 * p });

// Logo, 5.6 – 10.6: a bright chord blooms, bells trace it, the wordmark wipes in with a breath.
pad([48, ...FMAJ9], 5.6, 10.6, { gain: 0.065, attack: 0.6, release: 0.4, cutLo: 900, cutHi: 3200, cutRise: 1.5 });
[77, 81, 84, 88].forEach((m, i) => place(bell(mtof(m), 3.5), 5.7 + i * 0.09, { gain: 0.08, pan: -0.45 + i * 0.3, send: 0.9 }));
place(whoosh(0.9, 500, 3500), 7.0, { gain: 0.16, send: 0.5, panFn: (p) => -0.8 + 1.6 * p });
place(riser(1.6, { lo: 180, hi: 9000 }), 9.0, { gain: 0.2, send: 0.4 });
place(reverseCymbal(1.4), 9.2, { gain: 0.12, send: 0.3 });

// The groove, 10.6 – 40.6.
place(impact(2.2), T0, { gain: 0.36, send: 0.3 });
kicks.forEach((t) => place(kick(), t, { gain: 0.36 }));
for (let t = T0 + BEAT / 2; t < END; t += BEAT) place(hat(), t, { gain: 0.11, pan: 0.25, send: 0.1 });
for (let t = 22; t < END - 1; t += BEAT * 2) place(clap(), t + BEAT, { gain: 0.32, pan: -0.05, send: 0.35 });
for (let t = 22; t < END - 1; t += BEAT / 2) place(hat(80), t + BEAT / 4, { gain: 0.045, pan: -0.35, send: 0.05 });
// Snare roll into the call to action.
for (let t = END - 1, k = 0; t < END - 0.01; k++) {
  const p = (t - (END - 1)) / 1;
  place(clap(), t, { gain: 0.08 + 0.2 * p, send: 0.3 });
  t += p < 0.5 ? BEAT / 4 : BEAT / 8;
}

for (let b = 0, t = T0; t < END - 0.01; b++, t += BAR) {
  const [chord, root] = PROG[b % 4];
  pad(chord, t, Math.min(t + BAR, END), { gain: 0.05, attack: 0.05, release: 0.5, cutLo: 1400, cutHi: 1800, cutRise: 1, duck });
  // Bass: a sub sine plus a filtered saw an octave up, pumping in eighths.
  for (let e = 0; e < 4 * 2; e++) {
    const s = t + e * BEAT / 2;
    if (s >= END) break;
    const n = Math.round(0.24 * SR), sub = new Float32Array(n);
    for (let i = 0; i < n; i++) {
      const tt = i / SR, env = Math.min(1, i / 200) * Math.exp(-tt * 4) * Math.min(1, (n - i) / 400);
      sub[i] = Math.tanh(1.3 * Math.sin(TAU * mtof(root) * tt)) * env * duck(s + tt);
    }
    const mid = svf(saw(mtof(root + 12), n), 700, 1.1).map((x, i) => x * sub[i] * 0.9);
    place(sub, s, { gain: 0.19 });
    place(mid, s, { gain: 0.16 });
  }
  // Plucked arpeggio from the numbers onward.
  if (t >= 14.6) {
    const pattern = [0, 2, 4, 1, 3, 2, 4, 3, 0, 2, 4, 1, 3, 4, 2, 1];
    pattern.forEach((idx, k) => {
      const s = t + k * BEAT / 4;
      if (s < 15.6 || s >= END) return;
      place(pluck(mtof(chord[idx] + 12)), s, { gain: (k % 4 === 0 ? 0.075 : 0.05) * duck(s), pan: k % 2 ? 0.45 : -0.45, send: 0.7 });
    });
  }
}

// Strikes through Webview, JVM, Electron (scene.html: 11.7 + i * 0.22).
[0, 1, 2].forEach((i) => {
  const t = 11.7 + i * 0.22;
  place(whoosh(0.35, 1500, 6000), t - 0.05, { gain: 0.16, pan: -0.6 + i * 0.6, send: 0.3 });
  place(tap(180), t + 0.1, { gain: 0.3, pan: -0.4 + i * 0.4 });
});
place(bell(mtof(76), 3), 12.9, { gain: 0.08, send: 0.9 });

// The three numbers count up (scene.html: at = 16.2 + i * 0.3, counting over at+0.1 .. at+1.6 with outExpo).
const outExpo = (k) => (k >= 1 ? 1 : 1 - Math.pow(2, -10 * k));
[[0, 69], [120, 0], [0, 205]].forEach(([from, to], i) => {
  const at = [16.2, 16.8, 16.5][i] + 0.1;
  let shown = from, last = -1;
  for (let f = 0; f <= 1.5 * 60; f++) {
    const t = at + f / 60;
    const v = Math.round(from + (to - from) * outExpo(clamp((t - at) / 1.5)));
    if (v !== shown && t - last > 1 / 28) { place(tick(1800 + i * 450), t, { gain: 0.1, pan: -0.6 + i * 0.6, send: 0.2 }); last = t; }
    shown = v;
  }
  place(bell(mtof([84, 88, 91][i]), 2.5), at + 1.2, { gain: 0.05, pan: -0.6 + i * 0.6, send: 0.9 });
});

// Product cuts.
place(whoosh(0.9, 300, 2000), 15.0, { gain: 0.16, send: 0.4, panFn: (p) => 0.7 - 1.4 * p });
place(whoosh(1.0, 200, 1600), 21.5, { gain: 0.2, send: 0.4, panFn: (p) => -0.7 + 1.4 * p });
place(whoosh(1.1, 150, 1200), 22.1, { gain: 0.16, send: 0.3 });
place(whoosh(0.8, 400, 2800), 25.5, { gain: 0.18, send: 0.4, panFn: (p) => 0.6 - 1.2 * p });
place(whoosh(0.8, 400, 2800), 30.5, { gain: 0.18, send: 0.4, panFn: (p) => -0.6 + 1.2 * p });
place(whoosh(0.8, 300, 2200), 35.5, { gain: 0.16, send: 0.4 });

// Engines: two confident notes for what ships today, soft taps for what is coming.
place(bell(mtof(72), 3, 1.4, 1.8), 36.7, { gain: 0.11, pan: -0.35, send: 0.8 });
place(bell(mtof(76), 3, 1.4, 1.8), 36.9, { gain: 0.11, pan: 0.35, send: 0.8 });
for (let i = 0; i < 5; i++) place(tap(900 + i * 60), 37.9 + i * 0.12, { gain: 0.06, pan: -0.6 + i * 0.3, send: 0.5 });
place(riser(1.2, { lo: 300, hi: 9000 }), 39.4, { gain: 0.18, send: 0.4 });

// Call to action, 40.6 – 47: the groove stops, the chord opens, resolves and fades.
place(impact(3), END, { gain: 0.36, send: 0.45 });
pad([48, ...FMAJ9], END, 43.4, { gain: 0.07, attack: 0.15, release: 1.2, cutLo: 3200, cutHi: 1600, cutRise: 2.8 });
pad([43, ...CADD9, 72], 43.2, 45.8, { gain: 0.06, attack: 0.9, release: 1.2, cutLo: 1200, cutHi: 2200, cutRise: 2 });
[77, 81, 84, 91].forEach((m, i) => place(bell(mtof(m), 4.5), 40.7 + i * 0.07, { gain: 0.08, pan: -0.45 + i * 0.3, send: 0.9 }));
[84, 88, 91].forEach((m, i) => place(bell(mtof(m), 3.5), 43.25 + i * 0.1, { gain: 0.05, pan: -0.3 + i * 0.3, send: 0.9 }));

// Fade with the picture (scene.html: 46.2 – 47).
for (const b of [dry, wet]) for (const ch of b) for (let i = Math.round(45.6 * SR); i < N; i++) ch[i] *= Math.max(0, 1 - (i / SR - 45.6) / 1.4);

// ---------------------------------------------------------------- reverb IR and output

function ir(len = 3.2) {
  const n = Math.round(len * SR), pre = Math.round(0.022 * SR);
  return [0, 1].map(() => {
    const src = new Float32Array(n);
    for (let i = pre; i < n; i++) src[i] = noise() * Math.exp(-6.9 * (i - pre) / (n - pre));
    // Darken the tail: a lowpass whose cutoff falls over the decay.
    return svf(src, (i) => 9000 * Math.exp(-(i / n) * 2.6) + 600, 0.6);
  });
}

function wav(path, channels) {
  const n = channels[0].length, data = Buffer.alloc(n * channels.length * 4);
  for (let i = 0; i < n; i++) for (let c = 0; c < channels.length; c++) data.writeFloatLE(channels[c][i], (i * channels.length + c) * 4);
  const h = Buffer.alloc(44);
  h.write("RIFF", 0); h.writeUInt32LE(36 + data.length, 4); h.write("WAVE", 8);
  h.write("fmt ", 12); h.writeUInt32LE(16, 16); h.writeUInt16LE(3, 20); h.writeUInt16LE(channels.length, 22);
  h.writeUInt32LE(SR, 24); h.writeUInt32LE(SR * channels.length * 4, 28); h.writeUInt16LE(channels.length * 4, 32); h.writeUInt16LE(32, 34);
  h.write("data", 36); h.writeUInt32LE(data.length, 40);
  writeFileSync(path, Buffer.concat([h, data]));
}

function ffmpeg(args) {
  const r = spawnSync("ffmpeg", ["-y", "-v", "error", ...args], { stdio: "inherit" });
  if (r.status !== 0) throw new Error(`ffmpeg failed: ${args.join(" ")}`);
}

mkdirSync(outDir, { recursive: true });
const work = mkdtempSync(join(tmpdir(), "savoia-sound-"));
try {
  wav(join(work, "dry.wav"), dry);
  wav(join(work, "wet.wav"), wet);
  wav(join(work, "ir.wav"), ir());
  // Mix, then master in two passes: measure loudness, apply one static gain to
  // -14 LUFS and let a limiter catch the peaks. A single dynamic loudnorm pass
  // would flatten the quiet intro up to the level of the drop.
  const mix = join(work, "mix.wav");
  ffmpeg([
    "-i", join(work, "dry.wav"), "-i", join(work, "wet.wav"), "-i", join(work, "ir.wav"),
    "-filter_complex",
    "[1:a][2:a]afir=dry=1:wet=1[rev];" +
    "[0:a][rev]amix=inputs=2:normalize=0:weights='1 0.45'," +
    "highpass=f=32,equalizer=f=70:t=h:w=80:g=-3,equalizer=f=3000:t=q:w=1.2:g=1.5,acompressor=threshold=-14dB:ratio=2.5:attack=15:release=180:makeup=1,atrim=0:" + DUR + "[out]",
    "-map", "[out]", "-c:a", "pcm_f32le", mix,
  ]);
  const probe = spawnSync("ffmpeg", ["-hide_banner", "-i", mix, "-af", "loudnorm=print_format=json", "-f", "null", "-"], { encoding: "utf8" });
  const measured = JSON.parse(/\{[^{}]*"input_i"[^{}]*\}/.exec(probe.stderr)[0]);
  const gain = -14 - Number(measured.input_i);
  const soundtrack = join(outDir, "soundtrack.wav");
  ffmpeg(["-i", mix, "-af", `volume=${gain.toFixed(2)}dB,alimiter=limit=0.84:attack=2:release=60:level=disabled`, "-c:a", "pcm_s24le", soundtrack]);
  console.log(soundtrack);

  const video = join(outDir, "savoia-studio-promo.mp4");
  if (!process.argv.includes("--no-mux") && existsSync(video)) {
    const tmp = join(outDir, ".muxed.mp4");
    ffmpeg(["-i", video, "-i", soundtrack, "-map", "0:v:0", "-map", "1:a:0", "-c:v", "copy", "-c:a", "aac", "-b:a", "256k", "-shortest", "-movflags", "+faststart", tmp]);
    renameSync(tmp, video);
    console.log(video);
  }
} finally {
  rmSync(work, { recursive: true, force: true });
}
