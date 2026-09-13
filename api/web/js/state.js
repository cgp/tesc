// The single source the views read. Subscribers select their dependencies so
// unrelated updates cannot replace an active form or disturb its focus.

const state = {
  health: null,
  route: null,
  profiles: [],
  brokenProfiles: [],
  //: When the profile list was last read off disk. Shown next to the reload
  //: button so that pressing it has a visible effect even when nothing changed.
  profilesReadAt: null,
  //: The profile being written, if any. Intentional editor actions read the form
  //: into this draft before rebuilding it; background updates leave its DOM intact.
  profileDraft: null,
  //: The profile currently being walked, if any. A discovery walk is several
  //: seconds of control-plane calls, and a button with no sign of life reads as a
  //: broken one.
  resolving: null,
  //: The profile being checked, if any, and the last report for each one. Kept in
  //: memory rather than stored: reachability is true of a moment, and a result from
  //: yesterday shown as if it were current would be worse than no result.
  verifying: null,
  verified: {},
  recordings: [],
  selectedRecording: null,
  live: null,
  error: null,
};

const listeners = new Set();

// Selectors return a tuple of primitives or object references. Replace selected
// objects on updates: mutating them in place would hide the change. Subscriptions
// begin with the current selection; initial rendering is the caller's choice.
export function subscribe(select, listener) {
  const subscription = { select, listener, values: select(state) };
  listeners.add(subscription);
  return () => listeners.delete(subscription);
}

export function get() {
  return state;
}

export function set(patch) {
  Object.assign(state, patch);
  for (const subscription of listeners) {
    const values = subscription.select(state);
    if (values.length === subscription.values.length &&
        values.every((value, index) => Object.is(value, subscription.values[index]))) continue;
    subscription.values = values;
    subscription.listener(state);
  }
}
