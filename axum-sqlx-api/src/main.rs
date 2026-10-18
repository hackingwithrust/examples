use axum::{
    Json, Router,
    extract::{Extension, FromRequest, Path, Query, Request},
    http::{StatusCode, header, HeaderMap, HeaderValue},
    response::{IntoResponse, Response},
    routing::get,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::json;
use sqlx::{PgPool, postgres::PgPoolOptions};
use std::{
    convert::Infallible,
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use thiserror::Error;
use tower::{Layer, Service};
use validator::{Validate, ValidationErrors};

const ALPHABET: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

// 62^7 is about 3.5 trillion codes. A full u64 would need up to 11
// characters; seven is plenty and keeps every code the same length.
const CODE_SPACE: u64 = 62u64.pow(7);

fn to_base62(mut n: u64) -> String {
    let mut digits = [0u8; 11];
    let mut len = 0;
    loop {
        digits[len] = ALPHABET[(n % 62) as usize];
        len += 1;
        n /= 62;
        if n == 0 {
            break;
        }
    }
    digits[..len].iter().rev().map(|&b| b as char).collect()
}

fn next_code(state: &mut u64) -> String {
    // xorshift64*: three shifts and a multiply turn the previous number into
    // the next one. 0 is a fixed point, so the seed must never be 0 - the
    // clock in create_link cannot produce 0, and neither can this sequence.
    *state ^= *state >> 12;
    *state ^= *state << 25;
    *state ^= *state >> 27;
    let n = state.wrapping_mul(0x2545_F491_4F6C_DD1D);
    // A small n encodes to fewer than seven digits. '0' is base62's zero,
    // so left-padding does not change the value, and every code comes out
    // the same length.
    format!("{:0>7}", to_base62(n % CODE_SPACE))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_the_boundaries() {
        assert_eq!(to_base62(0), "0");
        assert_eq!(to_base62(61), "z");
        assert_eq!(to_base62(62), "10");
        assert_eq!(to_base62(CODE_SPACE - 1), "zzzzzzz");
    }

    #[test]
    fn codes_are_seven_characters_from_the_alphabet() {
        let mut state = 42;
        for _ in 0..1_000 {
            let code = next_code(&mut state);
            assert_eq!(code.len(), 7);
            assert!(code.bytes().all(|b| ALPHABET.contains(&b)));
        }
    }

    #[test]
    fn the_sequence_keeps_moving() {
        let mut state = 42;
        let first = next_code(&mut state);
        let second = next_code(&mut state);
        assert_ne!(first, second);
    }
}

#[derive(Deserialize)]
struct Pagination {
    page: Option<usize>,
    limit: Option<usize>,
}

#[derive(Deserialize, Validate)]
struct CreateLink {
    #[validate(url(message = "must be a valid URL"))]
    url: String,
}

#[derive(Serialize)]
struct Link {
    code: String,
    url: String,
}

#[derive(Serialize)]
struct LinkPage {
    page: usize,
    limit: usize,
    links: Vec<Link>,
}

struct ApiResponse<T> {
    status: StatusCode,
    data: T,
}

impl<T: Serialize> ApiResponse<T> {
    fn ok(data: T) -> Self {
        Self {
            status: StatusCode::OK,
            data,
        }
    }

    fn with_status(status: StatusCode, data: T) -> Self {
        Self { status, data }
    }
}

impl<T: Serialize> IntoResponse for ApiResponse<T> {
    fn into_response(self) -> Response {
        (self.status, Json(self.data)).into_response()
    }
}

#[derive(Debug, Error)]
enum ApiError {
    // Extractor rejections carry their own status: 400 for malformed JSON,
    // 415 for a missing or wrong content-type.
    #[error("{1}")]
    BadBody(StatusCode, String),
    #[error("{0}")]
    Validation(#[from] ValidationErrors),
    #[error("{0}")]
    Unauthorized(&'static str),
    #[error("not found")]
    NotFound,
    // The sqlx error is kept as the source for logging (step 13) but never
    // serialised - its message names tables and columns a client has no
    // business reading.
    #[error("internal server error")]
    Database(#[from] sqlx::Error),
    #[error("internal server error")]
    Exhausted,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match &self {
            ApiError::BadBody(status, _) => *status,
            ApiError::Validation(_) => StatusCode::UNPROCESSABLE_ENTITY,
            ApiError::Unauthorized(_) => StatusCode::UNAUTHORIZED,
            ApiError::NotFound => StatusCode::NOT_FOUND,
            ApiError::Database(_) | ApiError::Exhausted => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, Json(json!({ "error": self.to_string() }))).into_response()
    }
}

struct ValidatedJson<T>(T);

impl<S, T> FromRequest<S> for ValidatedJson<T>
where
    T: DeserializeOwned + Validate,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let Json(value) = Json::<T>::from_request(req, state)
            .await
            .map_err(|rejection| ApiError::BadBody(rejection.status(), rejection.body_text()))?;
        value.validate()?;
        Ok(ValidatedJson(value))
    }
}

// The identity the layer proved. Handlers never re-check the header, and the
// rate limiter in the next step reads it from the request extensions.
#[derive(Clone)]
struct ApiUser {
    key: Arc<str>,
}

// Wraps the routes only an authenticated client may touch. A tower layer
// sits between the router and the handler: it sees every request first and
// can answer it without the handler ever running.
#[derive(Clone)]
struct RequireApiKey {
    key: Arc<str>,
}

impl RequireApiKey {
    fn new(key: impl Into<Arc<str>>) -> Self {
        Self { key: key.into() }
    }
}

impl<S> Layer<S> for RequireApiKey {
    type Service = RequireApiKeyService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        RequireApiKeyService {
            inner,
            key: self.key.clone(),
        }
    }
}

#[derive(Clone)]
struct RequireApiKeyService<S> {
    inner: S,
    key: Arc<str>,
}

impl<S> Service<Request> for RequireApiKeyService<S>
where
    S: Service<Request, Response = Response, Error = Infallible> + Send + 'static,
    S::Future: Send + 'static,
{
    type Response = Response;
    // axum requires every service in the stack to be infallible: the router
    // has no Err branch to handle. A failed check therefore becomes a
    // Response here - which ApiError's IntoResponse already knows how to
    // build.
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Response, Infallible>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request) -> Self::Future {
        match check_key(req.headers(), &self.key) {
            Err(err) => {
                let mut res = err.into_response();
                // RFC 9110: a 401 tells the client how to authenticate. This
                // is the only place the header can be added - handlers never
                // see a rejected request.
                res.headers_mut().insert(
                    header::WWW_AUTHENTICATE,
                    HeaderValue::from_static("Bearer"),
                );
                Box::pin(async move { Ok(res) })
            }
            Ok(()) => {
                let mut req = req;
                req.extensions_mut().insert(ApiUser {
                    key: self.key.clone(),
                });
                Box::pin(self.inner.call(req))
            }
        }
    }
}

