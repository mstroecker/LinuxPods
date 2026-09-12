//! Logs every Apple manufacturer-data advertisement, proximity or not.
//!
//! Runs the scanner without the GTK app or the battery provider, so it can watch
//! alongside a running LinuxPods instance.
//!
//! Usage: RUST_LOG=linuxpods=trace cargo run --example apple_sniff

use futures_util::StreamExt;
use linuxpods::ble::scanner::Scanner;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "linuxpods=trace".into()),
        )
        .init();

    let scanner = Scanner::new().await?;
    scanner.start_discovery().await?;
    let stream = scanner.advertisements().await?;
    tokio::pin!(stream);

    tracing::info!("sniffing Apple manufacturer data - ctrl-c to stop");
    while let Some(advert) = stream.next().await {
        tracing::info!(
            "proximity from {}: model=0x{:04X}",
            advert.ble_mac,
            advert.data.device_model
        );
    }
    Ok(())
}
