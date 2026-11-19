import assert from "node:assert/strict";
import { test } from "node:test";
import { analyze, bumpVersion, classifyCommit, parseChangeset } from "./release-check.mjs";

const app = ["crates/savoia-app/src/console.rs"];
const level = (subject, files = app, body = "") => classifyCommit(subject, body, files).level;

test("commits that touch no shipped files are chores", () => {
  assert.equal(level("Build the Apple silicon release on macos-15", [".github/workflows/release.yml"]), "chore");
  assert.equal(level("Add a script that renders the promo video", ["scripts/promo-video/render.mjs", "package.json"]), "chore");
  assert.equal(level("Add tests", ["crates/savoia-app/src/ui_tests.rs", "crates/savoia-pg/tests/live.rs"]), "chore");
});

test("fix wording on shipped files is a patch", () => {
  assert.equal(level("Don't panic on accented text in block comments"), "patch");
  assert.equal(level("Retry tool probes while the executable is text-file-busy"), "patch");
  assert.equal(level("Show the grid again after a crash"), "patch");
});

test("feature wording on shipped files is a minor", () => {
  assert.equal(level("Format SQL with Ctrl+Shift+F"), "minor");
  assert.equal(level("Open a table's data on double-click"), "minor");
});

test("breaking changes are major", () => {
  assert.equal(level("Drop support for MySQL 5.7"), "major");
  assert.equal(level("feat!: new connection file format"), "major");
  assert.equal(level("Rework the store", app, "BREAKING CHANGE: old files are not read"), "major");
});

test("conventional prefixes are honoured, but a chore on shipped code ships", () => {
  assert.equal(level("feat: add SQLite"), "minor");
  assert.equal(level("fix(pg): read $ in names"), "patch");
  assert.equal(level("docs: explain tunnels", ["docs/site/src/ssh.md"]), "chore");
  assert.equal(level("refactor: split console.rs"), "patch");
});

test("release commits are ignored", () => {
  assert.equal(level("Release Savoia Studio", ["package.json", "Cargo.toml", "CHANGELOG.md"]), "none");
});

test("bumpVersion", () => {
  assert.equal(bumpVersion("0.1.1", "patch"), "0.1.2");
  assert.equal(bumpVersion("0.1.1", "minor"), "0.2.0");
  assert.equal(bumpVersion("0.1.1", "major"), "1.0.0");
  assert.equal(bumpVersion("0.1.1", "chore"), "0.1.1");
});

test("parseChangeset reads the bump and the first line", () => {
  const cs = parseChangeset('---\n"savoia-studio": minor\n---\n\nDouble-click a table.\nMore.');
  assert.deepEqual(cs, { level: "minor", summary: "Double-click a table." });
  assert.equal(parseChangeset("no frontmatter"), null);
});

const versions = { pkg: "0.1.1", cargo: "0.1.1", lockVersion: "0.1.1", lastTag: "v0.1.1" };
const commit = (lvl) => ({ hash: "abc1234", level: lvl });

test("pending changesets decide the next version", () => {
  const r = analyze({ versions, changesets: [{ level: "minor" }], commits: [commit("minor")] });
  assert.equal(r.state, "pending-changesets");
  assert.equal(r.next, "0.2.0");
  assert.deepEqual(r.problems, []);
});

test("commits bigger than the changesets are flagged", () => {
  const r = analyze({ versions, changesets: [{ level: "patch" }], commits: [commit("minor")] });
  assert.equal(r.next, "0.1.2");
  assert.equal(r.problems.length, 1);
});

test("shipped commits with no changeset are flagged", () => {
  const r = analyze({ versions, changesets: [], commits: [commit("patch"), commit("chore")] });
  assert.equal(r.state, "missing-changesets");
  assert.equal(r.next, "0.1.2");
  assert.equal(r.problems.length, 1);
});

test("only chores means nothing to release", () => {
  const r = analyze({ versions, changesets: [], commits: [commit("chore")] });
  assert.equal(r.state, "nothing-to-release");
  assert.deepEqual(r.problems, []);
});

test("a merged Release PR waits for its tag", () => {
  const r = analyze({ versions: { ...versions, pkg: "0.2.0", cargo: "0.2.0", lockVersion: "0.2.0" }, changesets: [], commits: [] });
  assert.equal(r.state, "awaiting-tag");
  assert.equal(r.current, "0.1.1");
  assert.equal(r.next, "0.2.0");
});

test("Cargo out of sync with package.json is flagged", () => {
  const r = analyze({ versions: { ...versions, cargo: "0.1.0" }, changesets: [], commits: [] });
  assert.equal(r.problems.length, 1);
});
