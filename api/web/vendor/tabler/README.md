# Tabler, vendored

    Tabler v1.0.0 — https://tabler.io — MIT
    Copyright 2018-2025 The Tabler Authors / codecalm.net Paweł Kuna

    tabler.min.css  https://cdn.jsdelivr.net/npm/@tabler/core@1.0.0/dist/css/tabler.min.css
    tabler.min.js   https://cdn.jsdelivr.net/npm/@tabler/core@1.0.0/dist/js/tabler.min.js

Byte-for-byte as published; no local edits. Our own layer is `web/css/app.css`, which
only names Tabler's CSS variables or sits in a namespace Tabler does not use — editing
the files in this directory is what would make the next version bump expensive.

Copied rather than linked because a tool that observes a private network must not need
the public one to draw itself. The stylesheet reaches nothing: every image in it is an
inline `data:` URI, there is no `@import`, and the font stack ends at the system UI
font, so nothing is fetched at render time either. `scripts/check.sh` fails if a CDN
URL reappears anywhere under `web/`.

Icons are inline SVG paths lifted from [Tabler Icons](https://tabler.io/icons) (MIT)
and live in `web/js/ui.js` and `web/index.html`. There is no icon font and no sprite
sheet: a menu that needs a second request to show its icons shows them late.

## Updating

Re-download both files at the new version, update the URLs above, then open every page
and check the menu, the cards, and a live table. There is no build step to catch a
renamed class.
