pub mod backend;
pub mod config;
pub mod pasv;
pub mod protocol;
pub mod server;
pub mod shutdown;

use anyhow::Result;
use config::Config;

pub async fn run(config: Config) -> Result<()> {
    server::listener::run(config).await
}
