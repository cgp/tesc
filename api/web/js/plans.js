// Plans: the mixture that decides what a run sends, and the editor for it.
//
// Three documents make a plan (§4), and they are not equally editable. The mixture
// is the knob that gets turned, so it is a form. The calls are authored from the
// codebase or generated from a schema and are read-only here (§20.3) -- editing a
// request definition in a browser is how a plan drifts from the service it
// describes. Targets are not stored with a plan at all: they come from a profile
// when the bundle is assembled.
//
// **Nothing on this page decides whether a plan is valid.** The percentages, the
// implied rates and the sample counts all arrive computed from `/api/plans/.../
// validate`, which is the same function the save path and the bundle gate call. What
// is here is arithmetic in the other direction only: turning a typed iterations/s
// back into the percentage the document stores, because the document has to be built
// before it can be submitted.

import { count, escape } from "./format.js";
import { empty, icon } from "./ui.js";

// What a description can be. The label says what each one is good for, because the
// difference that matters is not the file format: two of these counted real traffic
// and three of them describe a shape.
const SOURCES = [
  ["openapi", "OpenAPI 3 (JSON or YAML)", "Every operation, its parameters and the codes it declares. Weights are flat."],
  ["swagger", "Swagger 2.0 (JSON or YAML)", "The same structural calls, read locally without a conversion service."],
  ["wadl", "WADL", "HTTP resources and methods from an XML application description."],
  ["wsdl", "WSDL 1.1", "SOAP operations, with an envelope built from the schema."],
  ["har", "HAR capture", "Real paths and real frequencies. Bodies and headers are not copied."],
  ["access_log", "Access log", "Common or combined format: weights grounded in production traffic."],
  ["routes", "Route list", "One METHOD /path per line, for when nothing else exists."],
];

const MODES = ["fixed", "stages", "breakpoint"];
const SESSIONS = ["fresh", "reuse", "pool"];
const REQUEST_TYPES = ["json", "xml", "html", "form", "text", "generated", "none", "other"];

/** A new chain, with one step: a chain with no steps sends nothing. */
export function blankChain(call) {
  return { name: "", percent: 0, session: "reuse", steps: [blankStep(call)] };
}

export function blankStep(call) {
  return { id: "", call: call ?? "" };
}

export function blankDocument(name = "") {
  return {
    version: 1,
    name,
    calls: ["calls/generated.json"],
    load: { mode: "fixed", rate: 75, duration: "60s", max_concurrency: 200 },
    chains: [],
  };
}

/** Keep one simple chain per selected schema endpoint, preserving edits if possible. */
export function chainsForCalls(previous, names) {
  const used = new Set();
  const chains = names.map((call) => {
    const old = previous.find((chain) => chain.steps?.[0]?.call === call);
    if (old) {
      used.add(old.name);
      return { ...old };
    }
    const chain = blankBasicChain(call, [...previous, ...Array.from(used, (name) => ({ name }))]);
    used.add(chain.name);
    return chain;
  });
  const percent = names.length ? Math.round((100 / names.length) * 10000) / 10000 : 0;
  return chains.map((chain, index) => ({
    ...chain,
    percent: index === chains.length - 1
      ? Math.round((100 - percent * (chains.length - 1)) * 10000) / 10000
      : percent,
  }));
}

/** A Basic row is always a complete, single-call chain. */
export function blankBasicChain(call, existing = []) {
  const base = call || "call";
  const used = new Set(existing.map((chain) => chain.name));
  let name = base;
  let suffix = 2;
  while (used.has(name)) name = `${base}-${suffix++}`;
  return {
    name,
    percent: 0,
    session: "reuse",
    steps: [{ id: name, call: call ?? "" }],
  };
}

/** The call table only opens over a shape it can preserve. */
export function basicCompatible(doc, calls = []) {
  const known = new Set(calls.map((call) => call.name));
  const chains = doc.chains ?? [];
  const names = chains.map((chain) => chain.name);
  return (
    (doc.load?.mode ?? "fixed") === "fixed" &&
    names.every(Boolean) &&
    new Set(names).size === names.length &&
    chains.every(
      (chain) =>
        chain.steps?.length === 1 &&
        known.has(chain.steps[0].call) &&
        !chain.steps[0].repeat_until
    )
  );
}

export function selectState(state) {
  return state.planDraft
    ? [state.planDraft, state.planCheck, state.profiles, state.schemas, state.planNew]
    : [state.plans, state.brokenPlans, state.plansReadAt, state.planNew];
}

export function render(state) {
  rendered = signature(state);
  // The paste panel wins over both views: it is a question being answered, and the
  // page behind it is what the answer is for.
  if (state.planNew) return describe(state.planNew);
  if (state.planDraft) return editor(state);
  return list(state);
}

export function help() {
  return {
    title: "Plans, percentages, and what they buy",
    body: `
      <p>A plan is three documents. The <strong>mixture</strong> says what share of
      the traffic each chain of calls takes and how much traffic there is; the
      <strong>calls</strong> are the individual requests; the <strong>targets</strong>
      are the machines. Only the first is edited here.</p>

      <p><strong>A percentage buys chain iterations, not requests.</strong> A
      two-step chain at 20% of 150/s is 30 iterations a second and 60 requests a
      second. Both figures are shown beside every chain, because a percentage on its
      own is not a quantity anyone can judge, and the difference between the two is
      the length of the chain.</p>

      <p><strong>The percentages must total 100.</strong> They are never
      renormalized to get there: adjusting five chains to accommodate a typo in the
      sixth would measure a mixture nobody chose. The shortfall or the excess is
      named instead, and the plan will not assemble into a bundle until it is
      resolved.</p>

      <p><strong>Sample counts are settled before the run, not after it.</strong>
      Below about 2250 requests a p99 is an order statistic with a confidence
      interval wide enough to hide a regression, so this page says so while the
      duration and the rate are still being chosen. It says it per chain as well as
      for the run: a run can be long enough overall while the 5% chain inside it is
      nowhere near, and it is that chain's percentiles that end up quoted.</p>

      <p><strong>Errors stop a run; warnings do not.</strong> A plan with errors
      still saves — half-finished is a normal state to leave an afternoon's work in —
      but it will not assemble into a bundle, because the engine would reject the
      same document a moment later and a zip that cannot run still looks like an
      artifact.</p>

      <p><strong>Calls are read-only</strong> (§20.3). What is offered instead is the
      bundle: export it, change the call in the file with the tooling and review a
      code change gets, and put it back.</p>

      <p><strong>A plan can be generated from a description of the service</strong>
      (§8) — an OpenAPI, Swagger, WADL or WSDL document, a HAR capture, an access log, or a bare
      list of routes. That is the mechanical half of the job: one call per operation,
      parameters from the schema's own examples, assertions from the codes it
      declares. There is no model in it, so the same document always gives the same
      plan and a diff between two of them means the service changed.</p>

      <p>A new plan opens blank. Choose one of the service descriptions already stored
      in Schemas, then check the endpoints this plan should call. The selected
      definitions become the read-only calls behind the mixture; generation is no
      longer a separate step before the editor.</p>

      <p>What generation will never do is <strong>invent a chain</strong>. Guessing
      that one call feeds another is unreliable in exactly the cases that matter, and
      a wrong chain is worse than none: it runs cleanly while testing a flow the
      service does not have. Every generated chain is one step long, and the todo
      list says where a sequence probably belongs.</p>

      <p>A generated plan stays marked as a draft until somebody says otherwise. It
      runs — that is the point of generating one — but its weights are an even split
      and its volume is a placeholder, and a provisional mixture that has stopped
      looking provisional is how a guess gets quoted as a measurement.</p>`,
  };
}

