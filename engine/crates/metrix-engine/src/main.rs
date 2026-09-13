//! Standalone bundle execution and the shared schema generator.

use clap::Parser;
use metrix_engine::{Plan, run};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Parser)]
#[command(about = "Run a fixed-rate HTTP plan without a control plane")]
struct Args {
    /// Bundle directory. B1.2 supports one static call, one target and zero phases.
    #[arg(
        long,
        required_unless_present = "emit_schemas",
        conflicts_with = "emit_schemas"
    )]
    plan: Option<PathBuf>,
    /// Write JSON Schemas generated from the shared Rust types.
    #[arg(long)]
    emit_schemas: Option<PathBuf>,
}

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
    let plan = Plan::load(&args.plan.expect("clap requires --plan"))?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(plan.worker_threads)
        .enable_all()
        .build()
        .map_err(|_| "cannot create engine runtime")?;
    // run() polls shutdown first, registering Ctrl-C before network setup.
    let result = runtime.block_on(async {
        let mut signal_error = None;
        let report = run(plan, async {
            signal_error = tokio::signal::ctrl_c().await.err();
        })
        .await?;
        if signal_error.is_some() {
            Err("cannot register Ctrl-C handler".to_owned())
        } else {
            Ok(report)
        }
    });
    // OS DNS resolution uses blocking runtime tasks and cannot itself be cancelled.
    // Do not let one outlive the declared deadline by blocking runtime teardown.
    runtime.shutdown_timeout(std::time::Duration::from_millis(100));
    let report = result?;
    eprintln!(
        "offered={} admitted={} sent_finished={} responses={} failed={} timed_out={} cancelled={} skipped_late={} skipped_concurrency={} skipped_connections={} peak_in_flight={} max_send_drift_ms={:.3} drift_samples={} interrupted={}",
        report.offered,
        report.admitted,
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
        report.sent_finished,
        report.interrupted
    );
    Ok(if report.interrupted {
        ExitCode::from(130)
    } else {
        ExitCode::SUCCESS
    })
}
