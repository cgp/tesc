//! Standalone bundle execution and the shared schema generator.

use clap::Parser;
use metrix_engine::{Output, OutputReport, Plan, calibrate, run_with_output, write_profile};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Parser)]
#[command(about = "Run a fixed-rate HTTP plan without a control plane")]
struct Args {
    /// Bundle directory. Supports one static call, one target and a complete phase timeline.
    #[arg(
        long,
        required_unless_present = "emit_schemas",
        conflicts_with = "emit_schemas"
    )]
    plan: Option<PathBuf>,
    /// Replace the bundle targets document.
    #[arg(long)]
    targets: Option<PathBuf>,
    /// Write JSON Schemas generated from the shared Rust types.
    #[arg(long)]
    emit_schemas: Option<PathBuf>,
    /// Measure this machine for the bundle's request shape and write machine-profile.json.
    #[arg(long, conflicts_with = "emit_schemas")]
    calibrate: bool,
    /// NDJSON interval summaries and lifecycle records. '-' writes to stdout.
    #[arg(long, default_value = "-")]
    summary: PathBuf,
    /// Optional NDJSON request records and lifecycle records.
    #[arg(long)]
    events: Option<PathBuf>,
    /// Fraction of request events retained; summaries always include every observation.
    #[arg(long, default_value_t = 1.0)]
    sample_rate: f64,
    /// Run one chain of the mixture on its own, at the whole rate.
    ///
    /// For working on a plan rather than measuring with one: a chain at 3% sends a
    /// request every few seconds, and finding out whether its extraction works
    /// should not take four minutes.
    #[arg(long)]
    chain: Option<String>,
    /// Deterministic request-event sampling seed.
    #[arg(long, default_value_t = 0)]
    seed: u64,
}

/// How much stack the scheduler thread and the runtime's workers get.
///
/// Chosen rather than inherited: see where it is used. Generous because the cost of
/// reserving address space is nothing next to the cost of finding out the hard way.
const SCHEDULER_STACK: usize = 16 * 1024 * 1024;

/// Each schema file and the type it is generated from.
macro_rules! schemas {
    ($($file:literal => $ty:ty),* $(,)?) => {
        fn write_schemas(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
            std::fs::create_dir_all(dir)?;
            let mut written = Vec::new();
            $(
                let schema = schemars::schema_for!($ty);
                let mut json = serde_json::to_string_pretty(&schema)
                    .expect("a generated schema always serializes");
                json.push('\n');
                let path = dir.join($file);
                std::fs::write(&path, json)?;
                written.push(path);
            )*
            Ok(written)
        }
    };
}

schemas! {
    "call.schema.json"    => metrix_plan::CallFile,
    "mix.schema.json"     => metrix_plan::Mix,
    "targets.schema.json" => metrix_plan::Targets,
    "events.schema.json"  => metrix_metrics::Record,
}

fn main() -> ExitCode {
    match execute(Args::parse()) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("metrix-engine: {error}");
            ExitCode::FAILURE
        }
    }
}