/* -------------------------------------------------------------------- the list */

function list(state) {
  const broken = state.brokenPlans.length
    ? `<div class="alert alert-warning" role="alert">
         <div class="d-flex">
           <div class="me-3">${icon("alert-triangle")}</div>
           <div>
             <h4 class="alert-title">Plans that will not parse</h4>
             <div class="text-secondary">Listed rather than dropped, for the same
               reason a broken profile is: a plan that vanishes from this page
               because it has a typo is one nobody can find in order to fix it.</div>
             <ul class="mb-0 mt-2">${state.brokenPlans
               .map((p) => `<li><code>${escape(p.name)}</code> — ${escape(p.error)}</li>`)
               .join("")}</ul>
           </div>
         </div>
       </div>`
    : "";

  if (!state.plans.length) {
    return (
      broken +
      empty({
        icon: "list-check",
        title: "No plans yet",
        body: `A plan is a directory holding a mixture and the calls it invokes. Drop
          one into the plans directory, or unpack an exported bundle into it — the
          two are the same shape on purpose.`,
        action: `<div class="btn-list justify-content-center">
                   <button class="btn btn-primary" data-action="plan-new">
                     ${icon("plus")} New plan
                   </button>
                   ${reloadButton(state.plansReadAt)}
                 </div>`,
      })
    );
  }

  return `<div class="metrix-stack">
    ${broken}
    <div class="metrix-toolbar">
      <p class="metrix-note text-secondary mb-0">
        The mixture is editable; the calls it invokes are not. A plan carries no
        targets — those come from a profile when the bundle is assembled, which is
        why one plan runs against staging and production with no edit.
      </p>
      <div class="btn-list">
        ${reloadButton(state.plansReadAt)}
        <button class="btn btn-primary" data-action="plan-new">
          ${icon("plus")} New plan
        </button>
      </div>
    </div>
    ${state.plans.map((plan) => planCard(plan)).join("")}
  </div>`;
}

/**
 * Re-read the plans directory.
 *
 * Plans are files, and the file is the primary form: someone who has just written
 * one, generated one, or pulled a change from version control has no reason to
 * reload the page. Not polled, for the same reason the profile list is not — nothing
 * here changes on its own, and replacing what somebody is reading, unasked, is worse
 * than a button.
 */
function reloadButton(readAt) {
  const when = readAt
    ? `<span class="text-secondary ms-2">read ${escape(
        new Date(readAt).toLocaleTimeString()
      )}</span>`
    : "";
  return `<button class="btn" data-action="plan-reload"
                  title="Re-read the plan files from disk">
    ${icon("refresh")} Reload ${when}
  </button>`;
}

function planCard(plan) {
  const name = escape(plan.name);
  const rows = plan.chains
    .map(
      (chain, index) => `<tr>
        <td class="name">${escape(chain.name ?? "")}</td>
        <td class="num">${chain.percent ?? "—"}%</td>
        <td class="num">${escape(implied(plan.figures.chains[index]))}</td>
        <td class="num">${escape(requestCount(plan.figures.chains[index]))}</td>
      </tr>`
    )
    .join("");

  return `<div class="card">
    <div class="card-header">
      <div>
        <h3 class="card-title">${name}</h3>
        <div class="card-subtitle">${escape(shape(plan.figures))} ·
          ${count(plan.chains.length, "chain")} ·
          ${count(plan.calls.length, "call")}</div>
      </div>
      <div class="card-actions d-flex align-items-center gap-2">
        ${plan.draft ? `<span class="badge bg-purple-lt" title="Generated and not yet reviewed">draft</span>` : ""}
        ${verdictBadge(plan)}
        <button class="btn btn-sm" data-action="plan-edit" data-plan="${name}">
          ${icon("pencil")} Edit
        </button>
      </div>
    </div>
    ${
      plan.chains.length
        ? `<div class="table-responsive">
             <table class="table card-table table-vcenter metrix-table">
               <thead><tr>
                 <th style="width:40%">Chain</th>
                 <th class="num" style="width:15%">Share</th>
                 <th class="num" style="width:22%">Implied</th>
                 <th class="num" style="width:23%">Requests</th>
               </tr></thead>
               <tbody>${rows}</tbody>
             </table>
           </div>`
        : ""
    }
  </div>`;
}

/** The one-line shape of a run: how much load, for how long, in what model. */
function shape(figures) {
  const rate =
    figures.rate == null ? figures.mode : `${figures.mode} · ${round(figures.rate)}/s`;
  const duration = figures.duration_s == null ? "no duration" : seconds(figures.duration_s);
  return `${rate} · ${duration} · ${figures.model}`;
}

function verdictBadge(plan) {
  const errors = plan.problems.filter((p) => p.severity === "error").length;
  const warnings = plan.problems.length - errors;
  if (errors) {
    return `<span class="badge bg-red-lt" title="This plan will not assemble into a bundle">
      ${count(errors, "error")}</span>`;
  }
  if (warnings) {
    return `<span class="badge bg-yellow-lt">${count(warnings, "warning")}</span>`;
  }
  return `<span class="badge bg-green-lt">ready</span>`;
}

/**
 * Paste a description; get a plan.
 *
 * The document is pasted rather than fetched from a URL. A control plane that
 * retrieves whatever address it is handed is a request forwarder sitting inside
 * somebody's network, which is a larger thing than a plan generator and a decision
 * nobody made when they asked for a skeleton.
 */
function describe(panel) {
  const error = panel.error
    ? `<div class="alert alert-danger" role="alert">
         <div class="d-flex">
           <div class="me-3">${icon("alert-triangle")}</div>
           <div>
             <h4 class="alert-title">${escape(
               panel.mode === "regenerate" ? "Not regenerated" : "Not generated"
             )}</h4>
             <div>${escape(panel.error)}</div>
           </div>
         </div>
       </div>`
    : "";

  const chosen = panel.source ?? SOURCES[0][0];
  const described = SOURCES.find(([value]) => value === chosen)?.[2] ?? "";

  return `<form class="metrix-stack" data-describe-form novalidate>
    ${error}
    <div class="card">
      <div class="card-header">
        <div>
          <h3 class="card-title">${
            panel.mode === "regenerate" ? "Regenerate calls" : "Generate a plan"
          }</h3>
          <div class="card-subtitle">${
            panel.mode === "regenerate"
              ? "Read the service again and replace the calls. The mixture is left alone — that is what the two documents are for."
              : "The mechanical half: one call per operation, assertions from the codes the service declares. Weights, chains and judgment stay with you."
          }</div>
        </div>
        <div class="card-actions d-flex align-items-center gap-2">
          <button type="button" class="btn btn-sm" data-action="describe-cancel">Cancel</button>
          <button type="button" class="btn btn-sm btn-primary" data-action="describe-run">
            ${icon("list-check")} ${panel.mode === "regenerate" ? "Replace calls" : "Generate"}
          </button>
        </div>
      </div>
      <div class="card-body">
        <div class="metrix-fields">
          ${
            panel.mode === "regenerate"
              ? ""
              : text("describe.name", "Plan name", panel.name, {
                  required: true,
                  hint: "Letters, digits, dot, dash, underscore. Used as the directory name.",
                })
          }
          <div class="metrix-field">
            <label class="form-label" for="f-describe.source">Source</label>
            <select class="form-select" id="f-describe.source" name="describe.source">
              ${SOURCES.map(
                ([value, label]) =>
                  `<option value="${escape(value)}"${value === chosen ? " selected" : ""}>
                     ${escape(label)}
                   </option>`
              ).join("")}
            </select>
            <div class="form-hint">${escape(described)}</div>
          </div>
        </div>
        <div class="mt-3">
          <label class="form-label" for="f-describe.content">The document</label>
          <textarea class="form-control metrix-paste" id="f-describe.content"
                    name="describe.content" rows="16" spellcheck="false"
                    placeholder="Paste it here.">${escape(panel.content ?? "")}</textarea>
          <div class="form-hint">Pasted, not fetched: nothing here reaches out to a
            URL on your behalf.</div>
        </div>
      </div>
    </div>
  </form>`;
}

