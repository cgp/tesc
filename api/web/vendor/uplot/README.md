# uPlot, vendored

    uPlot v1.6.32 — https://github.com/leeoniya/uPlot — MIT
    Copyright (c) 2024 Leon Sorokin

    uPlot.iife.min.js  https://cdn.jsdelivr.net/npm/uplot@1.6.32/dist/uPlot.iife.min.js
    uPlot.min.css      https://cdn.jsdelivr.net/npm/uplot@1.6.32/dist/uPlot.min.css

Byte-for-byte as published; no local edits. The IIFE build rather than the ES module
one, so it loads with a plain `<script>` tag alongside Tabler and needs no bundler —
there is no build step here and adding one to draw a line chart would be a poor trade.

Copied rather than linked for the same reason Tabler is: a tool that watches a private
network must not need the public one to draw itself. Neither file reaches anything —
the stylesheet has no `url()`, no `@import` and no font import, and the only URL in the
script is the project link in its banner comment.

Chosen in §2.4 of the design: it draws thousands of points at 60fps, supports the
synchronized crosshair §15 asks for across every chart on the page, and leaves gaps as
gaps when a series value is `null`, which is the one drawing behaviour this tool
cannot compromise on.

## Updating

Re-download both files at the new version and update the URLs above, then open the
Charts page against a recording that has a collection gap and a phase boundary, and
check that the gap is still a break in the line rather than a straight segment across
it. There is no build step to catch a renamed option.
