//! Phase execution with separate accumulators for warmup-admitted attempts.
//!
//! A slot holds one chain iteration, not one request. That is what makes `rate` mean
//! iterations started per second (design-engine §5): a three-step chain at 25/s is
//! 75 requests a second, and a scheduler that admitted requests rather than
//! iterations would be running the mixture three times too fast.
use crate::chain;
use crate::{Lag, Output, Plan, Recording, Report, Slot, cause, flush, record_send};
use crate::{
    http::{Pool, SendState},
    timeline::Timeline,
};
use metrix_metrics::{
    aggregation::{Accumulator, StepSample, Window},
    events::Phase,
};
use std::{
    future::{Future, poll_fn},
    sync::Arc,
    task::Poll,
    time::Duration,
};
use tokio::{sync::mpsc, time::Instant};
use tokio_util::sync::ReusableBoxFuture;

pub(crate) async fn run(
    plan: &Plan,
    shutdown: impl Future<Output = ()>,
    snapshots: Option<mpsc::Sender<Window>>,
    output: Option<&Output>,
) -> Result<Report, String> {
    tokio::pin!(shutdown);
    let mut report = Report::default();
    // The plan's own setup before the target's, and both before the arrival clock. A
    // sidecar that forked its first process on the first arrival would charge that
    // fork to the first request; one that cannot start at all is the plan's problem,
    // and an unreachable target must not be the thing reported instead.
    //
    // Boxed because this function's future is the whole scheduler and is polled on
    // one thread's stack: setup that happens once must not be carried inside the
    // state machine that runs for the length of the run.
    tokio::select! {
        biased;
        _ = &mut shutdown => { report.interrupted = true; return Ok(report); }
        started = Box::pin(plan.generators.start()) => started?,
    }
    // Tokens before the clock too, and pre-warmed to the size `identity` implies: a
    // token first fetched inside the measured window is a token fetch inside the
    // measured window (§6.1).
    if let Some(auth) = &plan.auth {
        tokio::select! {
            biased;
            _ = &mut shutdown => { report.interrupted = true; return Ok(report); }
            warmed = Box::pin(auth.warm()) => warmed?,
        }
    }
    let pool = tokio::select! {
        biased;
        _ = &mut shutdown => { report.interrupted = true; return Ok(report); }
        pool = Pool::prepare(&plan.target, plan.connections.min(plan.concurrency), plan.request_timeout()) => match pool {
            Ok(pool) => pool,
            Err(crate::Failure::LocalResource) => {
                report.generator_limited = true;
                report.stopped_because = Some("generator_limited".into());
                if let Some(output) = output { output.note("generator_limited", metrix_metrics::Severity::Invalid, serde_json::json!({"cause":"local_socket_exhaustion", "during":"setup"})); }
                return Ok(report);
            }
            Err(error) => return Err(format!("target setup failed: {error}")),
        },
    };
    let mut slots = Vec::new();
    slots
        .try_reserve_exact(plan.concurrency)
        .map_err(|_| "cannot allocate request slots")?;
    for _ in 0..plan.concurrency {
        slots.push(Slot {
            active: false,
            worker: 0,
            iteration: 0,
            admitted: Instant::now(),
            phase: Phase::Measure,
            send_state: Arc::new(SendState::for_run(Arc::clone(&plan.samples))),
            send_recorded: false,
            chain: plan.chains[0].name,
            future: ReusableBoxFuture::new(chain::run(None)),
        });
    }
    // Seeded with the plan's own chains and steps, so every window reports all of
    // them: a chain that sent nothing during a window did nothing, which is a
    // measurement rather than an absence.
    let mut workers: Vec<_> = (0..plan.worker_threads.min(plan.concurrency))
        .map(|_| Accumulator::default())
        .collect();
    let mut warmup_workers: Vec<_> = (0..workers.len()).map(|_| Accumulator::default()).collect();
    let mut interval_metrics = Accumulator::default();
    let mut warmup_interval = Accumulator::default();
    // Declared in place rather than through a constructor taking one by value: an
    // accumulator carries a thousand status counters, and moving one through a
    // function is a kilobyte of stack copy per worker on a thread that has little.
    for accumulator in workers
        .iter_mut()
        .chain(warmup_workers.iter_mut())
        .chain([&mut interval_metrics, &mut warmup_interval])
    {
        for compiled in &plan.chains {
            let steps: Vec<&'static str> = compiled.steps.iter().map(|step| step.id).collect();
            accumulator.declare(compiled.name, &steps);
            for step in &compiled.steps {
                if let Some(attached) = &step.request.generate {
                    accumulator.declare_generator(attached.name);
                }
            }
        }
    }
    // Which chain each arrival runs. Deterministic and exactly proportional: a
    // percentage is a claim about what the service was asked for, and a run that got
    // 19.3% of one chain because of sampling noise measured a mixture nobody wrote.
    let mut mixture = chain::Mixture::new(plan.weights.clone());
    let mut lag = Lag::default();
    let start = Instant::now();
    report.diagnostics = crate::Diagnostics::new(
        plan.detector_config,
        plan.concurrency,
        plan.baseline,
        plan.warmup,
        plan.duration,
    );
    let measure_from_ms = output.map(|o| {
        o.elapsed().saturating_add(
            (plan.baseline + plan.warmup)
                .as_millis()
                .min(u128::from(u64::MAX)) as u64,
        )
    });
    report.measured_from_ms = measure_from_ms;
    let mut timeline = Timeline::new(start, plan)?;
    if let Some(output) = output {
        output.phase(timeline.phase);
    }
    let mut last_snapshot = Duration::ZERO;
    let mut snapshot_tick = tokio::time::interval_at(
        start + Duration::from_millis(250),
        Duration::from_millis(250),
    );
    snapshot_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut active = 0;
    let mut pool = Some(pool);
    let mut initial_connection_counted = false;
    loop {
        report.diagnostics.observe(start.elapsed(), active);
        let phase = timeline.phase;
        if !initial_connection_counted && matches!(phase, Phase::Warmup | Phase::Measure) {
            if phase == Phase::Warmup {
                warmup_workers[0].counters.connections_opened = 1;
            } else {
                workers[0].counters.connections_opened = 1;
            }
            initial_connection_counted = true;
        }
        if timeline.ready(Instant::now(), active) {
            if let Some(schedule) = &mut timeline.schedule {
                let (arrival, late) = schedule.due(Instant::now());
                debug_assert!(arrival.is_none());
                report.offered += late;
                let h = report.diagnostics.phase_mut(phase);
                h.offered += late;
                h.skipped_late += late;
                report.skipped_late += late;
            }
            report.diagnostics.observe(start.elapsed(), active);
            flush(
                Recording {
                    slots: &mut slots,
                    workers: &mut workers,
                    warmup_workers: &mut warmup_workers,
                    warmup_interval: &mut warmup_interval,
                    lag: &mut lag,
                    phase,
                    auth: plan.auth.as_ref().map(|auth| auth.snapshot()),
                },
                &mut interval_metrics,
                &mut report,
                &mut last_snapshot,
                start.elapsed(),
                active,
                snapshots.as_ref(),
            );
            if let Some(output) = output {
                output.summary(
                    report.last_window.as_ref().expect("flushed window"),
                    phase,
                    &report.diagnostics,
                    timeline.ready(Instant::now(), active) || report.interrupted,
                    report.interrupted,
                );
            }
            if !timeline.advance(Instant::now())? {
                break;
            }
            if timeline.phase == Phase::Settle {
                drop(pool.take());
            }
            if let Some(output) = output {
                output.phase(timeline.phase);
            }
            continue;
        }
        let deadline = timeline.deadline();
        tokio::select! {
            biased;
            _ = &mut shutdown => {
                report.interrupted = true;
                report.cancelled = active as u64;
                for slot in &slots { if slot.active {
                    if slot.phase == Phase::Warmup { warmup_workers[slot.worker].cancel_chain(slot.chain); } else { workers[slot.worker].cancel_chain(slot.chain); }
                    if let Some(output) = output { output.cancel(slot.iteration, slot.phase, slot.chain, slot.admitted.elapsed()); }
                } }
                report.diagnostics.observe(start.elapsed(), active);
                flush(Recording { slots: &mut slots, workers: &mut workers, warmup_workers: &mut warmup_workers, warmup_interval: &mut warmup_interval, lag: &mut lag, phase, auth: plan.auth.as_ref().map(|auth| auth.snapshot()) },
                    &mut interval_metrics, &mut report, &mut last_snapshot, start.elapsed(), 0, snapshots.as_ref());
                if let Some(output) = output { output.summary(report.last_window.as_ref().expect("flushed window"), phase, &report.diagnostics, timeline.ready(Instant::now(), active) || report.interrupted, report.interrupted); }
                break;
            }
            _ = async { if let Some(deadline) = deadline { tokio::time::sleep_until(deadline).await; } else { std::future::pending::<()>().await; } } => {}
            deadline = snapshot_tick.tick() => {
                let late = Instant::now().saturating_duration_since(deadline);
                lag.max = lag.max.max(late);
                lag.samples += 1;
                report.max_scheduler_lag = report.max_scheduler_lag.max(late);
                report.scheduler_lag_samples += 1;
                report.diagnostics.observe(start.elapsed(), active);
                flush(Recording { slots: &mut slots, workers: &mut workers, warmup_workers: &mut warmup_workers, warmup_interval: &mut warmup_interval, lag: &mut lag, phase, auth: plan.auth.as_ref().map(|auth| auth.snapshot()) },
                    &mut interval_metrics, &mut report, &mut last_snapshot, start.elapsed(), active, snapshots.as_ref());
                if let Some(output) = output { output.summary(report.last_window.as_ref().expect("flushed window"), phase, &report.diagnostics, timeline.ready(Instant::now(), active) || report.interrupted, report.interrupted); }
                if phase == Phase::Measure && report.stopped_because.is_none() {
                    if let Some(b) = &plan.breakpoint {
                        if let Some(reason) = crate::breakpoint::assess(&report, if plan.refinement { metrix_plan::mix::StopOn::default() } else { b.stop_on }, plan.breakpoint_baseline_p99) {
                            report.generator_limited = reason == "generator_limited";
                            report.stopped_because = Some(reason.clone());
                            report.diagnostics.measure.end = start.elapsed();
                            timeline.stop(Instant::now(), plan.final_settle);
                            if let Some(output) = output { output.note(&reason, if report.generator_limited { metrix_metrics::Severity::Invalid } else { metrix_metrics::Severity::Warn }, serde_json::json!({"rate":plan.rate})); }
                        }
                    }
                }
            }
            (index, completion) = poll_fn(|cx| {
                for (index, slot) in slots.iter_mut().enumerate() {
                    if slot.active {
                        if let Poll::Ready(completion) = slot.future.get_pin().poll(cx) { return Poll::Ready((index, completion)); }
                    }
                }
                Poll::Pending
            }), if active > 0 => {
                record_send(&mut slots[index], &mut workers, &mut warmup_workers, &mut report);
                slots[index].active = false;
                active -= 1;
                let admitted_phase = slots[index].phase;
                let iteration = slots[index].iteration;
                let worker = if admitted_phase == Phase::Warmup { &mut warmup_workers[slots[index].worker] } else { &mut workers[slots[index].worker] };
                let chain_name = completion.chain.name;
                for outcome in &completion.steps {
                    let observation = &outcome.observation;
                    let step = &completion.chain.steps[outcome.index];
                    let step_id = step.id;
                    if let Some(output) = output { output.request(iteration, admitted_phase, chain_name, step_id, step.call, observation); }
                    worker.finish_step(&StepSample {
                        chain: chain_name,
                        step: step_id,
                        request_duration: observation.request_duration,
                        send_delay: observation.drift,
                        ttfb: observation.ttfb,
                        drift: None,
                        status: observation.status,
                        // An answer that failed an assertion is a failure of this
                        // step, under its own class: the request happened and what
                        // came back was not what the plan expects.
                        error: observation
                            .error
                            .map(cause)
                            .or(outcome.verdict.map(chain::Verdict::cause)),
                        assertion: outcome.verdict.and_then(chain::Verdict::assertion),
                        bytes_sent: if observation.sent.is_some() { observation.bytes_sent } else { 0 },
                        bytes_received: observation.bytes_received,
                        connections_opened: observation.connections_opened,
                        connection_reused: observation.connection_reused,
                    });
                    // Kept in full while this class still has room (§9.3). Claimed
                    // here rather than in the chain, because a sample is a record on
                    // the output stream and the chain does not have one.
                    if let (Some(output), Some(class)) = (output, crate::samples::classify(observation, outcome.verdict)) {
                        if let Some(ordinal) = plan.samples.claim(class) {
                            output.error_sample(metrix_metrics::events::ErrorSample {
                                t_ms: output.elapsed(), target_id: plan.target.id.clone(),
                                phase: admitted_phase, chain: chain_name.to_owned(),
                                step: step_id.to_owned(), call: step.call.to_owned(),
                                iteration, class, ordinal,
                                assertion: outcome.verdict.and_then(chain::Verdict::assertion),
                                detail: None,
                                request: outcome.sent.as_ref().map_or_else(
                                    || plan.samples.request(step.request.method.as_str(), "", &Default::default(), &[]),
                                    |sent| plan.samples.request(
                                        step.request.method.as_str(),
                                        sent.uri.path_and_query().map_or("/", |part| part.as_str()),
                                        &sent.headers,
                                        &sent.body,
                                    ),
                                ),
                                response: observation.response.as_ref().map(|captured| {
                                    plan.samples.response(observation.status.unwrap_or(0), &captured.headers, &captured.body)
                                }),
                            });
                        }
                    }
                    if observation.sent.is_some() { report.sent_finished += 1; }
                    if let Some(error) = observation.error {
                        report.failed += 1;
                        report.timed_out += u64::from(error == crate::Failure::Timeout);
                    } else if outcome.verdict.is_some() {
                        report.failed += 1;
                    } else { report.responses += 1; }
                }
                // Generation is the run's own cost, not the service's: recorded
                // whether or not it produced a request, and never as a target error.
                for call in &completion.generated {
                    worker.finish_generation(call.generator, call.took, call.failed);
                }
                if let Some((step_id, cause, detail)) = completion.not_sent() {
                    // Nothing was sent. The step still attempted and still failed,
                    // and saying which variable it wanted, or what the generator
                    // said, is the difference between a plan error and a service
                    // that started returning 404s.
                    worker.finish_step(&StepSample {
                        chain: chain_name, step: step_id,
                        request_duration: None, send_delay: Duration::ZERO,
                        ttfb: None, drift: None, status: None,
                        error: Some(cause),
                        assertion: None,
                        bytes_sent: 0, bytes_received: 0,
                        connections_opened: 0, connection_reused: false,
                    });
                    report.failed += 1;
                    if let Some(output) = output {
                        output.not_sent(iteration, admitted_phase, crate::output::NotSent { chain: chain_name, step: step_id, cause, detail, truncated: completion.was_truncated() });
                        // Nothing reached the wire, so there is no request or response
                        // to keep -- the detail is the whole sample, and it is what
                        // says which variable or which script.
                        let class = if cause == metrix_metrics::aggregation::Cause::Generation {
                            metrix_metrics::events::ErrorClass::Generation
                        } else if cause == metrix_metrics::aggregation::Cause::Unauthorized {
                            metrix_metrics::events::ErrorClass::Unauthorized
                        } else {
                            metrix_metrics::events::ErrorClass::Extraction
                        };
                        if let Some(ordinal) = plan.samples.claim(class) {
                            output.error_sample(metrix_metrics::events::ErrorSample {
                                t_ms: output.elapsed(), target_id: plan.target.id.clone(),
                                phase: admitted_phase, chain: chain_name.to_owned(),
                                step: step_id.to_owned(), call: String::new(),
                                iteration, class, ordinal, assertion: None,
                                detail: Some(detail.to_owned()),
                                request: plan.samples.request("", "", &Default::default(), &[]),
                                response: None,
                            });
                        }
                    }
                }
                // The iteration's own duration, recorded whether or not it reached
                // its last step: a chain that stopped early still took the time it
                // took, and keeping only the ones that worked would make the
                // chain's median a median of the successes.
                worker.finish_chain(chain_name, completion.duration, completion.admission_delay, completion.aborted());
                if completion.aborted() { report.chains_aborted += 1; }
                pool.as_mut().expect("pool while requests are active").release(completion.lease);
            }
            _ = async { timeline.clock.as_ref().expect("traffic clock").tick(&mut timeline.clock_tick).await; }, if timeline.clock.is_some() => {
                let (arrival, late) = timeline.schedule.as_mut().expect("traffic schedule").due(Instant::now());
                report.offered += late + u64::from(arrival.is_some());
                let h = report.diagnostics.phase_mut(phase); h.offered += late + u64::from(arrival.is_some()); h.skipped_late += late;
                report.skipped_late += late;
                if let Some(scheduled) = arrival {
                    if let Some((vu, slot)) = slots.iter_mut().enumerate().find(|(_, s)| !s.active) {
                        if let Some(lease) = pool.as_mut().expect("traffic pool").acquire() {
                            slot.send_state.reset();
                            slot.send_recorded = false;
                            let admitted = Instant::now();
                            let endpoint = Arc::clone(&pool.as_ref().expect("traffic pool").endpoint);
                            let running_index = mixture.next();
                            let running = Arc::clone(&plan.chains[running_index]);
                            slot.chain = running.name;
                            // The iteration number is settled before the job is built:
                            // it is what the iteration generates its values from, so a
                            // job carrying the previous one would send that one's row.
                            slot.iteration = plan.iteration_base + report.admitted;
                            let future = chain::run(Some(chain::Job { lease, endpoint, chain: running, datasets: Arc::clone(&plan.datasets), generators: Arc::clone(&plan.generators), auth: plan.auth.clone(), samples: Arc::clone(&plan.samples), sessions: Arc::clone(&plan.sessions), chain_index: running_index, seed: plan.seed, iteration: slot.iteration, vu, scheduled, admitted, send_state: Arc::clone(&slot.send_state) }));
                            assert!(slot.future.try_set(future).is_ok(), "request future layout changed");
                            slot.active = true;
                            slot.worker = report.admitted as usize % workers.len();
                            slot.admitted = admitted;
                            slot.phase = phase;
                            if phase == Phase::Warmup { warmup_workers[slot.worker].start_chain(slot.chain); } else { workers[slot.worker].start_chain(slot.chain); }
                            active += 1;
                            report.admitted += 1;
                            report.peak_in_flight = report.peak_in_flight.max(active);
                        } else { report.skipped_connections += 1; report.diagnostics.phase_mut(phase).skipped_connections += 1; }
                    } else { report.skipped_concurrency += 1; report.diagnostics.phase_mut(phase).skipped_concurrency += 1; }
                }
            }
        }
    }
    if let Some(output) = output {
        output.percentiles(
            &report.metrics,
            measure_from_ms.expect("output clock"),
            report.interrupted || report.stopped_because.is_some(),
        );
    }
    // A fixed run finishes the timeline it was given and then says whether the
    // numbers it produced can be believed (§16). Nothing stopped it, so the reason
    // is not a `stopped_because`; it is the Invalid annotation the rest of the
    // system already reads for run validity, and the exit code.
    if !report.interrupted && plan.breakpoint.is_none() {
        let overridden =
            plan.allow_generator_limited && plan.headroom_ratio.is_some_and(|ratio| ratio > 0.9);
        if let Some(evidence) = overridden
            .then_some("calibrated_ceiling")
            .or_else(|| crate::breakpoint::limiting(&report, false))
        {
            report.generator_limited = true;
            if let Some(output) = output {
                output.note(
                    "generator_limited",
                    metrix_metrics::Severity::Invalid,
                    serde_json::json!({"evidence": evidence, "rate": plan.rate}),
                );
            }
        }
    }
    report.slo = crate::slo::evaluate(
        &plan.slos,
        &report.metrics,
        report.diagnostics.measure.observed.as_secs_f64(),
    );
    if let Some(output) = output {
        if !plan.slos.is_empty() {
            let seconds = report.diagnostics.measure.observed.as_secs_f64();
            output.note(
                "slo_verdicts",
                metrix_metrics::Severity::Info,
                serde_json::json!({
                    "rate": plan.rate, "from_ms": report.measured_from_ms, "seconds": seconds,
                    "verdicts": report.slo,
                    "evidence": crate::slo::evidence(&plan.slos, &report.metrics, seconds),
                }),
            );
        }
    }
    Ok(report)
}
