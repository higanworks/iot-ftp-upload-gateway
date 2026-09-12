use std::path::PathBuf;

use clap::Parser;
use iot_ftp_upload_gateway::config::Config;

#[derive(Parser)]
#[command(name = "iot-ftp-upload-gateway")]
struct Cli {
    /// Path to a YAML config file. Environment variables override values from this file.
    #[arg(long)]
    config: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();
    let config = Config::load(cli.config.as_deref())?;
    tracing::info!(?config, "configuration loaded");

    iot_ftp_upload_gateway::run(config).await
}