/** Read the generator form back. Three fields, so no merge to do. */
export function readDescribe(form) {
  const data = new FormData(form);
  const value = (name) => (data.get(name) ?? "").toString();
  return {
    name: value("describe.name").trim(),
    source: value("describe.source").trim(),
    content: value("describe.content"),
  };
}

/* ------------------------------------------------------------------ the editor */

function editor(state) {
  const draft = state.planDraft;
  const doc = draft.doc;
  const check = state.planCheck;
  const calls = draft.schemaCalls ?? draft.detail?.call_details ?? [];
  const canEditCalls = basicCompatible(doc, calls);

  const error = draft.error
    ? `<div class="alert alert-danger" role="alert">
         <div class="d-flex">
           <div class="me-3">${icon("alert-triangle")}</div>
           <div>
             <h4 class="alert-title">Not saved</h4>
             <div>${escape(draft.error)}</div>
           </div>
         </div>
       </div>`
    : "";

  return `<form class="metrix-stack metrix-plan-editor" data-plan-form
               data-editor-mode="${canEditCalls ? "basic" : "load"}" novalidate>
      ${error}
    <div class="card">
      <div class="card-header">
        <div>
          <h3 class="card-title">${escape(draft.name)}</h3>
        </div>
        <div class="card-actions d-flex align-items-center gap-2">
          <button type="button" class="btn btn-sm" data-action="plan-cancel">Close</button>
          <button type="button" class="btn btn-sm btn-primary" data-action="plan-save">
            ${icon("device-floppy")} Save changes
          </button>
        </div>
      </div>
      ${draft.mode === "create" ? `<div class="card-body border-top">${text(
        "plan.name", "Plan name", draft.name, {
          required: true,
          hint: "Letters, digits, dot, dash, underscore. Used as the directory name.",
        }
      )}</div>` : ""}
      ${draft.mode === "create" ? schemaCard(state, draft, calls) : ""}
    </div>

    <div class="metrix-plan-workspace${canEditCalls ? "" : " metrix-plan-load-only"}">
      ${canEditCalls ? basicTable(doc, check, draft) : complexChainsNote(doc)}
      ${loadCard(doc, check, canEditCalls)}
    </div>
    <div class="card">
      <div class="card-body" id="plan-verdict">${verdict(check)}</div>
      ${notes(draft.detail?.notes)}
      <div class="card-body border-top text-secondary metrix-plan-explanation">
        ${canEditCalls
          ? "Each row is one call. Its RPS determines the stored rate and percentages. Calls themselves remain read-only."
          : "This plan's chains are carried unchanged when Load is saved. Calls themselves remain read-only."}
        Targets come from a profile when the bundle is assembled.
      </div>
    </div>
    ${draftCard(draft)}
    ${exportCard(state, draft)}
    ${callsCard(draft)}
    ${carriedCard(doc)}
  </form>`;
}

function schemaCard(state, draft, calls) {
  const schemas = state.schemas ?? [];
  const selected = new Set(draft.selectedCalls ?? []);
  const options = schemas.length
    ? schemas.map((schema) => `<option value="${escape(schema.id)}"${schema.id === draft.schemaId ? " selected" : ""}>
        ${escape(schema.filename)} · ${escape(schema.source)} · ${count(schema.call_count, "endpoint")}
      </option>`).join("")
    : `<option value="">No stored schemas — upload one in Schemas first</option>`;
  const endpointRows = calls.length
    ? calls.map((call) => `<label class="form-check mb-2">
        <input class="form-check-input" type="checkbox" data-schema-call="${escape(call.name)}"
               ${selected.has(call.name) ? "checked" : ""}>
        <span class="form-check-label"><code>${escape(call.name)}</code>
          <span class="text-secondary ms-2">${escape(call.method)} ${escape(call.path)}</span>
        </span>
      </label>`).join("")
    : `<div class="text-secondary">Choose a stored schema to see its endpoints.</div>`;
  return `<div class="card border-top-0 rounded-0">
    <div class="card-body">
      <div class="metrix-field">
        <label class="form-label" for="f-plan.schema">Schema</label>
        <select class="form-select" id="f-plan.schema" name="plan.schema">
          <option value="">Choose an existing schema</option>${options}
        </select>
        <div class="form-hint">Only endpoints checked below become calls in this plan.</div>
      </div>
      <div class="mt-3"><div class="form-label">Endpoints</div>${endpointRows}</div>
    </div>
  </div>`;
}

/**
 * Things true of the stored form that are not problems with the mixture.
 *
 * A `targets.json` left in a plan directory by a re-imported bundle is the one that
 * matters: it is ignored, targets come from the profile, and a file that is silently
 * ignored is a file somebody will edit expecting it to count.
 */
function notes(list) {
  if (!list?.length) return "";
  return `<div class="card-body border-top text-secondary">
    <ul class="mb-0">${list.map((note) => `<li>${escape(note)}</li>`).join("")}</ul>
  </div>`;
}

/**
 * What is wrong with the mixture, and whether it can run.
 *
 * Rendered from the server's answer and patched in place as fields are committed, so
 * it is never the browser's opinion of the document and never a stale one. Errors
 * and warnings are kept apart because they mean different things: one stops the run,
 * the other says the run will happen and produce a number nobody should quote.
 */
export function verdict(check) {
  if (!check) {
    return `<div class="text-secondary">
      <span class="spinner-border spinner-border-sm me-2" role="status"></span>Checking…
    </div>`;
  }
  const errors = check.problems.filter((p) => p.severity === "error");
  const warnings = check.problems.filter((p) => p.severity === "warning");

  const head = check.ready
    ? `<span class="badge bg-green-lt">ready to run</span>
       <span class="text-secondary">this mixture assembles into a bundle</span>`
    : `<span class="badge bg-red-lt">will not run</span>
       <span class="text-secondary">the bundle refuses to assemble until these are
         fixed — the engine would reject the same document a moment later</span>`;

  return `<div class="d-flex align-items-baseline gap-2 flex-wrap">${head}</div>
    ${problemList(errors, "severity-invalid")}
    ${problemList(warnings, "severity-warn")}
    <div class="mt-2">${implication(check.figures)}</div>`;
}

