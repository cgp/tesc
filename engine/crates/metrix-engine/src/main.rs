//! The binary: runtime, scheduler, HTTP, target loop.
//!
//! Load execution is track B and not started. The one thing implemented here is
//! `--emit-schemas`, which belongs to the shared foundation (F0.3): the Rust types
//! are authoritative, and the JSON Schemas the API validates against are generated
//! from them so the two sides cannot drift.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

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
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("--emit-schemas") => {
            let Some(dir) = args.next() else {
                eprintln!("--emit-schemas needs a directory");
                return ExitCode::FAILURE;
            };
            match write_schemas(Path::new(&dir)) {
                Ok(paths) => {
                    for p in paths {
                        println!("{}", p.display());
                    }
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("writing schemas to {dir}: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Some("--help" | "-h") => {
            println!("metrix-engine --emit-schemas <dir>");
            println!();
            println!("Load execution is not implemented; see docs/implementation-engine.md.");
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("metrix-engine: load execution is not implemented");
            eprintln!("only --emit-schemas <dir> is available (F0.3)");
            ExitCode::FAILURE
        }
    }
}
