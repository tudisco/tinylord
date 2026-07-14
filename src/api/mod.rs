//! HTTP surface: shared state, router assembly, health, and OpenAPI (§7).

pub mod admin;
pub mod browser_auth;
pub mod documents;
pub mod indexes;
pub mod pubsub;
pub mod query_ep;

use crate::auth::Principal;
use crate::config::{Config, CorsConfig};
use crate::db::{DbHandle, DbRegistry};
use crate::encryption::Encryption;
use crate::errors::{ApiError, ApiResult};
use crate::limits::RateGuard;
use crate::system::{Role, System};
use axum::http::{header, HeaderName, HeaderValue, Method};
use axum::response::IntoResponse;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use std::sync::Arc;
use tower_http::cors::CorsLayer;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::trace::TraceLayer;
use tower_http::services::{ServeDir, ServeFile};

/// Application state shared by all handlers. Cheap to clone.
#[derive(Clone)]
pub struct AppState {
    pub system: System,
    pub registry: Arc<DbRegistry>,
    pub rate_guard: Arc<RateGuard>,
    pub login_guard: Arc<LoginGuard>,
    pub config: Arc<Config>,
    /// Held for future use (e.g. per-request keyed operations); the registry and
    /// system already carry their own clones for opening connections.
    #[allow(dead_code)]
    pub encryption: Encryption,
    openapi: Arc<String>,
}

impl AppState {
    pub fn new(
        system: System,
        registry: Arc<DbRegistry>,
        config: Config,
        encryption: Encryption,
    ) -> Self {
        let rate_guard = Arc::new(RateGuard::new(config.limits.rate_per_minute));
        let login_guard = Arc::new(LoginGuard::default());
        let openapi = Arc::new(openapi_doc().to_string());
        Self {
            system,
            registry,
            rate_guard,
            login_guard,
            config: Arc::new(config),
            encryption,
            openapi,
        }
    }

    /// Assert `principal` has at least `needed` role on `db`. Global admins are
    /// NOT implicitly granted data access (§6).
    pub async fn authorize(&self, principal: &Principal, db: &str, needed: Role) -> ApiResult<()> {
        crate::ids::require_valid_name("database", db)?;
        if !self.system.database_exists(db).map_err(ApiError::internal)? {
            return Err(ApiError::not_found("database not found"));
        }
        let role = self
            .system
            .grant_role(&principal.id, db)
            .map_err(ApiError::internal)?;
        match role {
            Some(r) if r >= needed => Ok(()),
            _ => Err(ApiError::forbidden(format!(
                "requires '{}' on database",
                needed.as_str()
            ))),
        }
    }

    /// Authorize the snapshot operation: global admin OR per-db `admin` (§7.2).
    pub async fn authorize_snapshot(&self, principal: &Principal, db: &str) -> ApiResult<()> {
        crate::ids::require_valid_name("database", db)?;
        if !self.system.database_exists(db).map_err(ApiError::internal)? {
            return Err(ApiError::not_found("database not found"));
        }
        if principal.is_admin {
            return Ok(());
        }
        let role = self
            .system
            .grant_role(&principal.id, db)
            .map_err(ApiError::internal)?;
        if role == Some(Role::Admin) {
            Ok(())
        } else {
            Err(ApiError::forbidden("requires global admin or 'admin' on database"))
        }
    }

    /// Open (or reuse) a database handle after verifying it is registered.
    pub async fn open_db(&self, db: &str) -> ApiResult<Arc<DbHandle>> {
        crate::ids::require_valid_name("database", db)?;
        if !self.system.database_exists(db).map_err(ApiError::internal)? {
            return Err(ApiError::not_found("database not found"));
        }
        self.registry.get_or_open(db).await
    }
}

