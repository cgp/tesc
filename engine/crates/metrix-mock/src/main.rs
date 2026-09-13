//! Standalone target; no control plane or plan loader required.

use std::{net::SocketAddr, path::PathBuf, process::ExitCode};

use clap::Parser;
use metrix_mock::{Config, MockServer};

#[derive(Parser)]
#[command(about = "HTTP/1.1 and HTTP/2 target with controlled latency, errors, and capacity")]
struct Args {
    /// Strict JSON configuration; omitted means fixed 10ms, no errors or limits.
    #[arg(long)]
    config: Option<PathBuf>,
    /// Bind address; use port 0 to assign a free port. HTTP/2 uses prior knowledge.
    #[arg(long, default_value = "127.0.0.1:8080")]
    listen: SocketAddr,
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();
    match run(args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("metrix-mock: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    let config = match args.config {
        Some(path) => serde_json::from_slice(&std::fs::read(path)?)?,
        None => Config::default(),
    };
    let server = MockServer::bind(args.listen, config).await?;
    println!("metrix-mock listening on http://{}", server.local_addr()?);
    server
        .run_until(async {
            if let Err(error) = tokio::signal::ctrl_c().await {
                eprintln!("metrix-mock: cannot listen for Ctrl-C: {error}");
            }
        })
        .await?;
    Ok(())
}
