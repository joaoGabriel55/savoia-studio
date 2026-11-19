#!/usr/bin/env node
// Before a release: which version we are on, which one we are about to
// release, and whether the pending changesets match what actually changed.
//
// The release itself still comes from Changesets (docs/RELEASING.md). This
// script reads the commits since the last tag, guesses each one's bump from
// its message and the files it touched, and compares that guess with the
// bumps the changesets declare.
//
//   npm run release:check              # report
//   npm run release:check -- --json    # machine-readable
//   npm run release:check -- --since v0.1.0
//
// Exits 1 when the versions disagree, or when user-facing commits are waiting
// with no changeset (or a smaller bump than the commits suggest).
import { execFileSync } from "node:child_process";
import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

export const LEVELS = ["none", "chore", "patch", "minor", "major"];
const rank = (level) => LEVELS.indexOf(level);
export const maxLevel = (...levels) =>
  levels.reduce((a, b) => (rank(b) > rank(a) ? b : a), "none");

// Files that never reach a user's machine. Anything else under crates/ or
// assets/ ends up in the app or its installers.
const INTERNAL_PATHS = [
  /^docs\//,
  /^\.github\//,
  /^\.changeset\//,
  /^scripts\//,
  /^samples\//,
  /^\.claude\//,
  /(^|\/)tests?\//,
  /(^|\/)benches\//,
  /_tests?\.rs$/,
  /\.md$/,
  /^package(-lock)?\.json$/,
  /^skills-lock\.json$/,
  /^docker-compose\.yml$/,
  /^rust-toolchain\.toml$/,
  /^\.gitignore$|\/\.gitignore$/,
];
export const isUserFacingPath = (path) =>
  path !== "" && !INTERNAL_PATHS.some((re) => re.test(path));

const CONVENTIONAL = /^(\w+)(\([^)]*\))?(!)?:\s*/;
const CONVENTIONAL_LEVEL = {
  feat: "minor",
  fix: "patch",
  perf: "patch",
  revert: "patch",
  chore: "chore",
  ci: "chore",
  docs: "chore",
  build: "chore",
  test: "chore",
  refactor: "chore",
  style: "chore",
};