/// Assemble the full router with global middleware layers.
pub fn build_router(state: AppState) -> Router {
    let cors = build_cors(&state.config.cors);
    let body_limit = state.config.limits.request_body_bytes;

    Router::new()
        .route("/health", get(health))
        .route("/openapi.json", get(openapi))
        .route("/tinylord.js", get(browser_library))
        .route("/v1/auth/register", post(browser_auth::register))
        .route("/v1/auth/login", post(browser_auth::login))
        .route("/v1/auth/refresh", post(browser_auth::refresh))
        .route("/v1/auth/logout", post(browser_auth::logout))
        .route("/v1/auth/me", get(browser_auth::me))
        // Admin (§7.2)
        .route(
            "/v1/admin/databases",
            post(admin::create_database).get(admin::list_databases),
        )
        .route("/v1/admin/databases/{db}", delete(admin::delete_database))
        .route(
            "/v1/admin/databases/{db}/snapshot",
            post(admin::snapshot),
        )
        .route(
            "/v1/admin/principals",
            post(admin::create_principal).get(admin::list_principals),
        )
        .route("/v1/admin/principals/password", post(admin::reset_browser_password))
        .route(
            "/v1/admin/auth/registration",
            get(admin::registration_status).put(admin::set_registration),
        )
        .route(
            "/v1/admin/principals/{id}",
            delete(admin::delete_principal),
        )
        .route(
            "/v1/admin/grants",
            post(admin::create_grant).delete(admin::delete_grant),
        )
        // Documents (§7.3)
        .route(
            "/v1/db/{db}/collections/{coll}/documents",
            post(documents::create),
        )
        .route(
            "/v1/db/{db}/collections/{coll}/documents/{id}",
            get(documents::get_doc)
                .put(documents::put_doc)
                .delete(documents::delete_doc),
        )
        // Query & count (§7.5)
        .route(
            "/v1/db/{db}/collections/{coll}/query",
            post(query_ep::query),
        )
        .route(
            "/v1/db/{db}/collections/{coll}/count",
            post(query_ep::count),
        )
        // Indexes (§7.6)
        .route(
            "/v1/db/{db}/collections/{coll}/indexes",
            post(indexes::create).get(indexes::list),
        )
        .route(
            "/v1/db/{db}/collections/{coll}/indexes/{name}",
            delete(indexes::drop),
        )
        // Realtime (§7.7)
        .route(
            "/v1/db/{db}/collections/{coll}/subscribe",
            get(crate::realtime::subscribe),
        )
        // Ephemeral pub/sub channels & presence
        .route(
            "/v1/db/{db}/channels/{channel}/publish",
            post(pubsub::publish),
        )
        .route(
            "/v1/db/{db}/channels/{channel}/subscribe",
            get(pubsub::subscribe),
        )
        .route(
            "/v1/db/{db}/channels/{channel}/presence",
            get(pubsub::presence),
        )
        // A missing API route must never fall through to a static SPA entry.
        .route("/v1/{*path}", axum::routing::any(api_not_found))
        // Global request body limit (§11). Layered outermost so it applies to all.
        .layer(RequestBodyLimitLayer::new(body_limit))
        .layer(cors)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// Add static hosting after all API routes, preserving API precedence.
pub fn with_static_files(app: Router, directory: std::path::PathBuf, spa_fallback: bool) -> Router {
    let files = ServeDir::new(&directory);
    if spa_fallback {
        app.fallback_service(files.not_found_service(ServeFile::new(directory.join("index.html"))))
    } else {
        app.fallback_service(files)
    }
}

async fn api_not_found() -> ApiError { ApiError::not_found("API route not found") }

#[derive(Default)]
pub struct LoginGuard { attempts: std::sync::Mutex<std::collections::HashMap<String, (u32, std::time::Instant)>> }
impl LoginGuard {
    pub fn check(&self, key: &str) -> bool { let mut m = self.attempts.lock().expect("login guard lock"); let e = m.entry(key.to_string()).or_insert((0, std::time::Instant::now())); if e.1.elapsed() >= std::time::Duration::from_secs(60) { *e = (0, std::time::Instant::now()); } e.0 < 5 }
    pub fn fail(&self, key: &str) { let mut m = self.attempts.lock().expect("login guard lock"); let e = m.entry(key.to_string()).or_insert((0, std::time::Instant::now())); e.0 += 1; }
    pub fn success(&self, key: &str) { self.attempts.lock().expect("login guard lock").remove(key); }
}

fn build_cors(cfg: &CorsConfig) -> CorsLayer {
    // Explicit allow-list; never `*` when Authorization is in play (§15).
    let origins: Vec<HeaderValue> = cfg
        .allowed_origins
        .iter()
        .filter_map(|o| {
            if o == "*" {
                tracing::warn!("ignoring CORS origin '*'; wildcards are unsafe with bearer tokens");
                None
            } else {
                o.parse::<HeaderValue>().ok()
            }
        })
        .collect();

    CorsLayer::new()
        .allow_origin(origins)
        .allow_methods(vec![
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::DELETE,
        ])
        .allow_headers(vec![
            header::AUTHORIZATION,
            header::CONTENT_TYPE,
            HeaderName::from_static("x-csrf-token"),
        ])
}

async fn health() -> impl IntoResponse {
    Json(serde_json::json!({ "status": "ok" }))
}

async fn openapi(axum::extract::State(state): axum::extract::State<AppState>) -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "application/json")],
        state.openapi.as_ref().clone(),
    )
}

async fn browser_library() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        include_str!("../../tinylord.js"),
    )
}

/// The document response envelope (§7.4).
#[derive(serde::Serialize)]
pub struct DocEnvelope {
    pub id: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub doc: serde_json::Value,
}

