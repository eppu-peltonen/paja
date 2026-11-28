// Tests for view.js: `node --test desk/ui/view.test.js` (the desk CI job runs it).
"use strict";
const test = require("node:test");
const assert = require("node:assert/strict");
const v = require("./view.js");

test("durations read as 7m 13s", () => {
  assert.equal(v.duration(433000), "7m 13s");
  assert.equal(v.duration(42000), "42s");
  assert.equal(v.duration(3900000), "1h 5m");
  assert.equal(v.duration(-5), "0s");
});

test("token counts are short", () => {
  assert.equal(v.tokens(842), "842");
  assert.equal(v.tokens(12345), "12.3k");
  assert.equal(v.tokens(2600000), "2.6M");
  assert.equal(v.usd(1.234), "$1.23");
});

test("a file list shows the first few and how many more", () => {
  assert.equal(v.files(["a", "b"]).text, "a, b");
  const many = v.files(["a", "b", "c", "d", "e"]);
  assert.deepEqual(many.shown, ["a", "b", "c"]);
  assert.equal(many.more, 2);
  assert.equal(many.text, "a, b, c +2 more");
  assert.equal(v.files([]).text, "");
  assert.equal(v.files(undefined).more, 0);
});

test("an item's colour follows its number and differs between neighbours", () => {
  assert.equal(v.colourTag(484), v.colourTag(484));
  assert.notEqual(v.colourTag(484), v.colourTag(485));
});

test("rounds, marks and first lines", () => {
  assert.deepEqual([1, 2, 3].map(v.roundLabel), ["first", "rework 1", "rework 2"]);
  assert.deepEqual(["pending", "pass", "block"].map(v.verdictMark), ["…", "✓", "✗"]);
  assert.equal(v.firstLine("\n  Which one?\nmore"), "Which one?");
  assert.equal(v.firstLine(null), "");
});

test("adding and removing a lane stays within 0 to 32", () => {
  assert.equal(v.lanesAfter(1, 1), 2);
  assert.equal(v.lanesAfter(1, -1), 0);
  assert.equal(v.lanesAfter(0, -1), null);
  assert.equal(v.lanesAfter(32, 1), null);
  assert.equal(v.lanesAfter(31, 1), 32);
});

const lane = (n, number) => ({ lane: n, number, title: `t${number}`, stage: "build", paths: [] });

test("a stopped pool shows idle cards, one per lane kept", () => {
  const cards = v.laneCards({ state: "stopped", target: 2, lanes: [], merging: [], landing: null });
  assert.deepEqual(cards.map((c) => c.kind), ["idle", "idle"]);
  const none = v.laneCards({ state: "stopped", target: 0, lanes: [] });
  assert.equal(none.length, 0);
});

test("a running pool shows busy lanes, idle slots, merging and closing", () => {
  const status = {
    state: "running",
    target: 3,
    lanes: [lane(2, 11), lane(1, 10)],
    landing: 10,
    merging: [11],
  };
  const cards = v.laneCards(status);
  assert.deepEqual(cards.map((c) => c.kind), ["busy", "busy", "idle"]);
  assert.deepEqual(cards.slice(0, 2).map((c) => c.lane.number), [10, 11]);
  assert.deepEqual(cards.slice(0, 2).map((c) => c.merging), [true, true]);
  assert.equal(cards.some((c) => c.closing), false);
});

test("a busy lane beyond the target finishes, then closes", () => {
  const cards = v.laneCards({
    state: "running",
    target: 1,
    lanes: [lane(1, 10), lane(2, 11)],
    landing: null,
    merging: [],
  });
  assert.deepEqual(cards.map((c) => c.closing), [false, true]);
  assert.equal(cards.every((c) => !c.merging), true);
});

test("waiting items split into ready and the rest", () => {
  const { ready, other } = v.splitWaiting([
    { number: 1, why: "no free lane" },
    { number: 2, why: "waits on #12" },
  ]);
  assert.deepEqual(ready.map((w) => w.number), [1]);
  assert.deepEqual(other.map((w) => w.number), [2]);
});

test("held items split by whether their issue is open", () => {
  const { open, closed } = v.heldItems([
    { number: 1, status: "escalated", open: true },
    { number: 2, status: "escalated", open: false },
    { number: 3, status: "queued", open: true },
  ]);
  assert.deepEqual(open.map((i) => i.number), [1]);
  assert.deepEqual(closed.map((i) => i.number), [2]);
});

test("activity lines say what happened, oldest first", () => {
  const events = [
    { kind: "run.landed", ts_ms: 3, data: { number: 7, merge_commit: "abcdef1234", checks_passed: 3 } },
    { kind: "run.step", ts_ms: 2, data: { number: 7, role: "builder", model: "opus" } },
    { kind: "pool.admitted", ts_ms: 1, data: { number: 7, lane: 1, paths: ["a.rs", "b.rs"] } },
    { kind: "run.tampered", ts_ms: 0, data: {} },
  ];
  const lines = v.activityLines(events);
  assert.deepEqual(lines.map((l) => l.what), ["admitted", "step", "landed"]);
  assert.equal(lines[0].detail, "lane 1: a.rs, b.rs");
  assert.equal(lines[1].detail, "builder on opus");
  assert.equal(lines[2].detail, "abcdef1, 3 checks");
  assert.equal(lines[2].number, 7);
  assert.equal(v.activityLine({ kind: "pool.lanes", ts_ms: 1, data: { lanes: 4 } }).detail, "4 lanes");
  assert.equal(v.activityLine({ kind: "run.escalated", ts_ms: 1, data: { number: 5 } }).what, "held");
  assert.equal(v.activityLine({ kind: "run.round", ts_ms: 1, data: { round: 2, verdict: "changes" } }).detail, "round 2 changes");
});