fn execute(args: Args) -> Result<ExitCode, String> {
    if let Some(dir) = args.emit_schemas {
        for path in write_schemas(&dir).map_err(|e| format!("writing schemas: {e}"))? {
            println!("{}", path.display());
        }
        return Ok(ExitCode::SUCCESS);
    }
    let plan_path = args.plan.expect("clap requires --plan");
    let mut plan = if args.calibrate {
        Plan::load_for_calibration(&plan_path)?
    } else if let Some(targets) = &args.targets {
        Plan::load_with_targets(&plan_path, targets)?
    } else {
        Plan::load(&plan_path)?
    };
    if args.calibrate {
        let profile = calibrate(&plan)?;
        write_profile(plan.bundle_root(), &profile)?;
        serde_json::to_writer_pretty(std::io::stdout(), &profile)
            .map_err(|_| "cannot write calibration result")?;
        println!();
        eprintln!(
            "wrote {} ({})",
            plan.bundle_root().join("machine-profile.json").display(),
            profile.id
        );
        return Ok(ExitCode::SUCCESS);
    }
    // The seed names one run of the plan rather than the plan, so it arrives on the
    // command line and is recorded in the run's identity. Everything generated --
    // which dataset row an iteration reads, what `uuid()` returns -- comes from it,
    // so a run can be replayed as the same workload.
    plan.set_seed(args.seed);
    if let Some(chain) = &args.chain {
        plan.only_chain(chain)?;
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(plan.worker_threads)
        .thread_stack_size(SCHEDULER_STACK)
        .enable_all()
        .build()
        .map_err(|_| "cannot create engine runtime")?;
    let output = Output::open(
        &plan,
        &args.summary,
        args.events.as_deref(),
        args.sample_rate,
        args.seed,
    )?;
    // run() polls shutdown first, registering Ctrl-C before network setup.
    //
    // On a stack this run asked for rather than the one the linker chose for `main`.
    // The scheduler is a single state machine that owns preallocated per-worker
    // accumulators and polls a chain iteration inline, so its frame is large by
    // design, and the HTTP/2 send path adds a good deal more on top. A megabyte is
    // enough until it is not, and the way it stops being enough is a stack overflow
    // part-way through a run rather than an error anybody can act on.
    let result = std::thread::scope(|scope| {
        let spawned = std::thread::Builder::new()
            .name("scheduler".into())
            .stack_size(SCHEDULER_STACK)
            .spawn_scoped(scope, move || {
                let result = runtime.block_on(async {
                    let mut signal_error = None;
                    let report = run_with_output(
                        plan,
                        async {
                            signal_error = tokio::signal::ctrl_c().await.err();
                        },
                        &output,
                    )
                    .await?;
                    if signal_error.is_some() {
                        Err("cannot register Ctrl-C handler".to_owned())
                    } else {
                        Ok(report)
                    }
                });
                // OS DNS resolution uses blocking runtime tasks and cannot itself be
                // cancelled. Do not let one outlive the declared deadline by blocking
                // runtime teardown.
                runtime.shutdown_timeout(std::time::Duration::from_millis(100));
                // Closed here rather than back on the calling thread, because an
                // `Output` owns the receiving end of its writer channels and is not
                // shareable across threads. It goes in with the run and the run's
                // delivery report comes back out with it.
                let code = match &result {
                    Ok(report) if report.interrupted => 130,
                    Ok(_) => 0,
                    Err(_) => 1,
                };
                let stopped = match &result {
                    Ok(report) if report.interrupted => Some("interrupted".into()),
                    Err(_) => Some("setup or internal failure".into()),
                    _ => None,
                };
                (result, code, output.finish(code, stopped))
            });
        match spawned {
            Ok(handle) => handle.join().unwrap_or_else(|_| {
                (
                    Err("the scheduler thread failed".to_owned()),
                    1,
                    OutputReport::default(),
                )
            }),
            Err(_) => (
                Err("cannot start the scheduler thread".to_owned()),
                1,
                OutputReport::default(),
            ),
        }
    });
    let (result, _code, delivery) = result;
    eprintln!(
        "summaries_dropped={} events_dropped={} events_sampled_out={} writers_unfinished={} output_failed={}",
        delivery.summaries_dropped,
        delivery.events_dropped,
        delivery.events_sampled_out,
        delivery.writers_unfinished,
        delivery.failed
    );
    let report = result?;
    eprintln!(
        "offered={} admitted={} sent={} sent_finished={} responses={} failed={} timed_out={} cancelled={} skipped_late={} skipped_concurrency={} skipped_connections={} peak_in_flight={} max_send_drift_ms={:.3} drift_samples={} max_scheduler_lag_ms={:.3} scheduler_lag_samples={} interrupted={}",
        report.offered,
        report.admitted,
        report.sent,
        report.sent_finished,
        report.responses,
        report.failed,
        report.timed_out,
        report.cancelled,
        report.skipped_late,
        report.skipped_concurrency,
        report.skipped_connections,
        report.peak_in_flight,
        report.max_send_drift.as_secs_f64() * 1000.0,
        report.metrics.drift.count() + report.warmup_metrics.drift.count(),
        report.max_scheduler_lag.as_secs_f64() * 1000.0,
        report.scheduler_lag_samples,
        report.interrupted
    );
    eprintln!(
        "measured_started={} measured_completed={} warmup_started={} warmup_completed={}",
        report.metrics.counters.started,
        report.metrics.counters.completed,
        report.warmup_metrics.counters.started,
        report.warmup_metrics.counters.completed
    );
    Ok(if report.interrupted {
        ExitCode::from(130)
    } else if delivery.failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}
