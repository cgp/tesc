// Shared formatting. Units are never implied: a number without one is a number
// nobody can check.

export function bytes(value) {
  if (value == null || Number.isNaN(value)) return "—";
  const units = ["B", "KB", "MB", "GB", "TB"];
  let n = value;
  let i = 0;
  while (n >= 1024 && i < units.length - 1) {
    n /= 1024;
    i += 1;
  }
  return `${n.toFixed(i === 0 ? 0 : 1)} ${units[i]}`;
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

export function escape(text) {
  const div = document.createElement("div");
  div.textContent = text ?? "";
  return div.innerHTML;
}