const BREAKING = [
  /BREAKING[ -]CHANGE/i,
  /\bbreaking\b/i,
  /\b(remove|drop)s? support\b/i,
  /\bno longer (supports?|reads?|opens?|accepts?)\b/i,
  /\bincompatible\b/i,
];
const FIX = [
  /^(fix|fixe[sd]|don'?t|do not|stop|keep|retry|handle|prevent|avoid|restore|correct|repair|guard|ignore|recover)\b/i,
  /\b(crash(es|ed)?|panics?|bug|regression|broken|wrong|missing|leaks?|hangs?|freezes?|race|typo)\b/i,
];
const FEATURE = [
  /^(add|adds|added|show|open|support|format|give|let|allow|introduce|import|export|enable|offer|create|new|list|preview|remember|run|save|search|sort|filter)\b/i,
  /\bnew\b/i,
];
const RELEASE = /^(Release Savoia Studio|Version Packages|chore\(release\))/i;

// One commit's guess. `files` are the paths it touched.
export function classifyCommit(subject, body = "", files = []) {
  const text = `${subject}\n${body}`;
  if (RELEASE.test(subject)) return { level: "none", reason: "release commit" };

  const conv = subject.match(CONVENTIONAL);
  if (conv?.[3] || /BREAKING[ -]CHANGE/.test(body)) {
    return { level: "major", reason: "marked breaking" };
  }

  const userFacing = files.filter(isUserFacingPath);
  if (files.length > 0 && userFacing.length === 0) {
    return { level: "chore", reason: "touches no shipped files" };
  }

  if (conv && CONVENTIONAL_LEVEL[conv[1].toLowerCase()]) {
    const level = CONVENTIONAL_LEVEL[conv[1].toLowerCase()];
    // A "chore" that changes shipped code still ships: count it as a patch.
    if (level === "chore" && userFacing.length > 0) {
      return { level: "patch", reason: `${conv[1]}: but changes shipped files` };
    }
    return { level, reason: `${conv[1]}:` };
  }

  const plain = conv ? subject.slice(conv[0].length) : subject;
  if (BREAKING.some((re) => re.test(text))) {
    return { level: "major", reason: "breaking wording" };
  }
  // Fix wording wins over feature verbs: "Show X again" after a regression
  // would read as a feature, "Don't panic on X" never reads as one.
  if (FIX.some((re) => re.test(plain))) return { level: "patch", reason: "fix wording" };
  if (FEATURE.some((re) => re.test(plain))) return { level: "minor", reason: "feature wording" };
  return { level: "patch", reason: "changes shipped files" };
}

export function bumpVersion(version, level) {
  const [major, minor, patch] = version.split(/[.-]/).map(Number);
  switch (level) {
    case "major":
      return `${major + 1}.0.0`;
    case "minor":
      return `${major}.${minor + 1}.0`;
    case "patch":
      return `${major}.${minor}.${patch + 1}`;
    default:
      return version;
  }
}

// `"savoia-studio": minor` lines in a changeset's frontmatter.
export function parseChangeset(text) {
  const front = text.match(/^---\n([\s\S]*?)\n---/);
  if (!front) return null;
  const levels = [...front[1].matchAll(/^\s*["']?[^"':]+["']?\s*:\s*(major|minor|patch)\s*$/gm)].map(
    (m) => m[1],
  );
  if (levels.length === 0) return null;
  const summary = text.slice(front[0].length).trim().split("\n")[0];
  return { level: maxLevel(...levels), summary };
}

// ---------------------------------------------------------------------------

const ROOT = join(fileURLToPath(import.meta.url), "..", "..");
const git = (...args) =>
  execFileSync("git", args, { cwd: ROOT, encoding: "utf8", stdio: ["ignore", "pipe", "ignore"] }).trim();
const tryGit = (...args) => {
  try {
    return git(...args);
  } catch {
    return null;
  }
};

function readVersions() {
  const pkg = JSON.parse(readFileSync(join(ROOT, "package.json"), "utf8")).version;
  const cargoToml = readFileSync(join(ROOT, "Cargo.toml"), "utf8");
  const cargo = cargoToml.match(/\[workspace\.package\][^[]*?\nversion\s*=\s*"([^"]+)"/)?.[1] ?? null;
  const lock = readFileSync(join(ROOT, "Cargo.lock"), "utf8");
  const lockVersion =
    lock.match(/\[\[package\]\]\nname = "savoia-app"\nversion = "([^"]+)"/)?.[1] ?? null;
  const lastTag = tryGit("describe", "--tags", "--abbrev=0", "--match", "v[0-9]*");
  const remote = tryGit("ls-remote", "--tags", "origin", `refs/tags/v${pkg}`);
  const remoteTag = remote === null ? "unknown" : remote !== "";
  return { pkg, cargo, lockVersion, lastTag, remoteTag };
}

function readChangesets() {
  const dir = join(ROOT, ".changeset");
  return readdirSync(dir)
    .filter((f) => f.endsWith(".md") && f !== "README.md")
    .map((file) => ({ file, ...parseChangeset(readFileSync(join(dir, file), "utf8")) }))
    .filter((c) => c.level);
}

function readCommits(since) {
  const range = since ? `${since}..HEAD` : "HEAD";
  const sep = "\x1e";
  const log = git("log", range, "--no-merges", `--format=${sep}%h%x1f%s%x1f%b%x1f`, "--name-only");
  return log
    .split(sep)
    .filter(Boolean)
    .map((chunk) => {
      const [hash, subject, body, rest] = chunk.split("\x1f");
      const files = rest.split("\n").map((l) => l.trim()).filter(Boolean);
      return { hash, subject, files, ...classifyCommit(subject, body, files) };
    });
}

export function analyze({ versions, changesets, commits }) {
  const problems = [];
  const { pkg, cargo, lockVersion, lastTag } = versions;

  if (cargo !== pkg) problems.push(`Cargo.toml says ${cargo}, package.json says ${pkg}: run node scripts/sync-version.mjs`);
  if (lockVersion && lockVersion !== pkg) problems.push(`Cargo.lock has savoia-app ${lockVersion}, package.json says ${pkg}`);

  const tagged = lastTag === `v${pkg}`;
  const declared = maxLevel(...changesets.map((c) => c.level));
  const detected = maxLevel(...commits.map((c) => c.level));
  const shipping = commits.filter((c) => rank(c.level) >= rank("patch"));

  // package.json ahead of the last tag: the Release PR was merged and the
  // tag hasn't been pushed yet, so this version is what goes out next.
  let current = pkg;
  let next;
  let state;
  if (!tagged && changesets.length === 0) {
    state = "awaiting-tag";
    current = lastTag ? lastTag.replace(/^v/, "") : "none";
    next = pkg;
  } else if (changesets.length > 0) {
    state = "pending-changesets";
    next = bumpVersion(pkg, declared);
  } else if (shipping.length > 0) {
    state = "missing-changesets";
    next = bumpVersion(pkg, detected);
  } else {
    state = "nothing-to-release";
    next = pkg;
  }

  if (!tagged && changesets.length > 0 && lastTag) {
    problems.push(`package.json is ${pkg} but the last tag is ${lastTag}: tag ${pkg} before releasing more`);
  }
  if (state === "missing-changesets") {
    problems.push(`${shipping.length} commit(s) change shipped files but there is no changeset: run npx changeset (${detected})`);
  } else if (state === "pending-changesets" && rank(detected) > rank(declared)) {
    const culprits = commits.filter((c) => rank(c.level) > rank(declared)).map((c) => c.hash);
    problems.push(
      `changesets declare ${declared} but commits look like ${detected} (${culprits.join(", ")}): ` +
        `a changeset may be missing or too small, which would release ${bumpVersion(pkg, declared)} instead of ${bumpVersion(pkg, detected)}`,
    );
  }

  const releaseLevel = state === "pending-changesets" ? declared : state === "missing-changesets" ? detected : "none";
  return { current, next, state, declared, detected, releaseLevel, problems };
}

