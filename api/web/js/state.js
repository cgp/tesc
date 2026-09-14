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
  plans: [],
  brokenPlans: [],
  //: When the plan directory was last read, shown beside its reload button for the
  //: same reason the profile list carries one: a reload that found nothing changed
  //: has to look different from a button that does nothing.
  plansReadAt: null,
  //: The plan being edited, if any: the mixture as loaded, the calls it invokes, and
  //: the bundle preview asked for while it is open. Editor actions read the form
  //: into it; background updates leave its DOM alone.
  planDraft: null,
  //: The paste panel, when a description is being turned into calls. Holds the
  //: mode (a new plan, or new calls for an existing one), what has been pasted, and
  //: the server's reason if it would not read it.
  planNew: null,
  //: What the server says about the draft as it stands -- the problems and the
  //: arithmetic. Never computed here: the browser renders this answer and reaches
  //: none of it, so a form cannot save what a hand-written file would be rejected
  //: for.
  planCheck: null,
  //: Which column the stats table is ordered by, if a person has chosen one.
  //: Null means the default: metric name, alphabetically.
  tableSort: null,
  //: Bumped when live samples arrive, so the charts redraw without the whole live
  //: object becoming a chart dependency. The table watches the summaries; the charts
  //: watch the points, and they arrive on the same tick.
  chartTick: 0,
  recordings: [],
  selectedRecordings: [],
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
  //: Which runs of the open series are ticked for comparison. Held while the page
  //: is open rather than in the hash: it is a selection in progress, and a URL that
  //: changed on every tick would fill the back button with half-made choices.
  selectedRuns: [],
  //: One recording's boxes ranked against each other. Held apart from the
  //: recording itself because it is a different question about it: not "what did
  //: this run do" but "which of these machines is the odd one out".
  sweep: null,
  //: The comparison being read, and each of its runs' samples for the overlay. The
  //: table comes summarised from the server; these are only what the lines are drawn
  //: from, which is why they are kept apart from it.
  comparison: null,
  comparisonSeries: null,
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
