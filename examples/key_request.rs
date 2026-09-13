//! Exercises the Request Keys path end to end, without the GUI.
//!
//! Connects over AAP, which starts the coordinator's read loop, then issues a
//! key request while that loop is parked in recv - the exact situation that
//! used to deadlock on the aap_client mutex.
//!
//! Usage: cargo run --example key_request -- <MAC>

use std::time::Duration;

use linuxpods::podstate::Coordinator;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .init();

    let mac = std::env::args().nth(1).expect("usage: key_request <MAC>");
    let coordinator = Coordinator::new().await?;

    println!("connecting AAP to {mac}...");
    coordinator.connect_aap(&mac).await?;
    println!("connected");

    // Let the read loop park in recv.
    tokio::time::sleep(Duration::from_millis(500)).await;

    println!("requesting encryption keys (read loop is parked in recv)...");
    match tokio::time::timeout(
        Duration::from_secs(5),
        coordinator.request_encryption_keys(&mac),
    )
    .await
    {
        Ok(Ok(())) => println!("  request returned OK"),
        Ok(Err(e)) => println!("  request FAILED: {e:#}"),
        Err(_) => {
            println!("  request TIMED OUT after 5s - the mutex deadlock is back");
            return Ok(());
        }
    }

    // Give the AirPods time to answer; the read loop stores any key it sees.
    println!("waiting for key response...");
    for i in 1..=10 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if coordinator.has_encryption_keys().await {
            println!("  keys present after {}ms", i * 500);
            break;
        }
    }

    println!("device_count={}", coordinator.device_count().await);
    Ok(())
}
