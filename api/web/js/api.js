// Every fetch call. The only place a URL appears, so changing one is one edit.

function json(method, body) {
  return {
    method,
    headers: { "Content-Type": "application/json", Accept: "application/json" },
    body: JSON.stringify(body),
  };
}

async function request(path, options = {}) {
  const response = await fetch(path, {
    headers: { Accept: "application/json" },
    ...options,
  });
  // 204 on delete: there is no body to parse, and asking for one would throw.
  if (response.status === 204) return null;

  if (!response.ok) {
    let detail = `${response.status} ${response.statusText}`;
    try {
      const body = await response.json();
      if (body.detail) detail = body.detail;
    } catch {
      // A non-JSON error body is still an error; the status line will do.
    }
    throw new Error(detail);
  }
  return response.json();
}

/**
 * The bundle as a file, rather than as JSON.
 *
 * Kept apart from `request` because the body is a zip and the plan hash is on a
 * header. Fetched rather than linked: a plan with errors in it is refused with a 422
 * naming them, and an `<a download>` would navigate the page to that message.
 */
async function archive(path) {
  const response = await fetch(path, { headers: { Accept: "application/zip" } });
  if (!response.ok) {
    let detail = `${response.status} ${response.statusText}`;
    try {
      detail = (await response.json()).detail ?? detail;
    } catch {
      // A zip endpoint that failed without JSON still has its status line.
    }
    throw new Error(detail);
  }
  return { blob: await response.blob(), hash: response.headers.get("X-Metrix-Plan-Hash") };
}

export const api = {
  health: () => request("/api/health"),
  schemas: () => request("/api/schemas"),
  schema: (id) => request(`/api/schemas/${encodeURIComponent(id)}`),
  uploadSchema: (body) => request("/api/schemas", json("POST", body)),
  deleteSchema: (id) =>
    request(`/api/schemas/${encodeURIComponent(id)}`, { method: "DELETE" }),
  profiles: () => request("/api/profiles"),
  profile: (name) => request(`/api/profiles/${encodeURIComponent(name)}`),
  profileDocument: (name) =>
    request(`/api/profiles/${encodeURIComponent(name)}/document`),
  createProfile: (document) => request("/api/profiles", json("POST", document)),
  replaceProfile: (name, document) =>
    request(`/api/profiles/${encodeURIComponent(name)}`, json("PUT", document)),
  resolveProfile: (name) =>
    request(`/api/profiles/${encodeURIComponent(name)}/resolve`, { method: "POST" }),
  verifyProfile: (name) =>
    request(`/api/profiles/${encodeURIComponent(name)}/verify`, { method: "POST" }),
  resolveDiscovery: (body) => request("/api/discovery/resolve", json("POST", body)),
  verifyCollector: (endpoint) =>
    request("/api/profiles/verify-collector", json("POST", endpoint)),
  verifyLoad: (endpoint) =>
    request("/api/profiles/verify-load", json("POST", endpoint)),
  profileTargets: (name) =>
    request(`/api/profiles/${encodeURIComponent(name)}/targets`),
  deleteProfile: (name) =>
    request(`/api/profiles/${encodeURIComponent(name)}`, { method: "DELETE" }),
  plans: () => request("/api/plans"),
  createPlan: (body) => request("/api/plans", json("POST", body)),
  plan: (name) => request(`/api/plans/${encodeURIComponent(name)}`),
  planDocument: (name) => request(`/api/plans/${encodeURIComponent(name)}/document`),
  // The whole mixture, not a patch, and not saved: the server answers with what it
  // would say about the same document on disk.
  validatePlan: (name, mix) =>
    request(`/api/plans/${encodeURIComponent(name)}/validate`, json("POST", mix)),
  validateBasicPlan: (name, body) =>
    request(`/api/plans/${encodeURIComponent(name)}/validate-basic`, json("POST", body)),
  replacePlan: (name, mix) =>
    request(`/api/plans/${encodeURIComponent(name)}`, json("PUT", mix)),
  replaceBasicPlan: (name, body) =>
    request(`/api/plans/${encodeURIComponent(name)}/basic`, json("PUT", body)),
  // The document itself, posted. There is no URL form on purpose: a control plane
  // that fetches whatever address it is handed is a request forwarder inside the
  // network it is meant to be observing.
  generatePlan: (body) => request("/api/plans/generate", json("POST", body)),
  regenerateCalls: (name, body) =>
    request(`/api/plans/${encodeURIComponent(name)}/regenerate`, json("POST", body)),
  acceptDraft: (name) =>
    request(`/api/plans/${encodeURIComponent(name)}/draft`, { method: "DELETE" }),
  bundle: (name, profile) =>
    request(
      `/api/plans/${encodeURIComponent(name)}/bundle?${new URLSearchParams({
        profile,
        format: "json",
      })}`
    ),
  bundleArchive: (name, profile) =>
    archive(
      `/api/plans/${encodeURIComponent(name)}/bundle?${new URLSearchParams({ profile })}`
    ),
  recordings: (params = {}) => {
    const query = new URLSearchParams(
      Object.entries(params).filter(([, v]) => v !== undefined && v !== null && v !== "")
    );
    const suffix = query.toString() ? `?${query}` : "";
    return request(`/api/recordings${suffix}`);
  },
  recording: (id) => request(`/api/recordings/${encodeURIComponent(id)}`),
  seriesList: () => request("/api/series"),
  // The key is a query parameter, not a path segment: it is a readable tuple with
  // pipes in it, and burying that in a path hides the very thing it is readable for.
  seriesTrend: (key) =>
    request(`/api/series/trend?${new URLSearchParams({ key })}`),
  compare: (ids, phase) => {
    const query = new URLSearchParams(ids.map((id) => ["run", id]));
    if (phase) query.set("phase", phase);
    return request(`/api/compare?${query}`);
  },
  startRecording: (body) => request("/api/recordings", json("POST", body)),
  stopRecording: (id) =>
    request(`/api/recordings/${encodeURIComponent(id)}/stop`, { method: "POST" }),
  live: () => request("/api/recordings/live"),
  summary: (id) => request(`/api/recordings/${encodeURIComponent(id)}/summary`),
  comparison: (id) => request(`/api/recordings/${encodeURIComponent(id)}/comparison`),
  recovery: (id) => request(`/api/recordings/${encodeURIComponent(id)}/recovery`),
  sweep: (id, phase) => {
    const query = phase ? `?${new URLSearchParams({ phase })}` : "";
    return request(`/api/recordings/${encodeURIComponent(id)}/sweep${query}`);
  },
  markBaseline: (id, override = false) =>
    request(
      `/api/recordings/${encodeURIComponent(id)}/baseline${override ? "?override=true" : ""}`,
      { method: "POST" }
    ),
  clearBaseline: (id) =>
    request(`/api/recordings/${encodeURIComponent(id)}/baseline`, { method: "DELETE" }),
  purgeable: (id) => request(`/api/recordings/${encodeURIComponent(id)}/purgeable`),
  purge: (id) =>
    request(`/api/recordings/${encodeURIComponent(id)}/purge`, { method: "POST" }),
  deleteSelected: (ids) => request("/api/recordings/delete", json("POST", { recording_ids: ids })),
  purgeSeries: (key) =>
    request(`/api/series/purge?${new URLSearchParams({ key })}`, { method: "POST" }),
  series: (id, metric, target) => {
    const query = new URLSearchParams({
      ...(metric ? { metric } : {}),
      ...(target ? { target } : {}),
    });
    const suffix = query.toString() ? `?${query}` : "";
    return request(`/api/recordings/${encodeURIComponent(id)}/series${suffix}`);
  },
};
