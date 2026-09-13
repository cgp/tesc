// Shared formatting. Units are never implied: a number without one is a number
// nobody can check.

export function bytes(value) {
  if (value == null || Number.isNaN(value)) return "—";
  const units = ["B", "KB", "MB", "GB", "TB"];
  // Scale the magnitude, not the value: a delta can be negative, and the loop below
  // would never fire for one -- printing "-800000000 B" where "-763.0 MB" belongs.
  const sign = value < 0 ? "-" : "";
  let n = Math.abs(value);
  let i = 0;
  while (n >= 1024 && i < units.length - 1) {
    n /= 1024;
    i += 1;
  }
  return `${sign}${n.toFixed(i === 0 ? 0 : 1)} ${units[i]}`;
}

/**
 * A count of things, with the noun, so a bare number never stands in a sentence.
 *
 * English pluralisation for the handful of nouns this page counts. "1 files" reads as
 * unfinished and "file(s)" reads as though nobody looked.
 */
export function count(n, singular, plural = `${singular}s`) {
  return `${n} ${n === 1 ? singular : plural}`;
}

export function percent(value) {
  return value == null ? "—" : `${value.toFixed(1)}%`;
}

export function duration(ms) {
  if (ms == null) return "—";
  const seconds = Math.round(ms / 1000);
  if (seconds < 60) return `${seconds}s`;
  const minutes = Math.floor(seconds / 60);
  return `${minutes}m ${String(seconds % 60).padStart(2, "0")}s`;
}

export function timestamp(iso) {
  if (!iso) return "—";
  return iso.replace("T", " ").replace("Z", " UTC");
}

export function metricValue(name, value) {
  if (value == null) return "—";
  if (name.endsWith("_bytes")) return bytes(value);
  if (name.endsWith("_bytes_per_s")) return `${bytes(value)}/s`;
  if (name.startsWith("cpu.") || name.endsWith("_pct")) return percent(value);
  if (name.endsWith("_per_s")) return `${value.toFixed(2)}/s`;
  return value.toFixed(2);
}

/**
 * How far a metric moved, as opposed to what it reads.
 *
 * Separate from `metricValue` because a change in a metric that is *itself* a
 * percentage is not a percentage: CPU going from 21% to 57% moved 36 percentage
 * points, and printing that as "+35.7%" beside the relative "+167.8%" puts two
 * different meanings of the same symbol next to each other in one cell. Points are
 * what a difference between two percentages is called, so it says so.
 */
export function metricChange(name, value) {
  if (value == null) return "—";
  const sign = value > 0 ? "+" : value < 0 ? "−" : "";
  const size = Math.abs(value);
  if (name.startsWith("cpu.") || name.endsWith("_pct")) {
    return `${sign}${size.toFixed(1)} pts`;
  }
  return `${sign}${metricValue(name, size)}`;
}

/**
 * A target id, shortened for a column heading.
 *
 * Discovered endpoints are named after the thing they are -- an ECS task id is 32
 * hex characters -- because that identity is what joins host metrics to load
 * metrics. It is a poor column heading, so the display is shortened and the full id
 * stays in the title attribute. Shortening the id itself would break the join it
 * exists to make.
 */
export function targetLabel(id) {
  const match = /^(task|container)\/([0-9a-f]{16,})(\/.*)?$/.exec(id ?? "");
  if (!match) return id ?? "";
  return `${match[1]}/${match[2].slice(0, 8)}…${match[3] ?? ""}`;
}

/**
 * Escape a string for interpolation into markup, attribute values included.
 *
 * This used to hand the string to `textContent` and read `innerHTML` back, which
 * escapes `&`, `<` and `>` and *not* quotes -- so anything containing a `"` broke
 * out of the attribute it was written into. Every id, profile name, target name,
 * annotation message and search term on this page goes through here, and most of
 * them land in an attribute. Written out rather than delegated to the DOM for that
 * reason, and because a formatter that needs a document cannot be tested without one.
 */
export function escape(text) {
  return String(text ?? "")
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;")
    .replaceAll("'", "&#39;");
}
