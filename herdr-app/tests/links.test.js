import { test } from "node:test";
import assert from "node:assert/strict";
import { prUrls, repoOf, stateOf } from "../mods/sim-prs/links.js";

const link = (url) => JSON.stringify({ type: "pr-link", sessionId: "s1", prUrl: url });

test("prUrls lists each PR once, newest first", () => {
  const a = "https://github.com/o/a/pull/1", b = "https://github.com/o/b/pull/2";
  assert.deepEqual(prUrls([link(a), link(b), link(a), ""].join("\n")), [a, b]);
});

test("prUrls skips lines that aren't whole records", () => {
  assert.deepEqual(prUrls(`{"type":"pr-link","prUrl":"htt\n${link("https://github.com/o/r/pull/3")}`), ["https://github.com/o/r/pull/3"]);
  assert.deepEqual(prUrls(""), []);
});

test("repoOf names the PR's repository", () => {
  assert.equal(repoOf("https://github.com/baleen37/herdr-sim/pull/61"), "baleen37/herdr-sim");
  assert.equal(repoOf("https://example.com/x"), "");
});

test("stateOf shows an open draft as a draft", () => {
  assert.equal(stateOf({ state: "OPEN", isDraft: true }), "DRAFT");
  assert.equal(stateOf({ state: "MERGED", isDraft: true }), "MERGED");
  assert.equal(stateOf({ state: "OPEN", isDraft: false }), "OPEN");
});
