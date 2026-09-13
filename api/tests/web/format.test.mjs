// The shared formatters. Small, and wrong in ways that only show up in one view.
import assert from "node:assert/strict";
import test from "node:test";

const { bytes, escape, metricChange, targetLabel } = await import("../../web/js/format.js");

test("bytes scales the magnitude, so a negative delta reads like a size", () => {
  assert.equal(bytes(0), "0 B");
  assert.equal(bytes(1536), "1.5 KB");
  assert.equal(bytes(8e9), "7.5 GB");
  // The bug this exists for: the scaling loop never fires for a negative number.
  assert.equal(bytes(-8e8), "-762.9 MB");
  assert.equal(bytes(-1536), "-1.5 KB");
  assert.equal(bytes(null), "—");
});

test("a long discovered id is shortened, and anything else is left alone", () => {
  assert.equal(targetLabel("task/3f1c5a7e9b2d4c6f8a0b1c2d3e4f5a6b"), "task/3f1c5a7e…");
  assert.equal(
    targetLabel("task/3f1c5a7e9b2d4c6f8a0b1c2d3e4f5a6b/api"),
    "task/3f1c5a7e…/api"
  );
  assert.equal(targetLabel("i-0aaa1111bbbb2222c"), "i-0aaa1111bbbb2222c");
  assert.equal(targetLabel("app-1"), "app-1");
  assert.equal(targetLabel(undefined), "");
});

test("escaping covers quotes, because most of what it escapes lands in an attribute", () => {
  // The bug this exists for: `"` used to pass through, so a value containing one
  // closed the attribute it was written into and the rest became markup.
  assert.equal(escape('"><script>alert(1)</script>'), "&quot;&gt;&lt;script&gt;alert(1)&lt;/script&gt;");
  assert.equal(escape("it's"), "it&#39;s");
  assert.equal(escape("a & b"), "a &amp; b");
  assert.equal(escape(null), "");
  assert.equal(escape(42), "42");
});

test("a change in a percentage is points, not another percentage", () => {
  // CPU from 21% to 57% moved 36 percentage points. Printing that as "+35.7%" next
  // to the relative "+167.8%" puts two meanings of one symbol in the same cell.
  assert.equal(metricChange("cpu.busy", 35.66), "+35.7 pts");
  assert.equal(metricChange("cpu.busy", -4), "−4.0 pts");
  assert.equal(metricChange("queue_pct", 1.25), "+1.3 pts");
});

test("a change in anything else keeps that metric's own units", () => {
  assert.equal(metricChange("mem.used_bytes", 359439205), "+342.8 MB");
  assert.equal(metricChange("mem.used_bytes", -359439205), "−342.8 MB");
  assert.equal(metricChange("conn.established", 3), "+3.00");
});

test("no change is signed neither way, and no value is a dash", () => {
  assert.equal(metricChange("cpu.busy", 0), "0.0 pts");
  assert.equal(metricChange("cpu.busy", null), "—");
});
