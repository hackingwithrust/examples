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
    collections::HashMap,
    convert::Infallible,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::Instant,
};
use thiserror::Error;
use tower::{Layer, Service};
use tower_http::{
    request_id::{MakeRequestUuid, PropagateRequestIdLayer, RequestId, SetRequestIdLayer},
    trace::{DefaultOnResponse, MakeSpan, TraceLayer},
};
use tracing::Level;
use tracing_subscriber::EnvFilter;
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
    use std::time::Duration;

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

    #[test]
    fn a_fresh_bucket_bursts_to_capacity_then_rejects() {
        let limiter = Limiter::new(3.0, 1.0);
        let key: Arc<str> = "dev-key".into();
        let start = Instant::now();
        for _ in 0..3 {
            assert!(limiter.check(key.clone(), start).is_ok());
        }
        // 0.7 seconds until the next token, rounded up to a whole second.
        assert_eq!(limiter.check(key, start), Err(1));
    }

    #[test]
    fn elapsed_time_refills_the_bucket() {
        let limiter = Limiter::new(3.0, 1.0);
        let key: Arc<str> = "dev-key".into();
        let start = Instant::now();
        for _ in 0..3 {
            limiter.check(key.clone(), start).unwrap();
        }
        // Half a second back is half a token - not enough to admit.
        let half = start + Duration::from_millis(500);
        assert!(limiter.check(key.clone(), half).is_err());
        // Half a second more completes the token.
        let full = half + Duration::from_millis(500);
        assert!(limiter.check(key, full).is_ok());
    }

    #[test]
    fn keys_do_not_share_a_bucket() {
        let limiter = Limiter::new(2.0, 1.0);
        let alpha: Arc<str> = "alpha".into();
        let beta: Arc<str> = "beta".into();
        let start = Instant::now();
        for _ in 0..2 {
            limiter.check(alpha.clone(), start).unwrap();
        }
        assert!(limiter.check(alpha, start).is_err());
        assert!(limiter.check(beta, start).is_ok());
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
    #[error("rate limit exceeded")]
    TooManyRequests(u64),
    // The rate limiter reads the identity the auth layer left behind. This
    // variant means it found none - the two layers were wired in the wrong
    // order. That is a deployment bug, not a client's fault, so it maps to
    // 500 like the other internal errors.
    #[error("internal server error")]
    Miswired,
    // The sqlx error is kept as the source so into_response can log it -
    // the response body still only says "internal server error".
    #[error("internal server error")]
    Database(#[from] sqlx::Error),
    #[error("internal server error")]
    Exhausted,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        // Every rejected request passes through here once. The lines below
        // run inside the request's span, so each one carries the request id
        // - a 500 in the log can be traced to the exact request. 5xx errors
        // log their cause; a 429 is worth a warning; 401 and 404 are the
        // client's story, already told by the response line.
        match &self {
            ApiError::Database(source) => {
                tracing::error!(error = ?source, "database error");
            }
            ApiError::Miswired => {
                tracing::error!("rate limiter found no API user; the layer order is wrong");
            }
            ApiError::Exhausted => {
                tracing::error!("five code collisions in a row; refusing to retry again");
            }
            ApiError::TooManyRequests(_) => {
                tracing::warn!("rate limit exceeded");
            }
            _ => {}
        }
        let status = match &self {
            ApiError::BadBody(status, _) => *status,
            ApiError::Validation(_) => StatusCode::UNPROCESSABLE_ENTITY,
            ApiError::Unauthorized(_) => StatusCode::UNAUTHORIZED,
            ApiError::NotFound => StatusCode::NOT_FOUND,
            ApiError::TooManyRequests(_) => StatusCode::TOO_MANY_REQUESTS,
            ApiError::Miswired | ApiError::Database(_) | ApiError::Exhausted => {
                StatusCode::INTERNAL_SERVER_ERROR
            }
        };
        let mut res = (status, Json(json!({ "error": self.to_string() }))).into_response();
        if let ApiError::TooManyRequests(retry) = &self {
            // RFC 9110: Retry-After tells the client when to try again. The
            // bucket refills continuously, so this is a whole-second
            // estimate, rounded up.
            res.headers_mut().insert(
                header::RETRY_AFTER,
                HeaderValue::from_str(&retry.to_string()).expect("a u64 is a valid header value"),
            );
        }
        res
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

// The identity the auth layer proved. Handlers never re-check the header,
// and the rate limiter below keys its buckets on it.
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

// One bucket per key. `capacity` tokens are available immediately - that is
// the burst a client may spend at once - and `refill_per_sec` drips back in
// continuously. A client that waits between requests never sees the limit;
// one that hammers is cut to the refill rate once the burst is spent.
struct TokenBucket {
    tokens: f64,
    updated: Instant,
}

// Cloned services share one Limiter through the Arc, so every connection
// agrees on how many tokens a key has left.
struct Limiter {
    buckets: Mutex<HashMap<Arc<str>, TokenBucket>>,
    capacity: f64,
    refill_per_sec: f64,
}

impl Limiter {
    fn new(capacity: f64, refill_per_sec: f64) -> Self {
        Self {
            buckets: Mutex::new(HashMap::new()),
            capacity,
            refill_per_sec,
        }
    }

    // One token per request. Ok(()) admits it; Err(retry) is how many
    // seconds until a token is back, rounded up to a whole second because
    // Retry-After takes integer seconds.
    fn check(&self, key: Arc<str>, now: Instant) -> Result<(), u64> {
        let mut buckets = self.buckets.lock().expect("bucket map poisoned");
        let bucket = buckets.entry(key).or_insert(TokenBucket {
            tokens: self.capacity,
            updated: now,
        });
        let elapsed = now.duration_since(bucket.updated).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * self.refill_per_sec).min(self.capacity);
        bucket.updated = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            Ok(())
        } else {
            let secs = ((1.0 - bucket.tokens) / self.refill_per_sec).ceil();
            Err(secs.max(1.0) as u64)
        }
    }
}