// The scheme is case-insensitive per RFC 9110, so "bearer" passes too.
fn check_key(headers: &HeaderMap, key: &str) -> Result<(), ApiError> {
    let value = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .ok_or(ApiError::Unauthorized("missing API key"))?;
    let token = value
        .split_once(' ')
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
        .map(|(_, token)| token)
        .ok_or(ApiError::Unauthorized("expected a Bearer token"))?;
    if token != key {
        return Err(ApiError::Unauthorized("invalid API key"));
    }
    Ok(())
}

async fn health(Extension(pool): Extension<PgPool>) -> Result<&'static str, StatusCode> {
    // query! macros read the schema through DATABASE_URL at compile time.
    match sqlx::query_scalar!("SELECT 1").fetch_one(&pool).await {
        Ok(_) => Ok("ok"),
        Err(_) => Err(StatusCode::SERVICE_UNAVAILABLE),
    }
}

async fn create_link(
    Extension(pool): Extension<PgPool>,
    ValidatedJson(input): ValidatedJson<CreateLink>,
) -> Result<ApiResponse<Link>, ApiError> {
    // The seed comes from the clock; every call to next_code advances the
    // state, so a retried insert draws a different code.
    let mut state = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock is before 1970")
        .as_nanos() as u64;

    for _ in 0..5 {
        let code = next_code(&mut state);
        match sqlx::query!(
            "INSERT INTO links (code, url) VALUES ($1, $2) RETURNING id, code, url",
            code,
            input.url
        )
        .fetch_one(&pool)
        .await
        {
            Ok(row) => {
                return Ok(ApiResponse::with_status(
                    StatusCode::CREATED,
                    Link {
                        code: row.code,
                        url: row.url,
                    },
                ));
            }
            Err(err) => {
                // 23505: the unique index rejected the code. Any other
                // failure is a real error that a retry cannot fix.
                let retryable = err
                    .as_database_error()
                    .is_some_and(|db| matches!(db.kind(), sqlx::error::ErrorKind::UniqueViolation));
                if !retryable {
                    return Err(err.into());
                }
            }
        }
    }

    // Five collisions in a row out of 3.5 trillion codes is not bad luck,
    // it is a bug - but a 500 beats an infinite loop.
    Err(ApiError::Exhausted)
}

