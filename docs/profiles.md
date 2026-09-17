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
editor of your own, or the reverse. Renaming moves the file. Existing recordings
keep the old profile name, while future recordings begin a new series under the new
name.

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

For an explicitly entered ALB, the profile editor can resolve the ALB hostname
through AWS and list the concrete hosts behind it. Those IP addresses are read-only:
choose one host for SSH collection, enable it, and save. The selected address is
stored as the endpoint's single `collect.host`; resolving does not convert the
profile into a discovered profile or save the other candidates. **Test** runs one
real SSH probe against a candidate and keeps the result only while the editor is
open.

Defaults: `ssh` port 22, `scrape` port 9100 and path `/metrics`. The Config page
shows the resolved destination for every endpoint, so you can check it without
starting a recording.

The profile's Observation section also accepts `ssh_user`. When set, it is used for
every SSH endpoint when an observation starts; an endpoint-specific `collect.user`
is used only when the profile-wide value is blank. This keeps the login choice in
one place for an environment without rewriting every endpoint.

## Worked examples

### One box running everything

```json
{
  "name": "local",
  "observe": { "interval": "1s", "collect": ["cpu", "memory", "disk", "net"] },
  "endpoints": [
    {
      "id": "localhost",
      "addressing": "ip",
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
  "observe": { "interval": "1s", "collect": ["cpu", "memory", "net"] },
  "endpoints": [
    {
      "id": "nginx",
      "addressing": "ip",
      "address": "10.0.1.10:443",
      "host_header": "staging.example.com",
      "tls": { "enabled": true },
      "collect": { "transport": "ssh", "user": "ec2-user" }
    },
    {
      "id": "app-1",
      "addressing": "ip",
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
  "addressing": "alb",
  "address": "10.0.1.9:443",
  "host_header": "staging.example.com",
  "collect": { "transport": "none" }
}
```

It takes load and contributes no host series. The Config page lists it as *not
collected* rather than hiding it, so the absence is visible rather than assumed.

## Letting discovery write the list

Instead of endpoints, a profile may say where to find them:

```json
{
  "name": "staging-discovered",
  "addressing": "load_balancer",
  "discover": {
    "hostname": "api.staging.example.com",
    "ttl": "10m",
    "collect": { "transport": "scrape", "port": 9100 }
  }
}
```

Metrix resolves the hostname the way AWS actually lays it out — Route 53 to the load
balancer, its listeners and host-header rules to a target group, the ECS service
registered against that group, its tasks and their image digests, and the EC2
instances and autoscaling group underneath. Name `cluster` and `service` instead of
`hostname` to skip DNS and the balancer entirely; with `direct` addressing that form
also needs a `host_header`, since there is no hostname to take one from.

`collect` is applied to every host found, because discovery yields machines that are
alike by construction. The balancer itself is listed as an endpoint with no collector
— you cannot log into an ALB — and under `direct` addressing it is left out, since
nothing would be sent there.

**Every hop is optional.** A hostname that is a network balancer with nothing in ECS
behind it resolves to instances and stops; a task in bridge networking has no address
of its own and is observed through the box hosting it. The Profiles page says how far
the walk got and what it could not determine, because a partial answer is the normal
one and is usually the answer you wanted.

**The file keeps the question; the store keeps the answer.** Endpoints are never
written back into the profile. A resolution is stored with a timestamp, reused until
`ttl` expires, walked again when you press **Resolve**, and walked again when a
recording starts — and the snapshot that recording used is pinned to it, so months
later it still says exactly which build was measured. If the set of machines changes
while a recording is open, that is a `host_count_changed` note on the recording
rather than a silent change of subject; collection stays with the machines it started
with.

Discovery is read-only and needs the permissions in `policy/metrix-readonly.json`.
Which AWS profile and region to use is `[aws]` in `config.toml`.

## Addressing

Each endpoint has an `addressing` value: `ip`, `alb`, `elb`, `ecs`, or `fargate`.
It classifies the network path represented by the concrete `address`; the engine
still receives only `host:port`. ALB and ELB endpoints produce the
`load_balancer` series class; IP, ECS, and Fargate produce `direct`, so a run through
a balancer is not compared with one sent straight to a task.

Discovery supports ALB/NLB through ELBv2, ECS, and Fargate tasks. Classic ELB is
explicit-only. Legacy files may still put `"addressing": "load_balancer"` or
`"addressing": "direct"` at profile level; the loader maps that value onto endpoints
and writes the endpoint form the next time the profile is saved.

Addressing a container directly usually needs `host_header`: most services vhost on
it, and a raw IP gets a 404 or a default backend.

## Which boxes take traffic

`"load": false` on an endpoint means *watch this box, do not send to it*. Every
endpoint still has an address — that is where the machine is — and this says whether
it is also where the load goes:

```json
{
  "id": "app-1",
  "address": "10.0.3.41:8080",
  "load": false,
  "collect": { "transport": "ssh", "user": "ec2-user" }
}
```

The two roles are usually different sets. Pointing at a balancer and watching the
boxes behind it is the common shape: the balancer is a target nothing can be
collected from, and each box behind it is observed without being addressed. The
Profiles page counts both — *1 sent to · 3 of 4 observed* — and shows a watched-only
address muted, so a column headed **Load target** never claims something it should
not.

Discovery sets this for you from its legacy profile-level path choice: with
`load_balancer` the balancer takes the traffic, with `direct` the boxes do. Resolved
endpoints are labelled with the concrete kind that discovery found.

## Checking a profile before trusting it

**Verify** on a profile card asks two questions of every endpoint at once, and they
are answered separately because they fail for different reasons:

- **Load target** — can a connection be opened, and does the TLS handshake complete?
  Nothing is sent. This asks whether the socket accepts, not what is listening on it.
- **Collector** — one real probe over the transport a recording would use. Not a port
  check: an SSH login that works and then cannot run the stats script, or an exporter
  answering 404 on the configured path, are exactly the failures a port check passes
  and a recording then hits. A collector that answers names the box it reached.

An endpoint with no collector, or one that takes no traffic, is drawn as neither
reachable nor unreachable — it was never going to be checked, and that is not a
fault.

The result is a snapshot of a moment and is not stored. It disappears when you leave
the page, which is correct: reachability yesterday says nothing about reachability
now.

If a recording runs anyway and a box never answers *once*, the recording carries a
`target_unreachable` note marked **invalid** — it cannot become a baseline without
someone saying so out loud, because a mean over "the environment" that silently
leaves out one machine is worse than no mean.
