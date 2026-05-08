use std::process::ExitCode;

mod admission;
mod capture;
mod cli;
mod config;
mod daemon;
mod engine;
pub mod error;
pub mod events;
mod linear;
mod rule;
mod runtime;

#[tokio::main]
async fn main() -> ExitCode {
    // Install the default tracing subscriber as the very first action so every
    // subsequent error / warn / info event surfaces from clap, config load,
    // bind, admission, rule, capture, and runner. The canonical structured
    // pipeline lands with `roki-obs-tracing-pipeline` and will replace this.
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .try_init();

    cli::run().await
}
