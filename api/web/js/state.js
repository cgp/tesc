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
  //: Which column the stats table is ordered by, if a person has chosen one.
  //: Null means the default: metric name, alphabetically.
  tableSort: null,
  //: Bumped when live samples arrive, so the charts redraw without the whole live
  //: object becoming a chart dependency. The table watches the summaries; the charts
  //: watch the points, and they arrive on the same tick.
  chartTick: 0,
  recordings: [],
  //: The archive around the rows: which filters are set, how many recordings exist
  //: in total, and what there is to filter by. Kept beside the rows rather than in
  //: them, because a filtered page cannot say how much it is hiding on its own.
  archive: { filters: {}, facets: {}, total: 0 },
  selectedRecording: null,
  //: Every setup that has been recorded against, and the one being read. A series
  //: is not a thing anyone creates -- runs group themselves by identity -- so this
  //: is derived from the archive rather than stored beside it.
  series: [],
  selectedSeries: null,
  //: How many usable runs a series needs before it has a band. Sent by the API so
  //: the list can say which series are long enough to be worth opening.
  seriesFloor: 0,
  live: null,
  error: null,
  //: A short confirmation of something that worked, in the same slot as the error.
  //: A copy button with no visible effect is indistinguishable from a broken one.
  notice: null,
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
