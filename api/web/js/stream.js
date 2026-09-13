// The live connection.
//
// EventSource, not WebSocket: the view is one-way, and the browser handles
// reconnection and Last-Event-ID for us -- so a dropped connection resumes where it
// left off rather than leaving a hole. Nothing here touches the DOM; it writes to
// state and the views re-render from that.

import { get, set } from "./state.js";

let source = null;
let currentId = null;

export function connected() {
  return source !== null && source.readyState !== EventSource.CLOSED;
}

export function connect(recordingId) {
  if (currentId === recordingId && connected()) return;
  disconnect();

  currentId = recordingId;
  source = new EventSource(`/api/recordings/${encodeURIComponent(recordingId)}/stream`);

  source.addEventListener("snapshot", (event) => {
    const snapshot = JSON.parse(event.data);
    set({
      live: {
        recordingId: snapshot.recording_id,
        status: snapshot.status,
        elapsedMs: snapshot.elapsed_ms,
        targets: snapshot.targets,
        metrics: snapshot.metrics,
        latest: snapshot.latest,
        counts: snapshot.counts,
        summaries: snapshot.summaries ?? {},
        spans: snapshot.spans ?? {},
        phase: snapshot.phase,
        gaps: get().live?.gaps ?? [],
        annotations: get().live?.annotations ?? [],
        connection: "live",
      },
    });
  });

  source.addEventListener("samples", (event) => {
    const payload = JSON.parse(event.data);
    const live = { ...(get().live ?? {}) };
    const latest = { ...(live.latest ?? {}) };

    for (const sample of payload.samples) {
      for (const [metric, value] of Object.entries(sample.metrics)) {
        latest[metric] = { ...(latest[metric] ?? {}), [sample.target_id]: value };
      }
    }

    live.latest = latest;
    live.metrics = Object.keys(latest).sort();
    live.elapsedMs = payload.elapsed_ms;
    live.counts = payload.counts;
    // Computed server-side, where the sample-count rule lives (stats/summary.py).
    live.summaries = payload.summaries ?? live.summaries ?? {};
    live.spans = payload.spans ?? live.spans ?? {};
    live.connection = "live";
    set({ live });
  });

  source.addEventListener("gap", (event) => {
    const live = { ...(get().live ?? {}) };
    live.gaps = [...(live.gaps ?? []), JSON.parse(event.data)];
    set({ live });
  });

  source.addEventListener("annotation", (event) => {
    const live = { ...(get().live ?? {}) };
    live.annotations = [...(live.annotations ?? []), JSON.parse(event.data)];
    set({ live });
  });

  source.addEventListener("status", (event) => {
    const payload = JSON.parse(event.data);
    const live = { ...(get().live ?? {}), status: payload.status, connection: "ended" };
    set({ live });
    disconnect();
  });

  source.onerror = () => {
    // EventSource reconnects on its own and replays from Last-Event-ID. Say so
    // rather than showing an error the user cannot act on.
    const live = { ...(get().live ?? {}), connection: "reconnecting" };
    set({ live });
  };
}

export function disconnect() {
  if (source) {
    source.close();
    source = null;
  }
  currentId = null;
}