function problemList(problems, className) {
  if (!problems.length) return "";
  return `<ul class="mt-2 mb-0">${problems
    .map(
      (problem) => `<li><code class="${className}">${escape(problem.where)}</code>
        — ${escape(problem.message)}</li>`
    )
    .join("")}</ul>`;
}

/** The run in numbers: what the rate and the duration come to, against the floor. */
function implication(figures) {
  if (!figures) {
    return `<span class="text-secondary">No figures: the mixture does not parse, so
      there is nothing to compute them from.</span>`;
  }
  if (figures.requests == null) {
    return `<span class="text-secondary">No request count: ${escape(
      figures.withheld ?? "the mixture does not imply one"
    )}. Nothing is estimated here — an invented sample count is the one number this
    tool must not print.</span>`;
  }
  const verdictText = figures.supported
    ? `above the ${figures.floor} a tail percentile needs`
    : `<span class="severity-warn">below the ${figures.floor} a tail percentile
       needs</span>`;
  return `<span class="text-secondary">About <strong>${figures.requests.toLocaleString()}</strong>
    requests — ${figures.iterations.toLocaleString()} iterations over
    ${seconds(figures.measured_s)} of measured traffic — ${verdictText}.</span>`;
}

function basicTable(doc, check, draft) {
  const chains = doc.chains ?? [];
  const calls = draft.schemaCalls ?? draft.detail?.call_details ?? [];
  const totalRate = check?.figures?.rate;
  return `<div class="card metrix-basic-card">
    <div class="card-header">
      <div>
        <h3 class="card-title">Calls</h3>
        <div class="card-subtitle" id="plan-basic-total"><strong>${round(totalRate) || "—"} RPS</strong> total ·
          ${count(chains.length, "call")}</div>
      </div>
      <div class="card-actions">
        <button type="button" class="btn btn-sm" data-action="basic-row-add"
                ${calls.length ? "" : "disabled"}>${icon("plus")} Add call</button>
      </div>
    </div>
    <div class="table-responsive">
      <table class="table card-table table-vcenter metrix-basic-table">
        <thead><tr>
          <th style="width:18%">Chain name</th>
          <th style="width:29%">Call</th>
          <th class="num" style="width:12%">RPS</th>
          <th style="width:13%">Expect</th>
          <th style="width:18%">Request type</th>
          <th class="text-end" style="width:10%">Remove</th>
        </tr></thead>
        <tbody>${chains
          .map((chain, index) => basicRow(
            chain,
            index,
            check?.figures?.chains?.[index]?.iterations_per_s ??
              (Number(doc.load?.rate ?? 0) * Number(chain.percent ?? 0) / 100),
            calls,
            chains.length
          ))
          .join("")}</tbody>
      </table>
    </div>
  </div>`;
}

function basicRow(chain, index, rps, calls, chainCount) {
  const selected = chain.steps?.[0]?.call ?? "";
  const detail = calls.find((call) => call.name === selected);
  const type = requestType(detail);
  const availableTypes = new Set(calls.map(requestType));
  const callOptions = calls
    .map(
      (call) => `<option value="${escape(call.name)}" data-request-type="${requestType(call)}"
        ${call.name === selected ? "selected" : ""}>${escape(call.name)} · ${escape(
          call.path
        )}</option>`
    )
    .join("");
  const typeOptions = REQUEST_TYPES.map(
    (value) => `<option value="${value}"${value === type ? " selected" : ""}
      ${availableTypes.has(value) ? "" : "disabled"}>${requestTypeLabel(value)}</option>`
  ).join("");
  return `<tr>
    <td><input class="form-control" name="basic.${index}.name" readonly
               value="${escape(chain.name ?? "")}" aria-label="Chain name"></td>
    <td><select class="form-select" name="basic.${index}.call"
                aria-label="Call">${callOptions}</select></td>
    <td><input class="form-control text-end" type="number" step="any" min="0"
               name="basic.${index}.rps" value="${escape(round(rps))}"
               aria-label="Requests per second"></td>
    <td><span class="metrix-basic-expect">${escape(expectedStatus(detail))}</span></td>
    <td><select class="form-select" name="basic.${index}.type"
                data-basic-type data-index="${index}" aria-label="Request type">
          ${typeOptions}
        </select></td>
    <td class="text-end text-nowrap">
      <button type="button" class="btn btn-sm btn-icon btn-outline-danger"
              data-action="basic-row-remove" data-index="${index}"
              title="Remove call" aria-label="Remove call"
              ${chainCount === 1 ? "disabled" : ""}>${icon("trash")}</button>
    </td>
  </tr>`;
}

function complexChainsNote(doc) {
  const chains = doc.chains ?? [];
  return `<div class="card">
    <div class="card-header"><h3 class="card-title">Chain details</h3></div>
    <div class="card-body text-secondary">
      This plan has ${count(chains.length, "chain")} with details the call table cannot edit.
      The chain definitions stay in the stored plan and its exported bundle when Load is saved.
    </div>
  </div>`;
}

function expectedStatus(detail) {
  const assertion = detail?.assert?.find((item) => "status" in item || "status_in" in item);
  if (!assertion) return "—";
  if ("status" in assertion) return String(assertion.status);
  return assertion.status_in.join(", ");
}

function requestType(detail) {
  if (!detail) return "none";
  const contentType = Object.entries(detail.headers ?? {}).find(
    ([name]) => name.toLowerCase() === "content-type"
  )?.[1];
  const source = String(contentType ?? detail.body ?? "").toLowerCase();
  if (source.includes("json")) return "json";
  if (source.includes("xml")) return "xml";
  if (source.includes("html")) return "html";
  if (source.includes("form")) return "form";
  if (source.includes("text")) return "text";
  if (source.includes("generator")) return "generated";
  if (!source || source === "none") return "none";
  return "other";
}

function requestTypeLabel(value) {
  return {
    json: "JSON",
    xml: "XML",
    html: "HTML",
    form: "Form",
    text: "Text",
    generated: "Generated",
    none: "No body",
    other: "Other",
  }[value];
}

function loadCard(doc, check, canEditCalls) {
  const load = doc.load ?? {};
  const phases = doc.phases ?? {};
  const mode = load.mode ?? "fixed";
  const modeOptions = MODES.map((value) => {
    const unavailable = value !== "fixed" && value !== mode;
    return `<option value="${value}"${value === mode ? " selected" : ""}
      ${unavailable ? "disabled" : ""}>
      ${value}${value === "fixed" ? "" : " (not implemented)"}
    </option>`;
  }).join("");

  return `<div class="card">
    <div class="card-header"><h3 class="card-title">Load</h3></div>
    <div class="card-body">
      <div class="metrix-fields metrix-load-fields">
        <div class="metrix-field metrix-load-mode">
          <label class="form-label" for="f-load.mode">Mode</label>
          <select class="form-select" id="f-load.mode" name="load.mode">${modeOptions}</select>
          <div class="form-hint">Stages and breakpoint are not implemented in the editor yet.</div>
        </div>
        ${text("load.duration", "Duration", load.duration, {
          required: true,
          hint: 'With units: "30s", "5m", "1h30m".',
        })}
        ${!canEditCalls && mode === "fixed"
          ? number("load.rate", "Total RPS", load.rate, {
              hint: "Combined rate of all chains in the exported bundle.",
            })
          : ""}
        ${text("load.warmup", "Warmup", load.warmup, {
          hint: "Measured separately and excluded from the summary. Blank for none.",
        })}
        ${text("phases.settle", "Settle", phases.settle, {
          hint: "Observed with no traffic, after it: recovery.",
        })}
        ${text("phases.baseline", "Baseline", phases.baseline, {
          hint: "Observed with no traffic, before the run: initial conditions.",
        })}
        ${number("load.max_concurrency", "Concurrency cap", load.max_concurrency, {
          hint: "In-flight requests. Hitting it annotates the run rather than failing it.",
        })}
      </div>
      ${canEditCalls
        ? '<div class="mt-2 text-secondary">Total RPS is calculated from the call rates.</div>'
        : ""}
      <div class="mt-3" id="plan-implied">${
        check
          ? implication(check.figures)
          : '<span class="text-secondary">Checking…</span>'
      }</div>
    </div>
  </div>`;
}

