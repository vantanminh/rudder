//! Search and read the built-in user guides. Open to any signed-in user or agent grant that can
//! read `system`, so an AI agent can look up how Rudder works on the panel it is connected to.

use axum::extract::{Path, Query};
use axum::routing::get;
use axum::Json;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::auth::AuthUser;
use crate::docs;
use crate::error::ApiError;
use crate::state::AppState;

use super::catalog::{bad, not_found};

const DEFAULT_LIMIT: usize = 5;
const MAX_LIMIT: usize = 20;
const MAX_QUERY: usize = 200;

#[derive(Serialize, ToSchema)]
pub struct DocSummary {
    /// Pass this to the read call, for example `cli`.
    pub name: String,
    pub title: String,
}

#[derive(Serialize, ToSchema)]
pub struct DocHit {
    pub name: String,
    pub title: String,
    /// The heading the match is under.
    pub section: String,
    pub snippet: String,
    /// Higher is a better match.
    pub score: u32,
}

#[derive(Serialize, ToSchema)]
pub struct DocSearchResponse {
    pub query: String,
    /// Version of the panel the guides belong to.
    pub version: String,
    pub results: Vec<DocHit>,
}

#[derive(Serialize, ToSchema)]
pub struct DocPage {
    pub name: String,
    pub title: String,
    pub version: String,
    /// The whole guide as Markdown.
    pub content: String,
}

#[derive(Deserialize)]
pub struct SearchQuery {
    q: Option<String>,
    limit: Option<usize>,
}

pub fn router() -> axum::Router<AppState> {
    axum::Router::new()
        .route("/api/v1/docs", get(list))
        .route("/api/v1/docs/search", get(search))
        .route("/api/v1/docs/{name}", get(read))
}

async fn list(_auth: AuthUser) -> Json<Vec<DocSummary>> {
    Json(
        docs::list()
            .iter()
            .map(|guide| DocSummary {
                name: guide.name.clone(),
                title: guide.title.clone(),
            })
            .collect(),
    )
}

async fn search(
    _auth: AuthUser,
    Query(query): Query<SearchQuery>,
) -> Result<Json<DocSearchResponse>, ApiError> {
    let q = query.q.unwrap_or_default();
    let q = q.trim();
    if q.is_empty() {
        return Err(bad("invalid_query", "q must not be empty"));
    }
    if q.chars().count() > MAX_QUERY {
        return Err(bad("invalid_query", "q is too long"));
    }
    let limit = query.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    Ok(Json(DocSearchResponse {
        query: q.to_owned(),
        version: rudder_core::VERSION.to_owned(),
        results: docs::search(q, limit)
            .into_iter()
            .map(|hit| DocHit {
                name: hit.name,
                title: hit.title,
                section: hit.heading,
                snippet: hit.snippet,
                score: hit.score,
            })
            .collect(),
    }))
}

async fn read(_auth: AuthUser, Path(name): Path<String>) -> Result<Json<DocPage>, ApiError> {
    let guide = docs::get(&name).ok_or_else(|| not_found("guide not found"))?;
    Ok(Json(DocPage {
        name: guide.name.clone(),
        title: guide.title.clone(),
        version: rudder_core::VERSION.to_owned(),
        content: guide.content.clone(),
    }))
}
