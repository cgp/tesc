# Engine arrival-timing baseline

Recorded 2026-09-14. This is a manually supervised performance reference, not a portable CI throughput requirement. Further optimization is deferred; 10,000 RPS is the working baseline on the setup below, with a small, explicitly measured arrival shortfall. It is not a zero-loss guarantee or a target capacity claim.

## Setup and evidence

The load generator and mock ran on separate Linux machines. The generator was an AMD Ryzen 9 6900HX, x86_64, with 16 online logical CPUs and fewer background workloads than the earlier Ryzen 5 5600G setup. Each run used `examples/plans/mock-fixed`, lasted 15 measured seconds, and enabled both summary and request-event NDJSON output. The reported warmup counts were zero. Runs followed the snapshot-worker fix (`93b311c`, move histogram aggregation and window construction off arrival dispatch).

The exact tested engine/mock revisions, mock host processor, kernel, CPU power settings, worker count, concurrency/connection limits, HTTP protocol, mock latency/configuration, NIC/link properties and network path were not recorded in the supplied evidence. Do not infer these from the current mutable example files. This historical baseline needs a fully captured reference session before strict regression gating.

Repeated stderr results were supplied at each rate. One summary NDJSON was examined at 10k and 12k; its timing distributions describe that individual run, not the pooled repetitions. Raw artifacts were not archived in this repository.

## Observed results

Late shortfall is `skipped_late / offered`; it measures whether the requested arrivals were admitted, separately from whether admitted traffic completed.

| Offered RPS | Runs | Offered per run | Late skips per run | Late-shortfall range | Median shortfall |
|---|---:|---:|---|---:|---:|
| 4,000 | 4 | 60,000 | 6, 58, 6, 10 | 0.010–0.097% | 0.013% |
| 8,000 | 5 | 120,000 | 56, 181, 55, 123, 96 | 0.046–0.151% | 0.080% |
| 10,000 | 5 | 150,000 | 238, 259, 140, 198, 192 | 0.093–0.173% | 0.132% |
| 12,000 | 5 | 180,000 | 1,530, 1,773, 1,113, 1,661, 1,944 | 0.618–1.080% | 0.923% |
| 16,000 | 4 | 240,000 | 13,403, 14,098, 13,851, 12,881 | 5.367–5.874% | 5.678% |

Every admitted request was sent and completed successfully in every listed run. Failures, timeouts, cancellations, concurrency/connection skips, output drops, unfinished writers, arrival telemetry drops and snapshot coalesced ticks were zero. Peak in-flight counts were 115–144 at 10k, 136–172 at 12k and 183–189 at 16k. Completion of admitted traffic does not by itself establish unchanged target latency.

The transition to substantially greater timing shortfall lies between 10k and 12k on this setup. At 16k, thousands of dispatches skip small batches of arrivals in each run. This supports arrival-timing pressure as the observed limit; it does not isolate its CPU, timer, executor or networking cause.

On the earlier Linux setup, the representative pre-worker 4k result had 432 late skips. After the worker change, seven subsequent runs had 11–73 skips (median 54), with a separate first run at 224. The remaining skips in its examined NDJSON occupied three consecutive windows despite snapshot worker flushes exceeding 1 ms in all 60 measured windows. Hardware/background work and mock placement changed before the table above, so cross-setup gains cannot be attributed solely to the processor or the code.

## Timing evidence at 10k and 12k

These distributions answer how much arrival spacing is consumed before and after the native timer wake. Every displayed percentile below is marked stable by shared stats support rules.

| Distribution | 10k: p50 / p99 / p99.9 | Samples | 12k: p50 / p99 / p99.9 | Samples |
|---|---|---:|---|---:|
| Timer wake lateness | 31 / 33 / 57 µs | 149,933 | 34 / 37 / 61 µs | 179,975 |
| Wake-to-dispatch delay | 7 / 25 / 62 µs | 149,933 | 8 / 49 / 90 µs | 179,975 |