function chainsCard(doc, check, draft) {
  const chains = doc.chains ?? [];
  const calls = draft.schemaCalls ?? draft.detail?.call_details ?? [];

  return `<div class="card">
    <div class="card-header">
      <div>
        <h3 class="card-title">Chains</h3>
        <div class="card-subtitle" id="plan-total">${total(check)}</div>
      </div>
      <div class="card-actions">
        <button type="button" class="btn btn-sm" data-action="chain-add"
                ${calls.length ? "" : "disabled"}>
          ${icon("plus")} Add chain
        </button>
      </div>
    </div>
    ${
      chains.length
        ? chains.map((chain, index) => chainCard(chain, index, check, calls)).join("")
        : `<div class="card-body text-secondary">No chains: this mixture sends
             nothing. Add one and pick the call it invokes.</div>`
    }
  </div>`;
}

/** The running total, from the server. Never renormalized, only named. */
function total(check) {
  if (!check) return "…";
  if (!check.figures) return "Shares cannot be totalled until the mixture parses.";
  const value = check.figures.percent_total;
  const gap = 100 - value;
  if (Math.abs(gap) < 0.01) return `Shares total 100%.`;
  const direction = gap > 0 ? `${round(gap)} short of` : `${round(-gap)} over`;
  return `<span class="severity-invalid">Shares total ${round(
    value
  )}% — ${direction} 100.</span>`;
}

function chainCard(chain, index, check, calls) {
  const figures = check?.figures?.chains[index];
  const at = (field) => `chains.${index}.${field}`;
  const session = chain.session ?? "reuse";
  const steps = chain.steps ?? [];

  return `<div class="card-body border-top">
    <div class="metrix-toolbar mb-2">
      <strong>Chain ${index + 1}</strong>
      <button type="button" class="btn btn-sm btn-outline-danger"
              data-action="chain-remove" data-index="${index}">
        ${icon("trash")} Remove chain
      </button>
    </div>
    <div class="metrix-fields metrix-chain-fields">
      ${text(at("name"), "Name", chain.name, {
        required: true,
        hint: "Keys every chart series, error report and SLO.",
      })}
      ${number(at("percent"), "Share %", chain.percent, {
        readOnly: true,
        hint: "Calculated from this chain's RPS.",
      })}
      ${number(at("rps"), "Iterations/s", figures?.iterations_per_s, {
        hint: "The load rate is the sum of every chain's value.",
      })}
      ${select(at("session"), "Session", SESSIONS, session, {
        hint: "fresh is a first-time user, reuse a returning one, pool a population.",
      })}
      ${
        session === "pool"
          ? number(at("pool_size"), "Pool size", chain.pool_size, {
              hint: "How many sessions are cycled across iterations.",
            })
          : ""
      }
    </div>
    <div class="mt-2 text-secondary metrix-chain-implied" data-chain="${index}">
      ${chainImplied(figures)}
    </div>
    <div class="mt-3">
      ${steps.map((step, stepIndex) => stepRow(step, index, stepIndex, steps, calls)).join("")}
      <button type="button" class="btn btn-sm mt-2" data-action="step-add"
              data-index="${index}">${icon("plus")} Add step</button>
    </div>
  </div>`;
}

/** What one chain's share comes to, and whether its own percentiles will stand up. */
function chainImplied(figures) {
  if (!figures || figures.requests == null) {
    return "No implied rate until the mixture has one.";
  }
  const supported = figures.supported
    ? ""
    : ` <span class="severity-warn">— below the floor, so its percentiles will be
        withheld</span>`;
  return `${round(figures.iterations_per_s)} iterations/s ×
    ${count(figures.steps, "step")} = <strong>${round(figures.requests_per_s)}
    req/s</strong>, about ${figures.requests.toLocaleString()} requests${supported}`;
}

function stepRow(step, chainIndex, index, steps, calls) {
  const at = (field) => `chains.${chainIndex}.steps.${index}.${field}`;
  const detail = calls.find((call) => call.name === step.call);
  const move = (direction, disabled, label) =>
    `<button type="button" class="btn btn-sm btn-icon" data-action="step-move"
             data-index="${chainIndex}" data-step="${index}" data-move="${direction}"
             title="${label}" ${disabled ? "disabled" : ""}>${icon(
               direction === "up" ? "chevron-up" : "chevron-down"
             )}</button>`;

  return `<div class="card card-sm mb-2">
    <div class="card-body">
      <div class="metrix-toolbar mb-2">
        <span class="text-secondary">Step ${index + 1}${
          detail?.description ? ` — ${escape(detail.description)}` : ""
        }</span>
        <div class="btn-list">
          ${move("up", index === 0, "Move earlier")}
          ${move("down", index === steps.length - 1, "Move later")}
          <button type="button" class="btn btn-sm btn-icon btn-outline-danger"
                  data-action="step-remove" data-index="${chainIndex}"
                  data-step="${index}" title="Remove step">${icon("trash")}</button>
        </div>
      </div>
      <div class="metrix-fields">
        ${text(at("id"), "Step id", step.id, {
          required: true,
          hint: "How this step's own latency is reported. Independent of the call.",
        })}
        ${select(
          at("call"),
          "Call",
          calls.map((call) => call.name),
          step.call,
          { hint: "One of the calls this plan defines." }
        )}
      </div>
      ${callLine(detail)}
      ${repeatFields(step, at)}
    </div>
  </div>`;
}

/** What the chosen call actually does, beside the step that chose it. */
function callLine(detail) {
  if (!detail) return "";
  const uses = (detail.uses ?? []).length
    ? ` · reads ${(detail.uses ?? []).map((u) => `<code>${escape(u)}</code>`).join(", ")}`
    : "";
  const extracts = (detail.extracts ?? []).length
    ? ` · provides ${(detail.extracts ?? []).map((e) => `<code>${escape(e)}</code>`).join(", ")}`
    : "";
  return `<div class="mt-2 text-secondary">
    <span class="badge bg-secondary-lt">${escape(detail.method)}</span>
    <code class="metrix-path ms-1">${escape(detail.path)}</code>${uses}${extracts}
  </div>`;
}

/**
 * The async-job pattern: a step that polls until the job it started is done.
 *
 * Behind a checkbox because most steps are not this, and four more fields on every
 * step would bury the two that matter. Polling time is recorded separately by the
 * engine so it does not contaminate the request latency.
 */
