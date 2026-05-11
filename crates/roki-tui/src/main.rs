use anyhow::Result;

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    #[cfg(windows)]
    {
        eprintln!("roki-tui: Windows is not supported in v1");
        std::process::exit(1);
    }
    #[cfg(not(windows))]
    {
        roki_tui::app::App::run().await
    }
}
