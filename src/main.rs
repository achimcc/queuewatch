//! The program around the library: read the configuration, then either
//! collect or serve. Every step fails closed.

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Context;
use queuewatch::config::Config;

const USAGE: &str = "usage: queuewatch <collect|serve|--version>";

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let result = match arguments.first().map(String::as_str) {
        Some("--version") => {
            println!("queuewatch {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Some("collect") => run(|config| async move {
            let collect = config
                .collect
                .context("the configuration has no [collect] section")?;
            queuewatch::collect::run(collect).await
        }),
        Some("serve") => run(|config| async move {
            let serve = config
                .serve
                .context("the configuration has no [serve] section")?;
            queuewatch::serve::run(serve).await
        }),
        _ => Err(anyhow::anyhow!(USAGE)),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("queuewatch: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run<F, Fut>(f: F) -> anyhow::Result<()>
where
    F: FnOnce(Config) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>>,
{
    let path = PathBuf::from(
        std::env::var("QUEUEWATCH_CONFIG")
            .unwrap_or_else(|_| "/etc/queuewatch/queuewatch.toml".into()),
    );
    let config = Config::load(&path)?;
    tokio::runtime::Runtime::new()
        .context("starting the runtime")?
        .block_on(f(config))
}