#[derive(Clone)]
struct RateLimit {
    limiter: Arc<Limiter>,
}

impl RateLimit {
    fn new(capacity: f64, refill_per_sec: f64) -> Self {
        Self {
            limiter: Arc::new(Limiter::new(capacity, refill_per_sec)),
        }
    }
}

impl<S> Layer<S> for RateLimit {
    type Service = RateLimitService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        RateLimitService {
            inner,
            limiter: self.limiter.clone(),
        }
    }
}

#[derive(Clone)]
struct RateLimitService<S> {
    inner: S,
    limiter: Arc<Limiter>,
}

impl<S> Service<Request> for RateLimitService<S>
where
    S: Service<Request, Response = Response, Error = Infallible> + Send + 'static,
    S::Future: Send + 'static,
{
    type Response = Response;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Response, Infallible>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request) -> Self::Future {
        // The auth layer runs first and left the identity behind. Its
        // absence here means the layers were wired in the wrong order - the
        // limiter has nothing to key on and must not guess.
        let key = match req.extensions().get::<ApiUser>() {
            Some(user) => user.key.clone(),
            None => {
                let res = ApiError::Miswired.into_response();
                return Box::pin(async move { Ok(res) });
            }
        };
        match self.limiter.check(key, Instant::now()) {
            Err(retry) => {
                let res = ApiError::TooManyRequests(retry).into_response();
                Box::pin(async move { Ok(res) })
            }
            Ok(()) => Box::pin(self.inner.call(req)),
        }
    }
}

// The span every log line of a request lands in. tower-http calls this once
// the request reaches the trace layer, and the request-id layer sits outside
// it, so the id is already in the extensions here. One id, chosen at the
// edge, printed on every line this request produces.
#[derive(Clone)]
struct RequestSpan;

impl<B> MakeSpan<B> for RequestSpan {
    fn make_span(&mut self, request: &axum::http::Request<B>) -> tracing::Span {
        let request_id = request
            .extensions()
            .get::<RequestId>()
            .map(|id| id.header_value().to_str().unwrap_or("-"))
            .unwrap_or("-");
        tracing::info_span!(
            "http_request",
            request_id = %request_id,
            method = %request.method(),
            path = %request.uri().path(),
        )
    }
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

// The tests below talk to a real Postgres. #[sqlx::test] replaces
// #[tokio::test]: before the body runs, sqlx connects to the server
// DATABASE_URL points at, creates a database named for this one test,
// applies the migrations in migrations/ to it, and hands the body a pool
// connected to it. When the test ends the database is dropped. One test
// cannot see what another wrote, so each starts from the same empty schema
// with no setup and no cleanup.
#[cfg(test)]
mod api_tests {
    use super::*;
    use axum::{body::Body, http::{Method, Request}, response::Response};
    use serde_json::Value;
    use tower::ServiceExt as _; // .oneshot: drive the router without a server

    // The tests read as request in, response out, so the two helpers below
    // keep the plumbing in one place.
    fn req(method: axum::http::Method, uri: &str, key: Option<&str>, body: Option<String>) -> Request<Body> {
        let mut builder = Request::builder().method(method).uri(uri);
        if let Some(key) = key {
            builder = builder.header("authorization", format!("Bearer {key}"));
        }
        if body.is_some() {
            builder = builder.header("content-type", "application/json");
        }
        builder.body(Body::from(body.unwrap_or_default())).unwrap()
    }

    async fn send(app: Router, request: Request<Body>) -> Response {
        app.oneshot(request).await.unwrap()
    }