const STATE_TEXT = {
  "awaiting-tag": "the Release PR is merged; tagging this version starts the build",
  "pending-changesets": "merging the Release PR will release this",
  "missing-changesets": "only if someone adds a changeset; none exist yet",
  "nothing-to-release": "nothing user-facing since the last tag",
};

function printReport({ versions, changesets, commits, result, since }) {
  const color = process.stdout.isTTY && !process.env.NO_COLOR;
  const c = (code) => (s) => (color ? `\x1b[${code}m${s}\x1b[0m` : s);
  const bold = c(1), dim = c(2), red = c(31), green = c(32), yellow = c(33);
  const tint = { major: red, minor: yellow, patch: green, chore: dim, none: dim };
  const tag = (level) => tint[level](level.padEnd(5));

  console.log(bold("Versions"));
  console.log(`  package.json  ${versions.pkg}`);
  console.log(`  Cargo.toml    ${versions.cargo ?? red("missing")}`);
  console.log(`  Cargo.lock    ${versions.lockVersion ?? dim("savoia-app not found")}`);
  console.log(`  last tag      ${versions.lastTag ?? dim("none")}`);
  console.log(`  v${versions.pkg} on origin  ${versions.remoteTag === "unknown" ? dim("unknown (offline?)") : versions.remoteTag ? "yes" : "no"}`);

  console.log(`\n${bold("Changesets")} ${dim(`(${changesets.length})`)}`);
  if (changesets.length === 0) console.log(dim("  none"));
  for (const cs of changesets) console.log(`  ${tag(cs.level)} ${cs.file}  ${dim(cs.summary.slice(0, 70))}`);

  console.log(`\n${bold("Commits since")} ${since ?? "the beginning"} ${dim(`(${commits.length})`)}`);
  if (commits.length === 0) console.log(dim("  none"));
  for (const cm of commits) console.log(`  ${tag(cm.level)} ${dim(cm.hash)} ${cm.subject}  ${dim(`· ${cm.reason}`)}`);

  console.log(`\n${bold("Release")}`);
  console.log(`  changesets declare  ${tag(result.declared)}`);
  console.log(`  commits suggest     ${tag(result.detected)}`);
  const arrow = result.current === result.next ? dim("(no change)") : `→ ${bold(result.next)}`;
  console.log(`  ${bold(result.current)} ${arrow}  ${dim(STATE_TEXT[result.state])}`);

  if (result.problems.length > 0) {
    console.log(`\n${red(bold("Check before releasing"))}`);
    for (const p of result.problems) console.log(`  ${red("•")} ${p}`);
  } else {
    console.log(`\n${green("✓")} versions agree and the changesets cover the commits`);
  }
}

function main(argv) {
  const json = argv.includes("--json");
  const sinceIdx = argv.indexOf("--since");
  const versions = readVersions();
  const since = sinceIdx >= 0 ? argv[sinceIdx + 1] : versions.lastTag;
  const changesets = readChangesets();
  const commits = readCommits(since);
  const result = analyze({ versions, changesets, commits });

  if (json) console.log(JSON.stringify({ since, versions, changesets, commits, ...result }, null, 2));
  else printReport({ versions, changesets, commits, result, since });
  process.exitCode = result.problems.length > 0 ? 1 : 0;
}

if (process.argv[1] === fileURLToPath(import.meta.url)) main(process.argv.slice(2));
