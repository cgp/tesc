// The single source the views read. Nothing else talks to the network; views
// subscribe here and re-render from whatever this holds.

const state = {
  health: null,
  route: null,
  profiles: [],
  brokenProfiles: [],
  recordings: [],
  selectedRecording: null,
  live: null,
  error: null,
};

const listeners = new Set();

export function subscribe(listener) {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

export function get() {
  return state;
}

export function set(patch) {
  Object.assign(state, patch);
  for (const listener of listeners) listener(state);
}
