pub mod email;
pub mod sessions;

use std::convert::Infallible;
use std::string::ToString;
use std::sync::Arc;

use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::json;
use tokio::sync::broadcast;
use warp::{http::StatusCode, Filter, Rejection, Reply};
use warp_sessions::MemoryStore;

use super::{AnyhowError, AuthRejectReason, RejectReason};
use crate::tables::DbPool;

pub fn init_session_store() -> MemoryStore {
    MemoryStore::new()
}

impl warp::reject::Reject for AnyhowError {}
impl warp::reject::Reject for RejectReason {}

impl RejectReason {
    pub fn into_rejection(self) -> Rejection {
        warp::reject::custom(self)
    }
}

pub fn with_db(
    pool: Arc<DbPool>,
) -> impl Filter<Extract = (Arc<DbPool>,), Error = std::convert::Infallible> + Clone {
    warp::any().map(move || pool.clone())
}

pub fn with_string<S: Send + Sync + Clone + ToString>(
    string: S,
) -> impl Filter<Extract = (String,), Error = std::convert::Infallible> + Clone {
    warp::any().map(move || string.to_string())
}

pub fn with_broadcast<M: Send + Sync + Clone + 'static>(
    sender: broadcast::Sender<M>,
) -> impl Filter<Extract = (broadcast::Sender<M>,), Error = Infallible> + Clone {
    warp::any().map(move || sender.clone())
}

pub async fn handle_rejection(
    err: warp::reject::Rejection,
) -> Result<Box<dyn warp::Reply>, std::convert::Infallible> {
    if err.is_not_found() {
        return Ok(Box::new(warp::reply::with_status(
            "NOT_FOUND",
            StatusCode::NOT_FOUND,
        )));
    }

    if let Some(auth_err) = err.find::<AuthRejectReason>() {
        match auth_err {
            AuthRejectReason::NoSessionToken => {
                let auth_path = warp::http::Uri::try_from("/auth/login").expect("uri failed");
                let mut no_cache_headers = HeaderMap::new();
                no_cache_headers.append(
                    "Cache-Control",
                    HeaderValue::from_str("no-store, must-revalidate")
                        .expect("Invalid header value"),
                );
                no_cache_headers.append(
                    "Expires",
                    HeaderValue::from_str("0").expect("Invalid header value"),
                );

                let reply = warp::redirect(auth_path);
                let mut response = reply.into_response();
                let headers = response.headers_mut();
                headers.extend(no_cache_headers);

                return Ok(Box::new(response));
            }
            AuthRejectReason::InvalidSessionToken { reason } => {
                tracing::error!("InvalidSessionToken: {}", reason);
                let json = warp::reply::json(&"Unauthorized");
                let response = warp::reply::with_status(json, warp::http::StatusCode::UNAUTHORIZED);
                return Ok(Box::new(response));
            }
            AuthRejectReason::OidcError { msg } => {
                tracing::error!("OidcError: {}", msg);
                let json = warp::reply::json(&"OIDC Configuration Error");
                let response =
                    warp::reply::with_status(json, warp::http::StatusCode::INTERNAL_SERVER_ERROR);
                return Ok(Box::new(response));
            }
            AuthRejectReason::CsrfMismatch => {
                tracing::error!("CSRF Mismatch!");
                let json = warp::reply::json(&"OIDC Configuration Error");
                let response = warp::reply::with_status(json, warp::http::StatusCode::FORBIDDEN);
                return Ok(Box::new(response));
            }
            AuthRejectReason::TokenTransferFailed { msg } => {
                tracing::error!("IdP is in down or degraded state! {}", msg);
                let json = warp::reply::json(&"Error communicating with identity provider");
                let response = warp::reply::with_status(json, warp::http::StatusCode::BAD_GATEWAY);
                return Ok(Box::new(response));
            }
            AuthRejectReason::InvalidCredentials => {
                let json = warp::reply::json(&"Invalid form of authorization");
                let response = warp::reply::with_status(json, warp::http::StatusCode::FORBIDDEN);
                return Ok(Box::new(response));
            }
        }
    }

    if let Some(anyhow_err) = err.find::<AnyhowError>() {
        tracing::error!("{:?}", anyhow_err.error);
        let json = warp::reply::json(&json!({"error": anyhow_err.error.to_string()}));
        let response =
            warp::reply::with_status(json, warp::http::StatusCode::INTERNAL_SERVER_ERROR);
        return Ok(Box::new(response));
    }

    if let Some(err) = err.find::<RejectReason>() {
        match err {
            RejectReason::Anyhow { error: anyhow_err } => {
                tracing::error!("{:?}", anyhow_err.error);
                let json = warp::reply::json(&json!({"error": anyhow_err.error.to_string()}));
                let response =
                    warp::reply::with_status(json, warp::http::StatusCode::INTERNAL_SERVER_ERROR);
                return Ok(Box::new(response));
            }
            RejectReason::BadRequest { reason } => {
                let json = warp::reply::json(&json!({"rejected": reason}));
                let response = warp::reply::with_status(json, warp::http::StatusCode::BAD_REQUEST);
                return Ok(Box::new(response));
            }
            RejectReason::Conflict { resource } => {
                let json = warp::reply::json(&json!({"conflict": resource}));
                let response = warp::reply::with_status(json, warp::http::StatusCode::CONFLICT);
                return Ok(Box::new(response));
            }
            RejectReason::DatabaseError { msg } => {
                tracing::error!("Database error: {}", msg);
                let json = warp::reply::json(&json!({"rejected": msg}));
                let response =
                    warp::reply::with_status(json, warp::http::StatusCode::INTERNAL_SERVER_ERROR);
                return Ok(Box::new(response));
            }
            RejectReason::Forbidden { user_id, reason } => {
                tracing::error!("Forbidden {}: {}", user_id, reason);
                let json = warp::reply::json(&json!({"rejected": "forbidden"}));
                let response = warp::reply::with_status(json, warp::http::StatusCode::FORBIDDEN);
                return Ok(Box::new(response));
            }
            RejectReason::NotFound { resource } => {
                let json = warp::reply::json(&json!({"missing": resource}));
                let response = warp::reply::with_status(json, warp::http::StatusCode::NOT_FOUND);
                return Ok(Box::new(response));
            }
            RejectReason::MissingEnvKey { key } => {
                tracing::error!("Missing Environment Key: {}", key);
                let json = warp::reply::json(&json!({"error": "Server misconfiguration error"}));
                let response =
                    warp::reply::with_status(json, warp::http::StatusCode::INTERNAL_SERVER_ERROR);
                return Ok(Box::new(response));
            }
            RejectReason::Session => {
                tracing::error!("Session error");
                let json = warp::reply::json(&json!({"error": "Server misconfiguration error"}));
                let response =
                    warp::reply::with_status(json, warp::http::StatusCode::INTERNAL_SERVER_ERROR);
                return Ok(Box::new(response));
            }
        }
    }

    tracing::error!("Unhandled Error: {:?}", err);
    let json = warp::reply::json(&"Unhandled error");
    Ok(Box::new(warp::reply::with_status(
        json,
        warp::http::StatusCode::INTERNAL_SERVER_ERROR,
    )))
}
