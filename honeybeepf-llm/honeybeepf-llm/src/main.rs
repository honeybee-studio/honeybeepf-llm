use anyhow::{Context, Result};
use aya::include_bytes_aligned;
use clap::Parser;

#[derive(Debug, Parser)]
struct Opt {
    /// Enable verbose output (sets log level to INFO)
    #[clap(short, long)]
    verbose: bool,

    /// Exit 0 if the heartbeat file was rewritten in the last 60 s, else 1 (the chart's probes; the image has no shell)
    #[clap(long)]
    check_heartbeat: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let opt = Opt::parse();
    if opt.check_heartbeat {
        std::process::exit(if honeybeepf_llm::health::heartbeat_is_fresh() {
            0
        } else {
            1
        });
    }

    // RUST_LOG picks the levels (else info with --verbose, warn without) and LOG_FORMAT text or json.
    honeybeepf_llm::logging::init(opt.verbose);
    honeybeepf_llm::audit::startup(env!("CARGO_PKG_VERSION"));

    // Load agent settings from environment variables or a .env file.
    let settings = honeybeepf_llm::settings::Settings::new().context("Failed to load settings")?;

    // Load the eBPF bytecode and initialize the HoneyBee engine.
    // include_bytes_aligned ensures the bytecode is correctly aligned in memory for Aya.
    let engine = honeybeepf_llm::HoneyBeeEngine::new(
        settings,
        include_bytes_aligned!(concat!(env!("OUT_DIR"), "/honeybeepf-llm")),
    )?;

    // Start the main event loop and attach probes to the kernel.
    engine.run().await?;

    Ok(())
}
