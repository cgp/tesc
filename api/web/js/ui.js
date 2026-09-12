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
