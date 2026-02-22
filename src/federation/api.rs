//! Federation HTTP API — Axum handlers for peer-facing endpoints.
//!
//! These endpoints are served on the federation port (separate from the main
//! gateway) and are authenticated via shared secret bearer tokens.
//!
//! | Endpoint                      | Method | Purpose                    |
//! |-------------------------------|--------|----------------------------|
//! | `/api/federation/agents`      | GET    | List exposed agents        |
//! | `/api/federation/relay`       | POST   | Relay message to an agent  |
//! | `/api/federation/health`      | GET    | Health check               |

use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;

use orra::channels::federation::{HealthStatus, RelayRequest, RelayResponse};
use orra::context::CharEstimator;
use orra::message::Message;
use orra::namespace::Namespace;
use orra::runtime::Runtime;

use super::FederationService;

// ---------------------------------------------------------------------------
// Federation-specific app state
// ---------------------------------------------------------------------------

/// State shared by federation API handlers.
#[derive(Clone)]
pub struct FederationState {
    pub service: Arc<FederationService>,
    /// Named agent runtimes (shared with main app).
    pub runtimes:
        Arc<tokio::sync::RwLock<std::collections::HashMap<String, Arc<Runtime<CharEstimator>>>>>,
}

// ---------------------------------------------------------------------------
// Auth middleware helper
// ---------------------------------------------------------------------------

/// Extract and validate the bearer token from the Authorization header.
fn validate_auth(headers: &HeaderMap, service: &FederationService) -> Result<(), StatusCode> {
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .ok_or(StatusCode::UNAUTHORIZED)?;

    let token = auth
        .strip_prefix("Bearer ")
        .ok_or(StatusCode::UNAUTHORIZED)?;

    if !service.validate_secret(token) {
        return Err(StatusCode::UNAUTHORIZED);
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// GET /api/federation/agents
// ---------------------------------------------------------------------------

/// Returns the list of agents this instance exposes to peers.
pub async fn list_agents(
    State(state): State<FederationState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, StatusCode> {
    validate_auth(&headers, &state.service)?;

    let agents = state.service.exposed_agents();
    Ok(Json(agents))
}

// ---------------------------------------------------------------------------
// POST /api/federation/relay
// ---------------------------------------------------------------------------

/// Relay a message to a local agent on behalf of a remote peer.
pub async fn relay_message(
    State(state): State<FederationState>,
    headers: HeaderMap,
    Json(request): Json<RelayRequest>,
) -> Result<impl IntoResponse, StatusCode> {
    validate_auth(&headers, &state.service)?;

    let agent_key = request.agent.to_lowercase();

    // Check if the requested agent is exposed
    let exposed = state.service.exposed_agents();
    if !exposed.iter().any(|a| a.name.to_lowercase() == agent_key) {
        return Err(StatusCode::NOT_FOUND);
    }

    // Find the runtime for this agent
    let runtimes = state.runtimes.read().await;
    let runtime = runtimes
        .get(&agent_key)
        .cloned()
        .ok_or(StatusCode::NOT_FOUND)?;
    drop(runtimes);

    // Create a federation namespace for session tracking
    let ns = Namespace::parse(&request.namespace);

    // Run the message through the agent's runtime
    let result = runtime
        .run(&ns, Message::user(&request.message))
        .await
        .map_err(|e| {
            eprintln!(
                "[federation] relay to '{}' failed: {e}",
                request.agent
            );
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let response = RelayResponse {
        message: result.final_message.content,
        agent: request.agent,
        instance: state.service.instance_name().to_string(),
    };

    Ok(Json(response))
}

// ---------------------------------------------------------------------------
// GET /api/federation/health
// ---------------------------------------------------------------------------

/// Returns health status of this instance.
pub async fn health(
    State(state): State<FederationState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, StatusCode> {
    validate_auth(&headers, &state.service)?;

    let agents = state.service.exposed_agents();
    let status = HealthStatus {
        instance: state.service.instance_name().to_string(),
        status: "ok".into(),
        agent_count: agents.len(),
    };

    Ok(Json(status))
}

// ---------------------------------------------------------------------------
// Router builder
// ---------------------------------------------------------------------------

/// Create the federation API router.
pub fn federation_router(state: FederationState) -> axum::Router {
    use axum::routing::{get, post};

    axum::Router::new()
        .route("/api/federation/agents", get(list_agents))
        .route("/api/federation/relay", post(relay_message))
        .route("/api/federation/health", get(health))
        .with_state(state)
}