function repeatFields(step, at) {
  const repeat = step.repeat_until;
  const enabled = Boolean(repeat);
  const selector = repeat
    ? ["json", "xpath", "header", "regex"].find((kind) => kind in repeat) ?? "json"
    : "json";

  const toggle = `<label class="form-check form-check-inline mt-2">
    <input class="form-check-input" type="checkbox" name="${at("repeat")}"
           ${enabled ? "checked" : ""}>
    <span class="form-check-label">Poll until a value appears</span>
  </label>`;

  if (!enabled) return toggle;

  return `${toggle}
    <div class="metrix-fields mt-2">
      ${select(at("repeat.selector"), "Read from", ["json", "xpath", "header", "regex"],
        selector, { hint: "How the value is found in the response." })}
      ${text(at("repeat.expression"), "Expression", repeat[selector], {
        hint: 'For JSON, a path like "$.status".',
      })}
      ${text(at("repeat.equals"), "Until it equals", repeat.equals, {
        hint: "The terminal value that ends the polling.",
      })}
      ${number(at("repeat.max_attempts"), "Max attempts", repeat.max_attempts, {
        hint: "Polling stops here whether or not the value arrived.",
      })}
      ${number(at("repeat.interval_ms"), "Interval (ms)", repeat.interval_ms, {
        hint: "Between attempts.",
      })}
    </div>`;
}

/**
 * What is still the generator's guesswork, as a list of things to decide.
 *
 * A checklist rather than a paragraph, because that is what it is: a generated plan
 * is a handover, and the todos are the note that came with it. Marking it reviewed is
 * its own action and not a side effect of saving — editing one percentage is not a
 * review, and a flag that cleared itself on the first edit would mark every generated
 * plan reviewed a minute after it was opened.
 */
function draftCard(draft) {
  const marker = draft.detail?.draft;
  if (!marker) return "";
  const todos = marker.todos ?? [];
  return `<div class="card">
    <div class="card-header">
      <div>
        <h3 class="card-title">
          <span class="badge bg-purple-lt me-2">draft</span>Generated from ${escape(
            marker.source
          )}
        </h3>
        <div class="card-subtitle">${
          marker.observed_weights
            ? "The weights came from counted traffic. Everything else below is still this tool's."
            : "The weights are an even split: nothing in a service description says what it actually gets asked for."
        }</div>
      </div>
      <div class="card-actions">
        <button type="button" class="btn btn-sm" data-action="draft-accept"
                title="Stop marking this plan as unreviewed">
          ${icon("check")} Mark reviewed
        </button>
      </div>
    </div>
    ${
      todos.length
        ? `<div class="card-body">
             <ul class="metrix-todos">${todos
               .map(
                 (todo) => `<li><code>${escape(todo.where)}</code> — ${escape(todo.message)}</li>`
               )
               .join("")}</ul>
           </div>`
        : ""
    }
  </div>`;
}

/**
 * The bundle: this plan against one profile's boxes.
 *
 * A profile is required rather than optional, because the only thing the API adds to
 * a stored plan is `targets.json` and it is the profile that knows what is actually
 * behind a hostname. The plan hash is shown before the download: it is what the
 * engine will call this plan, and what every run of it is grouped under.
 */
function exportCard(state, draft) {
  const profiles = state.profiles ?? [];
  const chosen = draft.profile ?? profiles[0]?.name ?? "";
  // A bundle is assembled from the plan on disk, because that is the plan a run
  // would use. Saying so beside the button is the difference between an export that
  // is out of date and an export that lies about what is in it.
  const unsaved = dirty(draft)
    ? `<div class="mt-3 severity-warn">The form has changes that are not saved. A run
         and a bundle are both assembled from the stored plan, so they would not
         include them.</div>`
    : "";

  if (!profiles.length) {
    return `<div class="card">
      <div class="card-header"><h3 class="card-title">Run it</h3></div>
      <div class="card-body text-secondary">A run needs a profile: the plan says what
        to send, and the profile says where. Make one on the Profiles page.</div>
    </div>`;
  }

  return `<div class="card">
    <div class="card-header">
      <div>
        <h3 class="card-title">Run it</h3>
        <div class="card-subtitle">The runnable directory: this mixture, its calls,
          and one profile's boxes as <code>targets.json</code>. Run it here, or take
          the same directory away and run it by hand.</div>
      </div>
    </div>
    <div class="card-body">
      <div class="metrix-fields">
        ${select("bundle.profile", "Against", profiles.map((p) => p.name), chosen, {
          hint: "Its endpoints become the targets. The plan stores none of its own.",
        })}
      </div>
      <div class="btn-list mt-3">
        <button type="button" class="btn btn-primary" data-action="plan-run">
          ${icon("player-play")} Run against ${escape(chosen)}
        </button>
        <button type="button" class="btn" data-action="bundle-preview">
          ${icon("clipboard")} Show what it contains
        </button>
        <button type="button" class="btn" data-action="bundle-download">
          ${icon("download")} Download zip
        </button>
      </div>
      ${unsaved}
      ${bundlePreview(draft.preview)}
    </div>
  </div>`;
}

/** Whether the form holds anything the stored plan does not. */
function dirty(draft) {
  return JSON.stringify(draft.doc) !== JSON.stringify(draft.saved ?? draft.doc);
}

function bundlePreview(preview) {
  if (!preview) return "";
  return `<div class="mt-3">
    <div class="text-secondary">
      Plan hash <code>${escape(preview.plan_hash)}</code> — computed over the
      ${count(preview.hashed.length, "hashed document")} by the engine's own rule, so
      this is what the run will be identified by before it starts.
    </div>
    <ul class="mt-2 mb-0">${Object.keys(preview.files)
      .map(
        (path) => `<li><code>${escape(path)}</code>${
          preview.hashed.includes(path)
            ? ""
            : ` <span class="text-secondary">carried, not hashed</span>`
        }</li>`
      )
      .join("")}</ul>
  </div>`;
}

/**
 * The calls, read-only (§20.3).
 *
 * Enough to understand what a chain does without opening a file: what each request
 * is, what it reads out of the response, and what it asserts — which is where a
 * chain like `login-fail` explains itself as expecting a 401.
 */
function callsCard(draft) {
  if (draft.mode === "create") return "";
  const calls = draft.schemaCalls ?? draft.detail?.call_details ?? [];
  if (!calls.length) return "";

  const rows = calls
    .map(
      (call) => `<tr>
        <td class="name">${escape(call.name)}
          ${call.description ? `<div class="text-secondary">${escape(call.description)}</div>` : ""}
        </td>
        <td><span class="badge bg-secondary-lt">${escape(call.method)}</span>
          <code class="metrix-path ms-1">${escape(call.path)}</code>
          ${call.body === "none" ? "" : `<div class="text-secondary">body: ${escape(call.body)}</div>`}
        </td>
        <td>${
          call.uses.length
            ? call.uses.map((u) => `<code>${escape(u)}</code>`).join("<br>")
            : "—"
        }</td>
        <td>${
          call.extracts.length
            ? call.extracts.map((e) => `<code>${escape(e)}</code>`).join("<br>")
            : "—"
        }</td>
        <td>${assertions(call.assert)}</td>
      </tr>`
    )
    .join("");

  return `<div class="card">
    <div class="card-header">
      <div>
        <h3 class="card-title">Calls</h3>
        <div class="card-subtitle">Read-only. Request definitions are authored from
          the codebase or generated from a schema; hand-editing one in a browser is
          how a plan drifts from the service it describes. Change them in the bundle,
          or read the service again.</div>
      </div>
      <div class="card-actions">
        <button type="button" class="btn btn-sm" data-action="calls-regenerate"
                title="Replace the calls from a newer description, leaving the mixture alone">
          ${icon("refresh")} Regenerate from a description
        </button>
      </div>
    </div>
    <div class="table-responsive">
      <table class="table card-table table-vcenter metrix-table">
        <thead><tr>
          <th style="width:22%">Call</th>
          <th style="width:26%">Request</th>
          <th style="width:14%">Reads</th>
          <th style="width:14%">Provides</th>
          <th style="width:24%">Asserts</th>
        </tr></thead>
        <tbody>${rows}</tbody>
      </table>
    </div>
  </div>`;
}

