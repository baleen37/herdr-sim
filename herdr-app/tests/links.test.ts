import { test } from "node:test";
import assert from "node:assert/strict";
import { repoOf, splitDiff, stateOf } from "../src/links.ts";

test("repoOf names the PR's repository", () => {
  assert.equal(repoOf("https://github.com/baleen37/herdr-sim/pull/61"), "baleen37/herdr-sim");
  assert.equal(repoOf("https://example.com/x"), "");
});

test("stateOf shows an open draft as a draft", () => {
  assert.equal(stateOf({ state: "OPEN", draft: true }), "DRAFT");
  assert.equal(stateOf({ state: "MERGED", draft: true }), "MERGED");
  assert.equal(stateOf({ state: "OPEN", draft: false }), "OPEN");
});

test("splitDiff separates added and removed lines", () => {
  assert.deepEqual(splitDiff("+12 -3"), ["+12", "-3"]);
  assert.deepEqual(splitDiff("+4"), ["+4", ""]);
  assert.deepEqual(splitDiff("-2"), ["", "-2"]);
});