The examined 10k run had 192 skips in 49/60 windows, 125 dispatches with skips and 68 native timer-accounted skips. The 12k run had 1,944 skips in all 60 windows, 1,846 dispatches with skips (1,801 missing exactly one arrival), and 25 native timer-accounted skips. Timer-accounted skips are not a complete attribution: timer lateness can consume headroom even when an arrival is subsequently skipped at dispatch.

Arrival spacing is 100 µs at 10k and 83.3 µs at 12k. Dispatch p99 nearly doubles while timer wake p99 rises modestly. Investigate the combined wake/dispatch budget; do not add separate percentiles as if they were paired samples. Snapshot flush durations now measure aggregation-thread work, not arrival-loop occupancy. Maxima alone are insufficient to diagnose this transition.

## Future improvement strategy

1. Reproduce 10k and 12k with the reference procedure below; optionally use 11k to refine the transition. Inspect interval timing populations and shortfalls before changing behavior.
2. Profile generator CPU/executor work under load and correlate interruptions with scheduler/network activity. Examine completion polling, admission/dispatch work, timer wake behavior and notification handoff. Use a separate diagnostic session if profiling changes timing; do not silently mix profiled and unprofiled results.
3. Test one bounded change at a time against the same-machine parent revision. Prioritize measured dispatch costs or timer headroom. Treat CPU affinity, power settings, timer tuning or spin behavior as separate experiments with CPU-cost measurements, not assumptions about the cause.
4. Require improvement in repeated 10k/12k admission and dispatch distributions while retaining successful completions, target latency evidence, phase attribution, cancellation, SLO/breakpoint decisions and honest shortfall accounting. Preserve nonblocking consumers and bounded memory. Run the full `bash scripts/check.sh` for behavioral changes.
5. Recheck 4k/8k for regressions and 16k for whether the transition actually moved. Publish the measured tradeoffs, including generator CPU use, rather than optimizing only the highest observed rate.

## Preventing drift: supervised reference runs

Ordinary CI remains responsible for deterministic correctness and synthetic scheduler/backpressure tests. It should not impose this machine-specific 10k threshold on shared or arbitrary runners. Use a manual performance check for arrival-loop, timer, histogram, transport, output and runtime changes, and before releases; a dedicated controlled runner can automate it later.

For each reference session:

- Capture engine and mock revisions, release build/toolchain, exact bundle and mock config, seed, protocol, limits, worker settings, duration, warmup and output mode. Retain credential-safe effective inputs and plan hash. Current example files are not a frozen fixture.
- Record both machines' CPU models, logical CPUs, RAM, OS/kernel, power/governor/boost and affinity settings, virtualization, temperature/throttling indications and background workloads. Record NIC/link speed, topology, wired/wireless path, RTT/loss and competing traffic. Keep generator and mock separate. A hardware, networking or workload change starts a new reference series.
- Verify mock headroom and latency behavior independently. Allow a consistent stabilization period. Record one initial run separately, then collect at least five measured repetitions per rate; retain every run, including outliers. Keep first-run treatment identical for old and new revisions.
- Compare old and new builds in alternating order on the same hosts and configurations. A historical table is context; a contemporary parent-versus-change comparison controls environmental drift better.
- Give every run its own artifact directory outside any directory cleared by `rm *.ndjson`. Archive stderr, summary/events NDJSON and an environment manifest with revision/rate/run index. Calculate per-run shortfall and median/range across runs. Preserve timing counts, overflow/support, skip batches/window locations, target latency and all loss/cap counters.

For the captured reference setup, use a **provisional investigation trigger** at 10k of median late shortfall above 0.20% over at least five comparable runs. The observed median was 0.132%, and every observed run was below 0.20%; this trigger is a practical initial guardrail, not a statistically established limit. Any nonzero failure, cap skip, output/telemetry loss or snapshot coalescing also requires explanation. Investigate timing-distribution deterioration even if shortfall remains below the trigger.

Confirm a suspected regression with another alternating comparison and inspect environment/target health before blaming code. Establish variation over multiple fully captured sessions before tightening thresholds. Keep these historical results; update a baseline only with archived evidence and an explicit explanation of the code or environment change, never simply to make a failing comparison pass.
