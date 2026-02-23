//! Federation lifecycle manager — supports hot-reload of federation settings.
//!
//! Wraps [`FederationService`] with start/stop/restart semantics, similar to
//! [`DiscordManager`](crate::discord_manager::DiscordManager).

use crate::hlog;
use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use orra::context::CharEstimator;
use orra::runtime::Runtime;
use orra::store::SessionStore;

use super::api::FederationState;
use super::{FederationService, LocalAgentInfo};
use crate::config::FederationConfig;

/// Manages the federation service lifecycle, allowing hot-reload of settings.
pub struct FederationManager {
    /// The currently active federation service (None when stopped/disabled).
    service: RwLock<Option<Arc<FederationService>>>,
    /// JoinHandles for background tasks (discovery, health checks, mDNS).
    task_handles: RwLock<Vec<JoinHandle<()>>>,
    /// CancellationToken to signal all background tasks to stop.
    cancel_token: RwLock<Option<CancellationToken>>,
    /// JoinHandle for the federation HTTP API server.
    api_handle: RwLock<Option<JoinHandle<()>>>,
    /// Shared runtimes (needed to create FederationState for the API server).
    runtimes: Arc<RwLock<HashMap<String, Arc<Runtime<CharEstimator>>>>>,
    /// Shared session store.
    store: Arc<dyn SessionStore>,
}

impl FederationManager {
    /// Create a new manager (not running).
    pub fn new(
        runtimes: Arc<RwLock<HashMap<String, Arc<Runtime<CharEstimator>>>>>,
        store: Arc<dyn SessionStore>,
    ) -> Self {
        Self {
            service: RwLock::new(None),
            task_handles: RwLock::new(Vec::new()),
            cancel_token: RwLock::new(None),
            api_handle: RwLock::new(None),
            runtimes,
            store,
        }
    }

    /// Get the currently active service (if running).
    pub async fn service(&self) -> Option<Arc<FederationService>> {
        self.service.read().await.clone()
    }

    /// Whether federation is currently running.
    pub async fn is_running(&self) -> bool {
        self.service.read().await.is_some()
    }

    /// Start the federation service with the given config.
    ///
    /// Creates a new `FederationService`, spawns background tasks with
    /// cancellation support, and starts the federation HTTP API server.
    pub async fn start(
        &self,
        config: FederationConfig,
        local_agents: Vec<LocalAgentInfo>,
        gateway_port: u16,
    ) -> Result<(), String> {
        // Stop any existing service first
        self.stop().await;

        let cancel_token = CancellationToken::new();
        let service = Arc::new(FederationService::new(config, local_agents));

        // Start background tasks (peer discovery, health checks, mDNS)
        let handles = service.start(gateway_port, cancel_token.clone()).await;

        // Start the federation HTTP API server
        let federation_port = service.port(gateway_port);
        let fed_state = FederationState {
            service: service.clone(),
            runtimes: self.runtimes.clone(),
            store: self.store.clone(),
        };
        let fed_router = super::api::federation_router(fed_state);
        let fed_addr = format!("0.0.0.0:{federation_port}");
        let api_token = cancel_token.clone();

        let api_handle = tokio::spawn(async move {
            match tokio::net::TcpListener::bind(&fed_addr).await {
                Ok(listener) => {
                    hlog!("[federation] API listening on http://{fed_addr}");
                    if let Err(e) = axum::serve(listener, fed_router)
                        .with_graceful_shutdown(api_token.cancelled_owned())
                        .await
                    {
                        hlog!("[federation] API server error: {e}");
                    }
                    hlog!("[federation] API server stopped");
                }
                Err(e) => {
                    hlog!("[federation] Failed to bind {fed_addr}: {e}");
                }
            }
        });

        hlog!(
            "[federation] Started (instance: '{}', port: {})",
            service.instance_name(),
            federation_port,
        );

        // Store everything
        *self.service.write().await = Some(service);
        *self.task_handles.write().await = handles;
        *self.cancel_token.write().await = Some(cancel_token);
        *self.api_handle.write().await = Some(api_handle);

        Ok(())
    }

    /// Stop the federation service gracefully.
    ///
    /// Cancels all background tasks and the API server, waits for them to
    /// finish (with timeout), then clears the service.
    pub async fn stop(&self) {
        // Cancel all background tasks
        if let Some(token) = self.cancel_token.write().await.take() {
            token.cancel();
            hlog!("[federation] Cancellation signal sent");
        }

        // Wait for background tasks to finish (with timeout)
        let handles: Vec<JoinHandle<()>> = self.task_handles.write().await.drain(..).collect();
        if !handles.is_empty() {
            let count = handles.len();
            let wait_all = futures_join_all(handles);
            match tokio::time::timeout(std::time::Duration::from_secs(5), wait_all).await {
                Ok(_) => hlog!("[federation] {count} background tasks stopped"),
                Err(_) => hlog!("[federation] Timeout waiting for {count} background tasks"),
            }
        }

        // Stop the API server
        if let Some(handle) = self.api_handle.write().await.take() {
            // The API server should already be shutting down due to cancellation.
            // Give it a short grace period.
            match tokio::time::timeout(std::time::Duration::from_secs(3), handle).await {
                Ok(_) => hlog!("[federation] API server stopped"),
                Err(_) => hlog!("[federation] Timeout waiting for API server"),
            }
        }

        // Clear the service
        *self.service.write().await = None;
    }

    /// Restart with new config. Equivalent to stop() + start().
    pub async fn restart(
        &self,
        config: FederationConfig,
        local_agents: Vec<LocalAgentInfo>,
        gateway_port: u16,
    ) -> Result<(), String> {
        self.stop().await;
        self.start(config, local_agents, gateway_port).await
    }
}

/// Join all handles, ignoring panics (mirrors `futures::future::join_all`
/// without adding a dep).
async fn futures_join_all(handles: Vec<JoinHandle<()>>) {
    for handle in handles {
        let _ = handle.await;
    }
}
