// Markup the views share: icons, and the empty state that used to be a bare
// sentence in a card.
//
// Icons are inline SVG paths from Tabler Icons (MIT) rather than a font or a
// sprite: there is no second request to fail, nothing to flash, and the stroke
// takes `currentColor` so a disabled button's icon dims with its label.
//
// `title` and `body` are markup, not text -- several of these carry a link. They
// are written here, never taken from the API, so there is nothing to escape.

import { escape as escapeText } from "./format.js";

const PATHS = {
  "alert-triangle": [
    "M10.363 3.591l-8.106 13.534a1.914 1.914 0 0 0 1.636 2.871h16.214a1.914 1.914 0 0 0 1.636 -2.87l-8.106 -13.536a1.914 1.914 0 0 0 -3.274 0z",
    "M12 9v4",
    "M12 16h.01",
  ],
  "arrow-left": ["M5 12l14 0", "M5 12l6 6", "M5 12l6 -6"],
  archive: [
    "M3 6a2 2 0 0 1 2 -2h14a2 2 0 0 1 2 2a2 2 0 0 1 -2 2h-14a2 2 0 0 1 -2 -2z",
    "M5 8v10a2 2 0 0 0 2 2h10a2 2 0 0 0 2 -2v-10",
    "M10 12h4",
  ],
  "chart-line": ["M4 19l16 0", "M4 15l4 -6l4 2l4 -5l4 4"],
  "chevron-down": ["M6 9l6 6l6 -6"],
  check: ["M5 12l5 5l10 -10"],
  "chevron-up": ["M6 15l6 -6l6 6"],
  clipboard: [
    "M9 5h-2a2 2 0 0 0 -2 2v12a2 2 0 0 0 2 2h10a2 2 0 0 0 2 -2v-12a2 2 0 0 0 -2 -2h-2",
    "M9 3m0 2a2 2 0 0 1 2 -2h2a2 2 0 0 1 2 2v0a2 2 0 0 1 -2 2h-2a2 2 0 0 1 -2 -2z",
  ],
  download: [
    "M4 17v2a2 2 0 0 0 2 2h12a2 2 0 0 0 2 -2v-2",
    "M7 11l5 5l5 -5",
    "M12 4l0 12",
  ],
  "device-floppy": [
    "M6 4h10l4 4v10a2 2 0 0 1 -2 2h-12a2 2 0 0 1 -2 -2v-12a2 2 0 0 1 2 -2",
    "M10 14a2 2 0 1 0 4 0a2 2 0 0 0 -4 0",
    "M14 4l0 4l-6 0l0 -4",
  ],
  "list-check": [
    "M3.5 5.5l1.5 1.5l2.5 -2.5",
    "M3.5 11.5l1.5 1.5l2.5 -2.5",
    "M3.5 17.5l1.5 1.5l2.5 -2.5",
    "M11 6l9 0",
    "M11 12l9 0",
    "M11 18l9 0",
  ],
  pencil: [
    "M4 20h4l10.5 -10.5a2.828 2.828 0 1 0 -4 -4l-10.5 10.5v4",
    "M13.5 6.5l4 4",
  ],
  "plug-connected": [
    "M7 12l5 5l-1.5 1.5a3.536 3.536 0 1 1 -5 -5l1.5 -1.5z",
    "M17 12l-5 -5l1.5 -1.5a3.536 3.536 0 1 1 5 5l-1.5 1.5z",
    "M3 21l2.5 -2.5",
    "M18.5 5.5l2.5 -2.5",
    "M10 11l-2 2",
    "M13 14l-2 2",
  ],
  plus: ["M12 5l0 14", "M5 12l14 0"],
  refresh: [
    "M20 11a8.1 8.1 0 0 0 -15.5 -2m-.5 -4v4h4",
    "M4 13a8.1 8.1 0 0 0 15.5 2m.5 4v-4h-4",
  ],
  target: [
    "M12 12m-1 0a1 1 0 1 0 2 0a1 1 0 1 0 -2 0",
    "M12 12m-5 0a5 5 0 1 0 10 0a5 5 0 1 0 -10 0",
    "M12 12m-9 0a9 9 0 1 0 18 0a9 9 0 1 0 -18 0",
  ],
  trash: [
    "M4 7l16 0",
    "M10 11l0 6",
    "M14 11l0 6",
    "M5 7l1 12a2 2 0 0 0 2 2h8a2 2 0 0 0 2 -2l1 -12",
    "M9 7v-3a1 1 0 0 1 1 -1h4a1 1 0 0 1 1 1v3",
  ],
  "player-play": ["M7 4v16l13 -8z"],
  "player-stop": ["M5 7a2 2 0 0 1 2 -2h10a2 2 0 0 1 2 2v10a2 2 0 0 1 -2 2h-10a2 2 0 0 1 -2 -2z"],
  server: [
    "M3 7a3 3 0 0 1 3 -3h12a3 3 0 0 1 3 3a3 3 0 0 1 -3 3h-12a3 3 0 0 1 -3 -3z",
    "M3 15a3 3 0 0 1 3 -3h12a3 3 0 0 1 3 3a3 3 0 0 1 -3 3h-12a3 3 0 0 1 -3 -3z",
    "M7 8l0 .01",
    "M7 16l0 .01",
  ],
};

export function icon(name, className = "icon") {
  const paths = PATHS[name];
  // A missing icon is a typo in our own source, and a silently blank one would be
  // found by eye weeks later. The render guard in main.js catches this.
  if (!paths) throw new Error(`unknown icon: ${name}`);
  return `<svg class="${className}" viewBox="0 0 24 24" fill="none" stroke="currentColor"
    stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"
    >${paths.map((d) => `<path d="${d}"/>`).join("")}</svg>`;
}

/** One labelled value in a `datagrid`. `content` is markup; `title` is text. */
export function field(title, content) {
  return `<div class="datagrid-item">
    <div class="datagrid-title">${escapeText(title)}</div>
    <div class="datagrid-content">${content}</div>
  </div>`;
}

export function empty({ icon: name, title, body, action = "" }) {
  return `<div class="card">
    <div class="empty">
      <div class="empty-icon">${icon(name)}</div>
      <p class="empty-title">${title}</p>
      <p class="empty-subtitle text-secondary">${body}</p>
      ${action ? `<div class="empty-action">${action}</div>` : ""}
    </div>
  </div>`;
}
