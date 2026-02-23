//! mDNS service discovery for federation.
//!
//! Registers this herald instance as `_herald._tcp.local.` and browses for
//! other instances on the local network.

use crate::hlog;
use super::client::PeerClient;
use super::{PeerHealth, PeerRegistry, PeerSource, PeerState};
use tokio_util::sync::CancellationToken;

/// mDNS service type for herald federation.
const SERVICE_TYPE: &str = "_herald._tcp.local.";

/// Register this instance as an mDNS service.
///
/// This blocks (via mDNS daemon loop) — run in a `tokio::spawn`.
/// Accepts a `CancellationToken` for cooperative shutdown.
pub async fn register_service(
    instance_name: &str,
    port: u16,
    cancel_token: CancellationToken,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mdns = mdns_sd::ServiceDaemon::new()?;

    let service_info = mdns_sd::ServiceInfo::new(
        SERVICE_TYPE,
        instance_name,
        &format!("{instance_name}.local."),
        "",
        port,
        None,
    )
    .map_err(|e| format!("failed to create service info: {e}"))?;

    mdns.register(service_info)?;

    hlog!(
        "[federation] mDNS: registered as '{instance_name}' on port {port}"
    );

    // Keep the daemon alive until cancelled.
    // The ServiceDaemon runs in its own background threads;
    // we hold it alive and sleep until shutdown is requested.
    loop {
        tokio::select! {
            _ = cancel_token.cancelled() => {
                hlog!("[federation] mDNS: unregistering service");
                let _ = mdns.shutdown();
                break;
            }
            _ = tokio::time::sleep(std::time::Duration::from_secs(3600)) => {}
        }
    }
    Ok(())
}

/// Browse for peer herald instances via mDNS and update the registry.
///
/// This runs continuously — launch in a `tokio::spawn`.
/// Accepts a `CancellationToken` for cooperative shutdown.
pub async fn browse_peers(
    registry: PeerRegistry,
    own_instance: &str,
    global_secret: &str,
    cancel_token: CancellationToken,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mdns = mdns_sd::ServiceDaemon::new()?;
    let receiver = mdns.browse(SERVICE_TYPE)?;

    hlog!("[federation] mDNS: browsing for peers...");

    let own_instance = own_instance.to_string();
    let global_secret = global_secret.to_string();

    loop {
        tokio::select! {
            _ = cancel_token.cancelled() => {
                hlog!("[federation] mDNS: browse task shutting down");
                let _ = mdns.shutdown();
                break;
            }
            result = receiver.recv_async() => {
                match result {
                    Ok(event) => match event {
                        mdns_sd::ServiceEvent::ServiceResolved(info) => {
                            let peer_name = info.get_fullname().to_string();

                            // Skip our own instance
                            if peer_name.contains(&own_instance) {
                                continue;
                            }

                            // Extract the instance name from the full name
                            // Full name format: "instance._herald._tcp.local."
                            let short_name = peer_name
                                .split('.')
                                .next()
                                .unwrap_or(&peer_name)
                                .to_string();

                            let port = info.get_port();
                            let addresses = info.get_addresses();

                            if let Some(addr) = addresses.iter().next() {
                                let url = format!("http://{addr}:{port}");
                                hlog!(
                                    "[federation] mDNS: discovered peer '{short_name}' at {url}"
                                );

                                // Try to discover agents from this peer
                                match PeerClient::discover_agents(&url, &global_secret).await {
                                    Ok(agents) => {
                                        registry
                                            .update_peer(PeerState {
                                                name: short_name,
                                                url,
                                                shared_secret: global_secret.clone(),
                                                agents,
                                                health: PeerHealth::Healthy,
                                                source: PeerSource::Mdns,
                                            })
                                            .await;
                                    }
                                    Err(e) => {
                                        hlog!(
                                            "[federation] mDNS: failed to discover agents from '{peer_name}': {e}"
                                        );
                                        // Still register the peer so health checks can find it later
                                        registry
                                            .update_peer(PeerState {
                                                name: short_name,
                                                url,
                                                shared_secret: global_secret.clone(),
                                                agents: vec![],
                                                health: PeerHealth::Unknown,
                                                source: PeerSource::Mdns,
                                            })
                                            .await;
                                    }
                                }
                            }
                        }
                        mdns_sd::ServiceEvent::ServiceRemoved(_, fullname) => {
                            let short_name = fullname
                                .split('.')
                                .next()
                                .unwrap_or(&fullname)
                                .to_string();

                            // Only remove mDNS-discovered peers, not static ones
                            let peers = registry.list_peers().await;
                            if let Some(peer) = peers.iter().find(|p| p.name == short_name) {
                                if peer.source == PeerSource::Mdns {
                                    hlog!(
                                        "[federation] mDNS: peer '{short_name}' removed"
                                    );
                                    registry.remove_peer(&short_name).await;
                                }
                            }
                        }
                        _ => {
                            // SearchStarted, SearchStopped, etc. — ignore
                        }
                    },
                    Err(e) => {
                        hlog!("[federation] mDNS browse error: {e}");
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    }
                }
            }
        }
    }
    Ok(())
}
