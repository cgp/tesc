// Every fetch call. The only place a URL appears, so changing one is one edit.

async function request(path, options = {}) {
  const response = await fetch(path, {
    headers: { Accept: "application/json" },
    ...options,
  });
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

export const api = {
  health: () => request("/api/health"),
  profiles: () => request("/api/profiles"),
  profile: (name) => request(`/api/profiles/${encodeURIComponent(name)}`),
  recordings: (params = {}) => {
    const query = new URLSearchParams(
      Object.entries(params).filter(([, v]) => v !== undefined && v !== null && v !== "")
    );
    const suffix = query.toString() ? `?${query}` : "";
    return request(`/api/recordings${suffix}`);
  },
  recording: (id) => request(`/api/recordings/${encodeURIComponent(id)}`),
  startRecording: (body) =>
    request("/api/recordings", {
      method: "POST",
      headers: { "Content-Type": "application/json", Accept: "application/json" },
      body: JSON.stringify(body),
    }),
  stopRecording: (id) =>
    request(`/api/recordings/${encodeURIComponent(id)}/stop`, { method: "POST" }),
  live: () => request("/api/recordings/live"),
  series: (id, metric, target) =>
    request(
      `/api/recordings/${encodeURIComponent(id)}/series?` +
        new URLSearchParams({ metric, ...(target ? { target } : {}) })
    ),
};