/// Hand-written OpenAPI 3.1 document (§7.1). Kept accurate to the routes above.
fn openapi_doc() -> serde_json::Value {
    use serde_json::json;
    let bearer = json!({ "BearerAuth": [] });
    let ok_json = json!({ "content": { "application/json": {} } });
    json!({
        "openapi": "3.1.0",
        "info": {
            "title": "tinylord",
            "version": "0.1.0",
            "description": "A tiny schemaless realtime document datastore."
        },
        "components": {
            "securitySchemes": {
                "BearerAuth": { "type": "http", "scheme": "bearer" }
            }
        },
        "paths": {
            "/health": { "get": { "summary": "Liveness", "security": [], "responses": {"200": ok_json} } },
            "/openapi.json": { "get": { "summary": "This document", "security": [], "responses": {"200": ok_json} } },
            "/v1/admin/databases": {
                "post": { "summary": "Create database", "security": [bearer], "responses": {"201": ok_json, "409": ok_json} },
                "get": { "summary": "List databases", "security": [bearer], "responses": {"200": ok_json} }
            },
            "/v1/admin/databases/{db}": {
                "delete": { "summary": "Delete database (irreversible)", "security": [bearer], "responses": {"204": {}} }
            },
            "/v1/admin/databases/{db}/snapshot": {
                "post": { "summary": "Consistent snapshot via VACUUM INTO", "security": [bearer], "responses": {"200": ok_json} }
            },
            "/v1/admin/principals": {
                "post": { "summary": "Create principal (token shown once)", "security": [bearer], "responses": {"201": ok_json} },
                "get": { "summary": "List principals with grants for an admin interface", "security": [bearer], "responses": {"200": ok_json} }
            },
            "/v1/admin/principals/password": {
                "post": { "summary": "Reset browser-user password and revoke browser sessions", "security": [bearer], "responses": {"200": ok_json, "404": ok_json} }
            },
            "/v1/admin/principals/{id}": {
                "delete": { "summary": "Disable principal", "security": [bearer], "responses": {"204": {}} }
            },
            "/v1/admin/grants": {
                "post": { "summary": "Upsert grant", "security": [bearer], "responses": {"200": ok_json} },
                "delete": { "summary": "Remove grant", "security": [bearer], "responses": {"204": {}} }
            },
            "/v1/admin/auth/registration": {
                "get": { "summary": "Get public registration policy", "security": [bearer], "responses": {"200": ok_json} },
                "put": { "summary": "Set public registration policy", "security": [bearer], "responses": {"200": ok_json} }
            },
            "/v1/db/{db}/collections/{coll}/documents": {
                "post": { "summary": "Create document", "security": [bearer], "responses": {"201": ok_json} }
            },
            "/v1/db/{db}/collections/{coll}/documents/{id}": {
                "get": { "summary": "Get document", "security": [bearer], "responses": {"200": ok_json, "404": ok_json} },
                "put": { "summary": "Replace/upsert document", "security": [bearer], "responses": {"200": ok_json, "201": ok_json} },
                "delete": { "summary": "Delete document", "security": [bearer], "responses": {"204": {}, "404": ok_json} }
            },
            "/v1/db/{db}/collections/{coll}/query": {
                "post": { "summary": "Query documents", "security": [bearer], "responses": {"200": ok_json} }
            },
            "/v1/db/{db}/collections/{coll}/count": {
                "post": { "summary": "Count documents", "security": [bearer], "responses": {"200": ok_json} }
            },
            "/v1/db/{db}/collections/{coll}/indexes": {
                "post": { "summary": "Create index", "security": [bearer], "responses": {"201": ok_json, "409": ok_json} },
                "get": { "summary": "List indexes", "security": [bearer], "responses": {"200": ok_json} }
            },
            "/v1/db/{db}/collections/{coll}/indexes/{name}": {
                "delete": { "summary": "Drop index", "security": [bearer], "responses": {"204": {}, "404": ok_json} }
            },
            "/v1/db/{db}/collections/{coll}/subscribe": {
                "get": {
                    "summary": "Subscribe to changes (SSE)",
                    "security": [bearer],
                    "responses": {"200": { "content": { "text/event-stream": {} } }}
                }
            },
            "/v1/db/{db}/channels/{channel}/publish": {
                "post": {
                    "summary": "Publish an ephemeral channel event",
                    "security": [bearer],
                    "responses": {"200": ok_json, "413": ok_json}
                }
            },
            "/v1/db/{db}/channels/{channel}/subscribe": {
                "get": {
                    "summary": "Subscribe to a channel's ephemeral events and presence (SSE)",
                    "security": [bearer],
                    "responses": {"200": { "content": { "text/event-stream": {} } }}
                }
            },
            "/v1/db/{db}/channels/{channel}/presence": {
                "get": {
                    "summary": "Current presence roster for a channel",
                    "security": [bearer],
                    "responses": {"200": ok_json}
                }
            }
        }
    })
}
