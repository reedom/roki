use std::process::ExitCode;

mod admission;
pub mod api;
mod capture;
mod cli;
mod config;
mod daemon;
mod engine;
pub mod error;
mod escalation;
pub mod events;
mod linear;
pub mod observability;
mod rule;
mod runtime;
pub mod store_handle;
mod workflow;

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
