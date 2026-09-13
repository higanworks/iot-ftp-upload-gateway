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

/// Logging is initialized before config loading (and doesn't go through the layered
/// defaults/YAML/env `Config` system, since it has to be set up before anything else can log
/// at all): `GATEWAY_LOG_FORMAT=json` switches to structured JSON output, one object per line,
/// suited to log processors like CloudWatch Logs Insights that parse JSON fields directly.
/// Anything else (including unset) keeps the default human-readable text format.
fn init_tracing() {
    let json_format =
        std::env::var("GATEWAY_LOG_FORMAT").is_ok_and(|value| value.eq_ignore_ascii_case("json"));

    if json_format {
        tracing_subscriber::fmt().json().flatten_event(true).init();
    } else {
        tracing_subscriber::fmt::init();
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();

    let cli = Cli::parse();
    let config = Config::load(cli.config.as_deref())?;
    tracing::info!(?config, "configuration loaded");

    iot_ftp_upload_gateway::run(config).await
}
