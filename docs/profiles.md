# Writing a profile

A profile is a named list of the machines in one environment. Plans reference it by
name, so the same mixture runs against staging, against production, or against one
suspect container with no edit to the plan.

Profiles live in `$METRIX_HOME/profiles/<name>.json`. The Config page shows the
resolved `$METRIX_HOME` and every profile it found, including any that will not
parse.

## The one thing to get right

Each endpoint carries **two addresses, for two different purposes**:

| Field | What it is |
|---|---|
| `address` | The socket **requests are sent to**. The load target. |
| `collect` | A **separate connection** that host statistics are read from. |

They are different ports, usually different software, and neither is derived from
the other.

**Nothing is inferred from the load target.** The tool does not connect to
`address` to find out what is listening, does not identify the process behind it,
and does not attach to that process. If `collect` is absent, that endpoint is
simply not observed — it still receives load.

## What gets collected

Whatever the transport reports is **whole-machine**, not per-process. Over SSH the
remote side is a shell loop reading `/proc/stat`, `/proc/loadavg`, `/proc/meminfo`,
`/proc/diskstats`, `/proc/net/dev`, `/proc/net/snmp`, `/proc/sys/fs/file-nr` and
`/proc/net/sockstat`. `cpu.user` is the box's CPU, not your service's.

So if nginx reverse-proxies to your app on the same machine, one set of numbers
covers both, and there is no way to split them. That is usually what you want for a
load test — the box is the thing that saturates — but it does mean a busy proxy and
a busy backend are indistinguishable in the CPU series. To tell them apart, put
them on separate machines and give each its own endpoint.

## Transports

| `transport` | Reads from | Needs |
|---|---|---|
| `ssh` | `/proc` over one long-lived connection | SSH access; nothing installed on the target |
| `scrape` | A Prometheus exposition over HTTP | An exporter already running, e.g. node_exporter |
| `none` | Nothing | — |

`collect.host` defaults to the host part of `address`, which is why most endpoints
only set `transport` and a port. Set `collect.host` when statistics come from
somewhere else — a bastion, or a sidecar on a different IP.

Defaults: `ssh` port 22, `scrape` port 9100 and path `/metrics`. The Config page
shows the resolved destination for every endpoint, so you can check it without
starting a recording.

## Worked examples

### One box running everything

```json
{
  "name": "local",
  "addressing": "load_balancer",
  "observe": { "interval": "1s", "collect": ["cpu", "memory", "disk", "net"] },
  "endpoints": [
    {
      "id": "localhost",
      "address": "127.0.0.1:8080",
      "collect": { "transport": "scrape", "port": 9100, "path": "/metrics" }
    }
  ]
}
```

Load goes to port 8080. Statistics come from node_exporter on port 9100 of the same
host. Port 8080 is never inspected for anything but responses.

### nginx in front of an app, on separate boxes

```json
{
  "name": "staging",
  "addressing": "direct",
  "observe": { "interval": "1s", "collect": ["cpu", "memory", "net"] },
  "endpoints": [
    {
      "id": "nginx",
      "address": "10.0.1.10:443",
      "host_header": "staging.example.com",
      "tls": { "enabled": true },
      "collect": { "transport": "ssh", "user": "ec2-user" }
    },
    {
      "id": "app-1",
      "address": "10.0.3.41:8080",
      "host_header": "staging.example.com",
      "collect": { "transport": "ssh", "user": "ec2-user" }
    }
  ]
}
```

Two machines, two endpoints, two series. Load can be aimed at either, and both are
observed for the whole run regardless of which one is receiving it — a proxy that
saturates while the backend idles is exactly the shape this is meant to show.

### A load balancer that cannot be logged into

```json
{
  "id": "alb",
  "address": "10.0.1.9:443",
  "host_header": "staging.example.com",
  "collect": { "transport": "none" }
}
```

It takes load and contributes no host series. The Config page lists it as *not
collected* rather than hiding it, so the absence is visible rather than assumed.

## Addressing

`addressing` is `load_balancer` or `direct`, and it is part of a recording's series
identity — the two measure different network paths and are never compared against
each other. Set it to `direct` when the addresses are individual containers or
instances rather than a balancer in front of them.

Addressing a container directly usually needs `host_header`: most services vhost on
it, and a raw IP gets a 404 or a default backend.
