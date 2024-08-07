use std::string::ToString;
use std::sync::Arc;

use cookie::{Cookie, SameSite};
use lazy_static::lazy_static;
use openidconnect::{core::CoreIdTokenClaims, AuthorizationCode, Nonce, PkceCodeVerifier};
use reqwest::header::{HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use urlencoding::decode;
use uuid::Uuid;
use warp::http::{header::AUTHORIZATION, Response};
use warp::{filters::path::FullPath, reply::WithHeader, Filter, Rejection, Reply};
use warp_sessions::{
    CookieOptions, MemoryStore, SameSiteCookieOption, SessionWithStore, WithSession,
};

use crate::oidc::{IdentityProvider, OidcToken};

use super::{AnyhowError, RejectReason};
use crate::api::{AuthRejectReason, AuthenticatedUser, ValidatesIdentity};

impl AuthRejectReason {
    fn into_rejection(self) -> Rejection {
        warp::reject::custom(self)
    }

    pub fn oidc_error(msg: &'static str) -> Rejection {
        AuthRejectReason::OidcError { msg }.into_rejection()
    }

    pub fn csrf_mismatch() -> Rejection {
        AuthRejectReason::CsrfMismatch.into_rejection()
    }

    pub fn token_transfer_failed(msg: String) -> Rejection {
        AuthRejectReason::TokenTransferFailed { msg }.into_rejection()
    }

    pub fn invalid_credentials() -> Rejection {
        AuthRejectReason::InvalidCredentials.into_rejection()
    }

    pub fn invalid_session_token(reason: String) -> Rejection {
        AuthRejectReason::InvalidSessionToken { reason }.into_rejection()
    }

    pub fn no_session_token() -> Rejection {
        AuthRejectReason::NoSessionToken.into_rejection()
    }
}

pub const AUTH_COOKIE: &str = "access_token";

#[derive(Serialize, Deserialize)]
pub struct RedirectQuery {
    origin: Option<String>,
}

async fn login_handler(
    query: RedirectQuery,
    mut session: SessionWithStore<MemoryStore>,
    idp: Arc<IdentityProvider>,
) -> Result<(impl Reply, SessionWithStore<MemoryStore>), Rejection> {
    let (auth_url, csrf_token, verifier, nonce) = idp.login_oidc(vec![String::from("email")]);

    session
        .session
        .insert("csrf_token", csrf_token.secret().clone())
        .map_err(|_| RejectReason::Session)?;
    session
        .session
        .insert("pkce_verifier", verifier.secret().clone())
        .map_err(|_| RejectReason::Session)?;
    session
        .session
        .insert("nonce", nonce.secret().clone())
        .map_err(|_| RejectReason::Session)?;
    if let Some(redirect_uri) = query.origin {
        session
            .session
            .insert("redirect_uri", redirect_uri)
            .map_err(|_| RejectReason::Session)?;
    }

    Ok((redirect(auth_url)?, session))
}

fn redirect<U: Into<String>>(url: U) -> Result<Response<hyper_warp::Body>, Rejection> {
    let uri: warp::http::Uri = url
        .into()
        .try_into()
        .map_err(|err| RejectReason::BadRequest {
            reason: format!("Invalid URL: {}", err),
        })?;
    let mut no_cache_headers = HeaderMap::new();
    no_cache_headers.append(
        "Cache-Control",
        HeaderValue::from_str("no-store, must-revalidate").expect("Invalid header value"),
    );
    no_cache_headers.append(
        "Expires",
        HeaderValue::from_str("0").expect("Invalid header value"),
    );

    let reply = warp::redirect(uri);
    let mut response = reply.into_response();
    let headers = response.headers_mut();
    headers.extend(no_cache_headers);
    Ok(response)
}

#[derive(Serialize, Deserialize)]
struct AuthQuery {
    code: String,
    state: String,
}

async fn auth_handler(
    query: AuthQuery,
    mut session: SessionWithStore<MemoryStore>,
    idp: Arc<IdentityProvider>,
) -> Result<(impl Reply, SessionWithStore<MemoryStore>), Rejection> {
    let AuthQuery { code, state } = query;
    let code = AuthorizationCode::new(code);

    let csrf_token = match session.session.get::<String>("csrf_token") {
        Some(csrf_token) => csrf_token,
        None => {
            tracing::warn!("Missing csrf token");
            return Ok((redirect("auth/login")?, session));
        }
    };

    let verifier = match session.session.get::<String>("pkce_verifier") {
        Some(pkce_verifier) => PkceCodeVerifier::new(pkce_verifier),
        None => {
            tracing::warn!("Missing PKCE verifier");
            return Ok((redirect("auth/login")?, session));
        }
    };

    let nonce = match session.session.get::<String>("nonce") {
        Some(nonce) => Nonce::new(nonce),
        None => {
            tracing::warn!("Missing nonce");
            return Ok((redirect("auth/login")?, session));
        }
    };

    let redirect_uri = match session.session.get::<String>("redirect_uri") {
        Some(redirect_uri) => decode(&redirect_uri)
            .map(|s| s.to_owned().to_string())
            .unwrap_or_else(|_| String::from("/")),
        None => String::from("/"),
    };

    if state != csrf_token {
        tracing::warn!("CSRF token mismatch! This is a possible attack!");
        return Ok((redirect("auth/login")?, session));
    }

    let token = match idp.token_oidc(code, verifier, nonce).await {
        Ok(token) => token,
        Err(err) => return Err(AuthRejectReason::token_transfer_failed(err.to_string())),
    };

    session.session.insert("token", token).ok();

    let redirect = format!(
        "<html><head><meta http-equiv=\"refresh\" content=\"0; URL='{}'\"/></head></html>",
        redirect_uri
    );
    Ok((warp::reply::html(redirect).into_response(), session))
}

fn parse_auth_cookie(cookie_str: &str) -> Result<OidcToken, Rejection> {
    serde_json::from_str(cookie_str)
        .map_err(|err| AuthRejectReason::invalid_session_token(format!("cookie: {}", err)))
}

pub async fn store_auth_cookie<T: Reply>(
    reply: T,
    session: SessionWithStore<MemoryStore>,
) -> Result<WithSession<WithHeader<T>>, Rejection> {
    if !session.session.data_changed() {
        // Set this random header because there is a type problem otherwise
        let reply = warp::reply::with_header(reply, "Server", "Subseq");
        return WithSession::new(reply, session).await;
    }

    let token_serialized = match session.session.get_raw("token") {
        Some(token) => token,
        None => {
            // Set this random header because there is a type problem otherwise
            let reply = warp::reply::with_header(reply, "Server", "Subseq");
            return WithSession::new(reply, session).await;
        }
    };

    let cookie = Cookie::build((AUTH_COOKIE, token_serialized.as_str()))
        .path("/")
        .http_only(true)
        .same_site(SameSite::Lax)
        .secure(true)
        .build();

    let cookie_content = cookie.to_string();
    let reply = warp::reply::with_header(reply, "Set-Cookie", cookie_content);
    tracing::trace!("Cookie set");
    WithSession::new(reply, session).await
}

lazy_static! {
    static ref COOKIE_OPTS: CookieOptions = CookieOptions {
        cookie_name: "sid",
        path: Some("/".to_string()),
        http_only: true,
        same_site: Some(SameSiteCookieOption::Lax),
        secure: true,
        ..Default::default()
    };
}

impl ValidatesIdentity for Arc<IdentityProvider> {
    fn validate_token(&self, token: &OidcToken) -> anyhow::Result<CoreIdTokenClaims> {
        IdentityProvider::validate_token(self, token)
    }

    async fn refresh_token(&self, token: OidcToken) -> anyhow::Result<OidcToken> {
        self.refresh(token).await
    }
}

pub fn authenticate(
    idp: Option<Arc<IdentityProvider>>,
    session: MemoryStore,
) -> impl Filter<Extract = (AuthenticatedUser, SessionWithStore<MemoryStore>), Error = Rejection> + Clone
{
    warp::any()
        .and(warp::cookie::optional::<String>(AUTH_COOKIE))
        .and(warp::header::optional::<String>(AUTHORIZATION.as_str()))
        .and(warp::path::full())
        .and(warp_sessions::request::with_session(
            session.clone(),
            Some(COOKIE_OPTS.clone()),
        ))
        .and_then(
            move |token: Option<String>,
                  bearer: Option<String>,
                  path: FullPath,
                  mut session: SessionWithStore<MemoryStore>| {
                let idp = idp.clone();
                async move {
                    if let Some(idp) = idp {
                        // Prefer the bearer token
                        let token = match bearer {
                            Some(tok) if tok.starts_with("Bearer ") => {
                                let content = tok.trim_start_matches("Bearer ");
                                OidcToken::from_bearer(content)
                            }
                            _ => match token {
                                Some(tok) => Some(parse_auth_cookie(&tok)?),
                                None => None,
                            },
                        };

                        match token {
                            Some(token) => {
                                let (auth_user, token) =
                                    AuthenticatedUser::validate_session(&idp, token)
                                        .await
                                        .map_err(AnyhowError::from)?;
                                if let Some(token) = token {
                                    tracing::trace!("Reset token");
                                    let inner_session = &mut session.session;
                                    inner_session.insert("token", token).ok();
                                }
                                Ok((auth_user, session))
                            }
                            None => {
                                let inner_session = &mut session.session;
                                inner_session
                                    .insert("redirect_path", path.as_str().to_string())
                                    .ok();
                                Err(AuthRejectReason::no_session_token())
                            }
                        }
                    } else if let Some(token) = token {
                        let NoAuthToken { user_id } =
                            serde_json::from_str(&token).map_err(|err| {
                                AuthRejectReason::invalid_session_token(format!("cookie: {}", err))
                            })?;
                        Ok((
                            AuthenticatedUser {
                                id: user_id,
                                username: "FAKE_NAME".to_string(),
                                email: "FAKE_EMAIL".to_string(),
                                email_verified: false,
                                given_name: None,
                                family_name: None,
                            },
                            session,
                        ))
                    } else {
                        Err(AuthRejectReason::no_session_token())
                    }
                }
            },
        )
        .untuple_one()
}

pub fn with_idp(
    idp: Arc<IdentityProvider>,
) -> impl Filter<Extract = (Arc<IdentityProvider>,), Error = std::convert::Infallible> + Clone {
    warp::any().map(move || idp.clone())
}

async fn no_auth_login_handler() -> Result<impl Reply, Rejection> {
    let login_form = r#"
        <html>
            <body>
                <form action="/auth" method="post">
                    <label for="user_id">User ID</label>
                    <input type="text" id="user_id" name="user_id" required minlength="36" size="36" />
                    <input type="submit" value="Submit" />
                </form>
            </body>
        </html>
    "#;
    Ok(warp::reply::html(login_form))
}

#[derive(Deserialize)]
struct FormData {
    user_id: String,
}

#[derive(Deserialize, Serialize)]
struct NoAuthToken {
    user_id: Uuid,
}

async fn no_auth_form_handler(
    mut session: SessionWithStore<MemoryStore>,
    form: FormData,
) -> Result<(impl Reply, SessionWithStore<MemoryStore>), Rejection> {
    let user_id =
        Uuid::parse_str(&form.user_id).map_err(|_| AuthRejectReason::invalid_credentials())?;
    let token = NoAuthToken { user_id };
    session.session.insert("token", token).ok();

    let original_path = String::from("/");
    let redirect = format!(
        "<html><head><meta http-equiv=\"refresh\" content=\"0; URL='{}'\"/></head></html>",
        original_path
    );
    Ok((warp::reply::html(redirect), session))
}

pub fn provider_routes(
    session: MemoryStore,
) -> impl Filter<Extract = impl Reply, Error = Rejection> + Clone {
    let logout = warp::path("logout")
        .and(warp_sessions::request::with_session(
            session.clone(),
            Some(COOKIE_OPTS.clone()),
        ))
        .map(|mut session: SessionWithStore<MemoryStore>| {
            session.session.destroy();
            let cookie = format!("{}=; Max-Age=0; Path=/; HttpOnly; Secure", AUTH_COOKIE);
            let reply = Response::builder()
                .header("Set-Cookie", cookie)
                .body("")
                .expect("Failed to build response");

            (reply, session)
        })
        .untuple_one()
        .and_then(warp_sessions::reply::with_session);

    warp::path("oauth").and(logout)
}

async fn logout_handler(
    idp: Arc<IdentityProvider>,
    session: SessionWithStore<MemoryStore>,
    token: String,
) -> Result<(impl Reply, SessionWithStore<MemoryStore>), Rejection> {
    let token = parse_auth_cookie(&token)
        .map_err(|err| AuthRejectReason::invalid_session_token(format!("{:?}", err)))?;
    let logout_url = idp.logout_oidc("/", &token);
    let uri = logout_url.as_str().parse::<warp::http::Uri>().unwrap();

    let reply = warp::redirect(uri);
    let mut response = reply.into_response();

    {
        let headers = response.headers_mut();
        let mut reply_headers = HeaderMap::new();
        reply_headers.append(
            "Cache-Control",
            HeaderValue::from_str("no-store, must-revalidate").expect("Invalid header value"),
        );
        reply_headers.append(
            "Expires",
            HeaderValue::from_str("0").expect("Invalid header value"),
        );
        let cookie = format!("{}=; Max-Age=0; Path=/; HttpOnly; Secure", AUTH_COOKIE);
        reply_headers.append(
            "Set-Cookie",
            HeaderValue::from_str(&cookie).expect("Invalid header value"),
        );
        headers.extend(reply_headers);
    }

    Ok((response, session))
}

pub fn routes(
    session: MemoryStore,
    idp: Arc<IdentityProvider>,
) -> impl Filter<Extract = impl Reply, Error = Rejection> + Clone {
    let login = warp::get()
        .and(warp::path("login"))
        .and(warp::path::end())
        .and(warp::query::<RedirectQuery>())
        .and(warp_sessions::request::with_session(
            session.clone(),
            Some(COOKIE_OPTS.clone()),
        ))
        .and(with_idp(idp.clone()))
        .and_then(login_handler)
        .untuple_one()
        .and_then(warp_sessions::reply::with_session);

    let auth = warp::path::end()
        .and(warp::get())
        .and(warp::query::<AuthQuery>())
        .and(warp_sessions::request::with_session(
            session.clone(),
            Some(COOKIE_OPTS.clone()),
        ))
        .and(with_idp(idp.clone()))
        .and_then(auth_handler)
        .untuple_one()
        .and_then(store_auth_cookie);

    let logout = warp::get()
        .and(warp::path("logout"))
        .and(warp::path::end())
        .and(with_idp(idp.clone()))
        .and(warp_sessions::request::with_session(
            session.clone(),
            Some(COOKIE_OPTS.clone()),
        ))
        .and(warp::cookie::cookie::<String>(AUTH_COOKIE))
        .and_then(logout_handler)
        .untuple_one()
        .and_then(warp_sessions::reply::with_session);

    warp::path("auth").and(login.or(auth).or(logout))
}

pub fn no_auth_routes(
    session: MemoryStore,
) -> impl Filter<Extract = impl Reply, Error = Rejection> + Clone {
    let login = warp::get()
        .and(warp::path("login"))
        .and(warp::path::end())
        .and_then(no_auth_login_handler);

    let auth = warp::path::end()
        .and(warp::post())
        .and(warp_sessions::request::with_session(
            session.clone(),
            Some(COOKIE_OPTS.clone()),
        ))
        .and(warp::body::form())
        .and_then(no_auth_form_handler)
        .untuple_one()
        .and_then(store_auth_cookie);
    warp::path("auth").and(login.or(auth))
}