/** An assertion in the words it was written in, not in a summary of them. */
function assertions(list) {
  if (!list?.length) return '<span class="text-secondary">nothing</span>';
  return list
    .map((one) => {
      const parts = Object.entries(one).map(
        ([key, value]) => `${key} ${typeof value === "object" ? JSON.stringify(value) : value}`
      );
      return `<div><code>${escape(parts.join(", "))}</code></div>`;
    })
    .join("");
}

/**
 * Everything the form does not draw but the file carries.
 *
 * Listed because it is preserved: the editor writes back the document it loaded with
 * the fields it owns replaced, so an auth block or a dataset survives a save it was
 * never shown in. Saying so is the difference between "preserved" and "apparently
 * gone".
 */
function carriedCard(doc) {
  const carried = [
    ["auth", doc.auth && `${doc.auth.mode}`],
    ["datasets", doc.datasets && count(Object.keys(doc.datasets).length, "dataset")],
    ["generators", doc.generators && count(Object.keys(doc.generators).length, "generator")],
    ["slo", doc.slo?.length && count(doc.slo.length, "threshold")],
    ["defaults", doc.defaults && count(Object.keys(doc.defaults).length, "field")],
    ["capture", doc.capture && "error samples and redaction"],
    ["observe", doc.observe && "host collection"],
    ["engine", doc.engine && "threading and connections"],
    ["load.stages", doc.load?.stages?.length && count(doc.load.stages.length, "stage")],
    ["load.breakpoint", doc.load?.breakpoint && "search parameters"],
  ].filter(([, value]) => value);

  if (!carried.length) return "";

  return `<div class="card">
    <div class="card-header"><h3 class="card-title">Carried unchanged</h3></div>
    <div class="card-body">
      <p class="metrix-note text-secondary">Not editable here, and not lost either: a
        save writes back the document it loaded with only the fields above replaced.
        Edit these in the file, or in an exported bundle.</p>
      <ul class="mb-0">${carried
        .map(([key, value]) => `<li><code>${escape(key)}</code> — ${escape(value)}</li>`)
        .join("")}</ul>
    </div>
  </div>`;
}

/* ------------------------------------------------------------------- patching */

// The shape the form on screen was built for. A committed field changes numbers; a
// changed session policy, load mode or step list changes which controls exist, and
// only the second needs the markup rebuilt.
let rendered = null;

function signature(state) {
  const draft = state.planDraft;
  if (!draft) return null;
  return JSON.stringify([
    draft.name,
    draft.error ?? null,
    draft.profile ?? null,
    draft.preview?.plan_hash ?? null,
    dirty(draft),
    Boolean(draft.detail?.draft),
    (state.profiles ?? []).map((profile) => profile.name),
    draft.doc.load?.mode ?? "fixed",
    (draft.detail?.call_details ?? []).map((call) => call.name),
    (draft.doc.chains ?? []).map((chain) => [
      chain.session ?? "reuse",
      (chain.steps ?? []).map((step) => [
        step.call,
        step.repeat_until
          ? ["json", "xpath", "header", "regex"].find((kind) => kind in step.repeat_until)
          : null,
      ]),
    ]),
  ]);
}

/**
 * Update the numbers without rebuilding the form.
 *
 * Every committed field re-checks the mixture, and a re-render on each one would
 * take the focus out from under somebody tabbing through the chains -- the same
 * reason the live stats table patches its own cells. Returns false when the answer
 * needs markup that is not on the page, which is main.js's cue to render properly.
 */
export function patch(state) {
  if (state.planNew || !state.planDraft || signature(state) !== rendered) return false;
  if (!document.querySelector("[data-plan-form]")) return false;
  return patchCheck(state.planCheck);
}

/**
 * Put a fresh check on the page without rebuilding the form.
 *
 * The figures change on every committed field, and re-rendering to show them would
 * take the focus out from under whoever is tabbing through the chains. Only the
 * derived text is replaced — and the iterations/s boxes, which are a second view of
 * a percentage rather than a field of their own, skipped while one has the caret in
 * it so a running conversion cannot fight the typing.
 */
export function patchCheck(check) {
  const holder = document.getElementById("plan-verdict");
  if (!holder || !check) return false;
  holder.innerHTML = verdict(check);

  const implied = document.getElementById("plan-implied");
  if (implied) implied.innerHTML = implication(check.figures);

  const totals = document.getElementById("plan-total");
  if (totals) totals.innerHTML = total(check);

  const basicTotal = document.getElementById("plan-basic-total");
  if (basicTotal && check.figures) {
    basicTotal.innerHTML = `<strong>${escape(round(check.figures.rate) || "—")} RPS</strong> total · ${count(
      check.figures.chains.length,
      "call"
    )}`;
  }

  if (!check.figures) return true;
  for (const node of document.querySelectorAll(".metrix-chain-implied")) {
    node.innerHTML = chainImplied(check.figures.chains[Number(node.dataset.chain)]);
  }

  for (const [index, chain] of check.figures.chains.entries()) {
    const inputs = document.querySelectorAll(
      `[name="chains.${index}.rps"], [name="basic.${index}.rps"]`
    );
    for (const input of inputs) {
      if (input !== document.activeElement) {
        input.value = chain.iterations_per_s == null ? "" : round(chain.iterations_per_s);
      }
    }
  }
  return true;
}

/* ------------------------------------------------------------------- controls */

function text(name, label, value, { required, hint } = {}) {
  return `<div class="metrix-field">
    <label class="form-label" for="f-${escape(name)}">
      ${escape(label)}${required ? ' <span class="text-danger">*</span>' : ""}
    </label>
    <input class="form-control" id="f-${escape(name)}" name="${escape(name)}"
           value="${escape(value ?? "")}">
    ${hint ? `<div class="form-hint">${escape(hint)}</div>` : ""}
  </div>`;
}

/**
 * A number box. `derives` marks the two that are one value seen twice: a share and
 * the iterations a second it comes to. Committing either writes the percentage --
 * the document stores a share, so a typed rate has to become one before it can be
 * saved or checked.
 */
function number(name, label, value, { hint, derives, readOnly } = {}) {
  return `<div class="metrix-field">
    <label class="form-label" for="f-${escape(name)}">${escape(label)}</label>
    <input class="form-control" type="number" step="any" min="0"
           id="f-${escape(name)}" name="${escape(name)}"
           ${derives ? `data-derives="${escape(derives)}"` : ""}
           ${readOnly ? "readonly" : ""}
           value="${value == null ? "" : escape(round(value))}">
    ${hint ? `<div class="form-hint">${escape(hint)}</div>` : ""}
  </div>`;
}