    async fn body_string(res: Response) -> String {
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        String::from_utf8(bytes.to_vec()).expect("body is utf-8")
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn health_reports_ok(pool: PgPool) {
        let res = send(app(pool, "test-key"), req(Method::GET, "/health", None, None)).await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(body_string(res).await, "ok");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn links_reject_a_missing_api_key(pool: PgPool) {
        let body = serde_json::to_string(&serde_json::json!({ "url": "https://example.org" })).unwrap();
        let res = send(
            app(pool, "test-key"),
            req(Method::POST, "/links", None, Some(body)),
        )
        .await;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_created_link_resolves(pool: PgPool) {
        let router = app(pool, "test-key");
        let body = serde_json::to_string(&serde_json::json!({ "url": "https://example.org/page" })).unwrap();
        let res = send(
            router.clone(),
            req(Method::POST, "/links", Some("test-key"), Some(body)),
        )
        .await;
        assert_eq!(res.status(), StatusCode::CREATED);
        let created: Value = serde_json::from_str(&body_string(res).await).unwrap();
        assert_eq!(created["url"], "https://example.org/page");
        let code = created["code"].as_str().unwrap();

        let res = send(
            router,
            req(Method::GET, &format!("/{code}"), None, None),
        )
        .await;
        assert_eq!(res.status(), StatusCode::FOUND);
        assert_eq!(
            res.headers()["location"],
            "https://example.org/page"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn an_unknown_code_is_a_404(pool: PgPool) {
        let res = send(app(pool, "test-key"), req(Method::GET, "/zzzzzzz", None, None)).await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn an_invalid_url_is_a_422(pool: PgPool) {
        let body = serde_json::to_string(&serde_json::json!({ "url": "not a url" })).unwrap();
        let res = send(
            app(pool, "test-key"),
            req(Method::POST, "/links", Some("test-key"), Some(body)),
        )
        .await;
        assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert!(body_string(res).await.contains("must be a valid URL"));
    }

    // The isolation guarantee, stated as a test: every test gets a fresh
    // database, so a list that was never written to is empty - and if the
    // tests ever start sharing a database, this assertion fails on the
    // second test that runs.
    #[sqlx::test(migrations = "./migrations")]
    async fn each_test_starts_from_an_empty_database(pool: PgPool) {
        let res = send(app(pool, "test-key"), req(Method::GET, "/links", Some("test-key"), None)).await;
        assert_eq!(res.status(), StatusCode::OK);
        let page: Value = serde_json::from_str(&body_string(res).await).unwrap();
        assert_eq!(page["links"].as_array().unwrap().len(), 0);
    }
}

// The whole service as a value. main adds the observability layers from
// step 13 on top and serves it; the tests below drive it directly - no
// listener, no port, no second process. Building the router needs a pool
// handle but opens no connection, so a test can hold one before any query
// has run.
fn app(pool: PgPool, key: impl Into<Arc<str>>) -> Router {
    // Two routers, because the routes have different audiences: browsers
    // hitting /{code} send no headers, and a load balancer polling /health
    // has none to offer. Only /links - the routes that change or read data -
    // sit behind the layers. route_layer, not layer: it runs only once a
    // route has matched, so a typo like /foo/bar stays a 404 instead of
    // being told to authenticate against a path that does not exist.
    // Both route_layers wrap the same routes, and the last one added is the
    // outermost: a request meets auth before the limiter, because the
    // limiter needs the identity auth leaves in the extensions.
    let protected = Router::new()
        .route("/links", get(list_links).post(create_link))
        .route_layer(RateLimit::new(10.0, 5.0))
        .route_layer(RequireApiKey::new(key));

    Router::new()
        .route("/health", get(health))
        .route("/{code}", get(resolve_link))
        .merge(protected)
        .layer(Extension(pool))
}

#[tokio::main]
async fn main() {
    // RUST_LOG picks the level, using the same syntax as env_logger. Without
    // it, info and up. The names are crate names, so `RUST_LOG=shortlink=debug`
    // turns up only this service's own lines and leaves the dependencies alone.
    // No colour: these lines are headed for files and aggregators, and the
    // escape codes would be noise there.
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

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

    // app() builds the routes, the auth layer and the rate limiter. The
    // three layers here are step 13's, and the rule is the same as step
    // 12's: the last one added is the first to see the request. Set runs
    // outermost and gives every request an id - or adopts the one in its
    // x-request-id header. Propagate sits inside it and reads the header
    // back on its way through, so it can stamp the response - a generated
    // id included. The trace layer runs inside both, so its span is made
    // after the id exists and every line carries it.
    let router = app(pool, key)
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(RequestSpan)
                .on_response(DefaultOnResponse::new().level(Level::INFO)),
        )
        .layer(PropagateRequestIdLayer::x_request_id())
        .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid));

    let listener = tokio::net::TcpListener::bind("0.0.0.0:8000").await.unwrap();
    tracing::info!("listening on {}", listener.local_addr().unwrap());
    axum::serve(listener, router).await.unwrap();
}
