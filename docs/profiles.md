# Writing a profile

A profile is a named list of the machines in one environment. Plans reference it by
name, so the same mixture runs against staging, against production, or against one
suspect container with no edit to the plan.

Profiles live in `$METRIX_HOME/profiles/<name>.json`. **Profiles** in the menu lists
every one it found and can create, edit and delete them; **Config** shows the
resolved `$METRIX_HOME` they are read from.

The editor and the files are the same thing. It submits a whole document to the
same validator a hand-written file goes through, and writes the file back in the
form shown below — so a profile can be started in the editor and finished in an
editor of your own, or the reverse. The one thing it will not do is rename: a
profile's name is part of a recording's series identity, so renaming one would
split its history in two. Copy and delete, deliberately, if that is what you want.

A profile that will not parse is listed with its error rather than hidden, but it
cannot be opened in the editor — there is nothing valid to load. Fix the file.

The page reads the directory when you open it and when you press **Reload**, and
at no other time — a list that rearranged itself under you while you were reading
it would be worse than a button. Press Reload after editing a file by hand, or
after pulling someone else's change; the timestamp beside the button says when
the files were last read.

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

Two things are recorded that are *not* series, because they are not rates:

- **Identity** — hostname, OS, kernel, architecture, core count. Read once at the
  start. It is what answers "what was this measured on?" a year later.
- **Filesystem usage** — read once before the run and once after it drains, per
  mount. The question a run needs answered is "did this consume disk, and how
  much", and two readings answer it for the cost of two `df` calls rather than one
  per second on every mount. Both appear on the recording, with the change between
  them. A mount with no reading at the end shows no change rather than a change of
  zero — a probe failing is not the same as nothing being written.

Both work over either transport. Over SSH they are a short POSIX script; over
scrape they come from `node_uname_info`, `node_os_info` and `node_filesystem_*`.

Whatever the transport reports is **whole-machine**, not per-process. Over SSH the
remote side is a shell loop reading `/proc/stat`, `/proc/loadavg`, `/proc/meminfo`,
`/proc/diskstats`, `/proc/net/dev`, `/proc/net/snmp`, `/proc/sys/fs/file-nr` and
`/proc/net/sockstat`. `cpu.user` is the box's CPU, not your service's.

Three task counts are collected, and they answer different questions: `proc.count`
is how many processes exist, `thread.count` is processes *and* threads, and
`proc.running` is how many are runnable right now. On an idle box with 200 threads
the three read roughly 190, 431 and 1. Over scrape the first two need node_exporter's
`processes` collector, which ships disabled; without it they are absent rather than
filled in from something else.

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