function select(name, label, options, value, { hint } = {}) {
  const items = options
    .map(
      (option) =>
        `<option value="${escape(option)}"${option === value ? " selected" : ""}>
           ${escape(option)}
         </option>`
    )
    .join("");
  return `<div class="metrix-field">
    <label class="form-label" for="f-${escape(name)}">${escape(label)}</label>
    <select class="form-select" id="f-${escape(name)}" name="${escape(name)}"
            >${items}</select>
    ${hint ? `<div class="form-hint">${escape(hint)}</div>` : ""}
  </div>`;
}

/* ------------------------------------------------------------ form to document */

/**
 * Read the live form back into a mixture document.
 *
 * **It starts from the document it loaded.** The form draws the load block, the
 * phases and the chains; a plan also carries auth, datasets, generators, capture,
 * SLOs and engine tuning, and rebuilding the document from the inputs alone would
 * delete every one of them on the first save. Only what the form owns is replaced.
 *
 * Empty strings are dropped rather than sent: the server reads a missing key as "use
 * the default" and an empty one as a value, and a blank warmup is the former.
 */
export function readForm(form, previous) {
  const data = new FormData(form);
  const editorMode = form.dataset?.editorMode ?? "advanced";
  const value = (name) => (data.get(name) ?? "").toString().trim();
  const numberAt = (name) => {
    const raw = value(name);
    return raw === "" || Number.isNaN(Number(raw)) ? null : Number(raw);
  };

  const load = { ...(previous.load ?? {}) };
  load.mode = value("load.mode") || "fixed";
  load.duration = value("load.duration");
  assign(load, "warmup", value("load.warmup") || null);
  assign(load, "max_concurrency", numberAt("load.max_concurrency"));

  const phases = { ...(previous.phases ?? {}) };
  assign(phases, "baseline", value("phases.baseline") || null);
  assign(phases, "settle", value("phases.settle") || null);

  let chains;
  if (editorMode === "basic") {
    chains = readBasicChains(previous.chains ?? [], value, numberAt);
    if (chains.length) {
      const rates = chains.map((_, index) => numberAt(`basic.${index}.rps`) ?? 0);
      const safeRates = rates.some((rate) => rate > 0) ? rates : rates.map(() => 1);
      load.rate = applyRates(chains, safeRates);
    }
  } else if (editorMode === "load") {
    chains = previous.chains ?? [];
    if (load.mode === "fixed") load.rate = numberAt("load.rate") ?? load.rate;
  } else {
    chains = (previous.chains ?? []).map((chain, index) => {
    const at = (field) => `chains.${index}.${field}`;
    const next = { ...chain };
    next.name = value(at("name"));
    next.percent = numberAt(at("percent")) ?? 0;
    next.session = value(at("session")) || "reuse";
    if (next.session === "pool") assign(next, "pool_size", numberAt(at("pool_size")));
    else delete next.pool_size;
    next.steps = (chain.steps ?? []).map((step, stepIndex) =>
      readStep(step, `chains.${index}.steps.${stepIndex}`, value, numberAt, data)
    );
    return next;
    });
    const rawRates = chains.map((_, index) => value(`chains.${index}.rps`));
    if (load.mode === "fixed" && rawRates.some((rate) => rate !== "")) {
      load.rate = applyRates(
        chains,
        rawRates.map((rate) => Math.max(Number(rate) || 0, 0))
      );
    }
  }

  // Non-fixed shapes carry their own rates. Leaving a fixed rate beside them would
  // make a number the engine ignores look authoritative.
  if (load.mode !== "fixed") delete load.rate;

  const document = { ...previous, load, chains };
  if (Object.keys(phases).length) document.phases = phases;
  else delete document.phases;
  return document;
}

function readBasicChains(previous, value) {
  const used = new Set();
  return previous.map((chain, index) => {
    const rawName = value(`basic.${index}.name`) || chain.name || `call-${index + 1}`;
    let name = rawName;
    let suffix = 2;
    while (used.has(name)) name = `${rawName}-${suffix++}`;
    used.add(name);
    const call = value(`basic.${index}.call`) || chain.steps?.[0]?.call || "";
    const previousStep = chain.steps?.[0] ?? {};
    return {
      ...chain,
      name,
      session: chain.session ?? "reuse",
      steps: [{ ...previousStep, id: previousStep.id || name, call }],
    };
  });
}

/** Turn row RPS into the one total rate and percentages the engine stores. */
function applyRates(chains, rates) {
  const total = rates.reduce((sum, rate) => sum + rate, 0);
  if (total <= 0) {
    chains.forEach((chain) => {
      chain.percent = 0;
    });
    return 0;
  }
  let assigned = 0;
  chains.forEach((chain, index) => {
    const percent =
      index === chains.length - 1
        ? 100 - assigned
        : Math.round((rates[index] / total) * 1000000) / 10000;
    chain.percent = percent;
    assigned += percent;
  });
  return total;
}

function readStep(step, prefix, value, numberAt, data) {
  const next = { ...step };
  next.id = value(`${prefix}.id`);
  next.call = value(`${prefix}.call`);

  if (!data.get(`${prefix}.repeat`)) {
    delete next.repeat_until;
    return next;
  }
  const selector = value(`${prefix}.repeat.selector`) || "json";
  // One selector at a time: the schema is a oneOf, and leaving the previous kind
  // beside the new one would be a document with two ways to read the response.
  const repeat = { [selector]: value(`${prefix}.repeat.expression`) };
  repeat.equals = value(`${prefix}.repeat.equals`);
  repeat.max_attempts = numberAt(`${prefix}.repeat.max_attempts`) ?? 5;
  repeat.interval_ms = numberAt(`${prefix}.repeat.interval_ms`) ?? 200;
  next.repeat_until = repeat;
  return next;
}

function assign(target, key, value) {
  if (value === null || value === "") delete target[key];
  else target[key] = value;
}

/**
 * A share typed as a rate, turned back into the share the document stores.
 *
 * The only arithmetic on this page that is not display: a document holds
 * percentages, so an iterations/s somebody typed has to become one before it can be
 * submitted. The server then says what that share actually comes to, and the box is
 * rewritten from its answer.
 */
export function shareFromRate(rate, total) {
  if (!total || !Number.isFinite(total) || total <= 0) return null;
  return Math.round((rate / total) * 1e6) / 1e4;
}

/* -------------------------------------------------------------------- numbers */

function round(value) {
  if (value == null || !Number.isFinite(Number(value))) return "";
  const number = Number(value);
  return String(Math.round(number * 100) / 100);
}

function seconds(value) {
  if (value == null) return "—";
  if (value < 60) return `${round(value)}s`;
  const minutes = Math.floor(value / 60);
  const rest = Math.round(value - minutes * 60);
  return rest ? `${minutes}m ${rest}s` : `${minutes}m`;
}

function implied(chain) {
  return chain?.requests_per_s == null ? "—" : `${round(chain.requests_per_s)} req/s`;
}

function requestCount(chain) {
  if (!chain || chain.requests == null) return "—";
  const value = chain.requests.toLocaleString();
  return chain.supported ? value : `${value} (thin)`;
}
