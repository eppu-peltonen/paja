// What Paja Desk's screen shows, as plain functions of the core's JSON: no document, no Tauri.
// desk.js draws with them; `node --test desk/ui/view.test.js` (the desk CI job) tests them.
"use strict";

const MAX_LANES = 32;
const FILES_SHOWN = 3;

// `7m 13s`, `1h 5m`, `42s`.
function duration(ms) {
  const secs = Math.max(0, Math.round((ms || 0) / 1000));
  const h = Math.floor(secs / 3600);
  const m = Math.floor((secs % 3600) / 60);
  const s = secs % 60;
  return h ? `${h}h ${m}m` : m ? `${m}m ${s}s` : `${s}s`;
}

// `842`, `12.3k`, `2.6M`.
function tokens(n) {
  const v = Math.max(0, Number(n) || 0);
  if (v >= 1e9) return `${(v / 1e9).toFixed(1)}G`;
  if (v >= 1e6) return `${(v / 1e6).toFixed(1)}M`;
  if (v >= 1e3) return `${(v / 1e3).toFixed(1)}k`;
  return String(Math.round(v));
}

function usd(n) {
  return `$${(Number(n) || 0).toFixed(2)}`;
}

// The first few files and how many more there are: `{ shown, more, text }`.
function files(paths, shown = FILES_SHOWN) {
  const all = paths || [];
  const head = all.slice(0, shown);
  const more = all.length - head.length;
  const text = head.join(", ") + (more > 0 ? (head.length ? ` +${more} more` : `+${more} more`) : "");
  return { shown: head, more, text };
}

// A hue from an item's number, so one item has one colour on every part of the screen.
// Consecutive numbers land far apart (the golden angle).
function colourTag(number) {
  const hue = Math.round((Number(number) * 137.508) % 360);
  return `hsl(${hue} 62% 48%)`;
}

// `first`, `rework 1`, `rework 2`: the review round (1 up) as the person reads it.
function roundLabel(round) {
  return !round || round <= 1 ? "first" : `rework ${round - 1}`;
}

// One mark per reviewer: pending, pass or block.
const MARKS = { pending: "…", pass: "✓", block: "✗" };
function verdictMark(verdict) {
  return MARKS[verdict] ?? "?";
}

// The first line of a question, for one-line lists.
function firstLine(text) {
  const line = String(text || "").split(/\r?\n/).find((l) => l.trim());
  return line ? line.trim() : "";
}

// The lane count after adding or removing one, within 0 to MAX_LANES; null when it can't move.
function lanesAfter(target, delta) {
  const next = Number(target) + delta;
  return next < 0 || next > MAX_LANES ? null : next;
}

// How many busy lanes are beyond the lane count the pool keeps. The core lets every busy lane
// finish and closes whichever frees first, so no card is the one that closes.
function shrinkBy(status) {
  if (status.state !== "running") return 0;
  return Math.max(0, (status.lanes || []).length - (status.target ?? 1));
}

// The cards of the lanes region, from `pool_status`: one per lane the pool keeps (at least the
// busy ones), busy ones first in lane order. While the pool is stopped every card is idle.
function laneCards(status) {
  const running = status.state === "running";
  const busy = running ? [...status.lanes].sort((a, b) => a.lane - b.lane) : [];
  const keep = Math.max(status.target ?? 1, busy.length);
  const merging = new Set(status.merging || []);
  const cards = busy.map((lane) => ({
    kind: "busy",
    lane,
    merging: status.landing === lane.number || merging.has(lane.number),
  }));
  for (let i = busy.length; i < keep; i++) cards.push({ kind: "idle" });
  return cards;
}

// The waiting items that wait only for a free lane, and the others with their reason.
function splitWaiting(waiting) {
  const ready = (waiting || []).filter((w) => w.why === "no free lane");
  return { ready, other: (waiting || []).filter((w) => w.why !== "no free lane") };
}

// Held items, one line each: the open ones, and those whose issue closed.
function heldItems(items) {
  const held = (items || []).filter((i) => i.status === "escalated");
  return { open: held.filter((i) => i.open), closed: held.filter((i) => !i.open) };
}

// One activity line from a `pool.*` or `run.*` event, or null for the events the strip skips:
// `{ ts_ms, what, number, detail }`.
function activityLine(event) {
  const d = event.data || {};
  const line = (what, detail) => ({ ts_ms: event.ts_ms, what, number: d.number ?? null, detail });
  switch (event.kind) {
    case "pool.admitted":
      return line("admitted", `lane ${d.lane ?? "?"}: ${files(d.paths).text || "no files named"}`);
    case "run.step":
      return line("step", [d.role, d.model].filter(Boolean).join(" on "));
    case "run.round":
      return line("round", `${d.round != null ? `round ${d.round} ` : ""}${d.verdict ?? ""}`.trim());
    case "run.landed": {
      const bits = [];
      if (d.merge_commit) bits.push(String(d.merge_commit).slice(0, 7));
      if (d.checks_passed != null) bits.push(`${d.checks_passed} checks`);
      return line("landed", bits.join(", "));
    }
    case "run.escalated":
      return line("held", "waiting for an answer");
    case "pool.lanes":
      return line("lanes", `${d.lanes} lanes`);
    default:
      return null;
  }
}

// The activity strip's lines, oldest first (`pool_events` returns the newest first).
function activityLines(events) {
  return [...(events || [])].reverse().map(activityLine).filter(Boolean);
}

const exported = {
  MAX_LANES, duration, tokens, usd, files, colourTag, roundLabel, verdictMark, firstLine,
  lanesAfter, laneCards, shrinkBy, splitWaiting, heldItems, activityLine, activityLines,
};
if (typeof module !== "undefined" && module.exports) module.exports = exported;
else window.View = exported;