async fn list_links(
    Extension(pool): Extension<PgPool>,
    Query(p): Query<Pagination>,
) -> Result<ApiResponse<LinkPage>, ApiError> {
    let page = p.page.unwrap_or(1).max(1) as i64;
    let limit = p.limit.unwrap_or(20) as i64;

    let rows = sqlx::query!(
        "SELECT code, url FROM links ORDER BY id LIMIT $1 OFFSET $2",
        limit,
        (page - 1) * limit
    )
    .fetch_all(&pool)
    .await?;

    Ok(ApiResponse::ok(LinkPage {
        page: page as usize,
        limit: limit as usize,
        links: rows
            .into_iter()
            .map(|row| Link {
                code: row.code,
                url: row.url,
            })
            .collect(),
    }))
}

async fn resolve_link(
    Path(code): Path<String>,
    Extension(pool): Extension<PgPool>,
) -> Result<Response, ApiError> {
    let row = sqlx::query!("SELECT url FROM links WHERE code = $1", code)
        .fetch_optional(&pool)
        .await?;

    match row {
        Some(row) => Ok((
            StatusCode::FOUND,
            [("Location", row.url.as_str())],
        )
            .into_response()),
        None => Err(ApiError::NotFound),
    }
}

#[tokio::main]
async fn main() {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL is not set");
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .acquire_timeout(std::time::Duration::from_secs(3))
        .connect(&url)
        .await
        .expect("failed to connect to DATABASE_URL");

    sqlx::migrate!().run(&pool).await.expect("failed to run migrations");

    // The key lives in the environment, not in the source. The service
    // refuses to start without one - a missing key means every /links
    // request is rejected, which is better discovered at boot.
    let key = std::env::var("SHORTLINK_API_KEY").expect("SHORTLINK_API_KEY is not set");

    // Two routers, because the routes have different audiences: browsers
    // hitting /{code} send no headers, and a load balancer polling /health
    // has none to offer. Only /links - the routes that change or read data -
    // sit behind the layer. route_layer, not layer: it runs only once a
    // route has matched, so a typo like /foo/bar stays a 404 instead of
    // being told to authenticate against a path that does not exist.
    let protected = Router::new()
        .route("/links", get(list_links).post(create_link))
        .route_layer(RequireApiKey::new(key));

    let app = Router::new()
        .route("/health", get(health))
        .route("/{code}", get(resolve_link))
        .merge(protected)
        .layer(Extension(pool));

    let listener = tokio::net::TcpListener::bind("0.0.0.0:8000").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
