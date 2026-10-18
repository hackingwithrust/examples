use axum::{
    Json, Router,
    extract::{Extension, Path, Query},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, postgres::PgPoolOptions};

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

#[derive(Deserialize)]
struct CreateLink {
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

async fn health(Extension(pool): Extension<PgPool>) -> Result<&'static str, StatusCode> {
    // query! macros read the schema through DATABASE_URL at compile time.
    match sqlx::query_scalar!("SELECT 1").fetch_one(&pool).await {
        Ok(_) => Ok("ok"),
        Err(_) => Err(StatusCode::SERVICE_UNAVAILABLE),
    }
}

async fn create_link(
    Extension(pool): Extension<PgPool>,
    Json(input): Json<CreateLink>,
) -> Result<ApiResponse<Link>, StatusCode> {
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
                    return Err(StatusCode::INTERNAL_SERVER_ERROR);
                }
            }
        }
    }

    // Five collisions in a row out of 3.5 trillion codes is not bad luck,
    // it is a bug - but a 500 beats an infinite loop.
    Err(StatusCode::INTERNAL_SERVER_ERROR)
}

async fn list_links(
    Extension(pool): Extension<PgPool>,
    Query(p): Query<Pagination>,
) -> Result<ApiResponse<LinkPage>, StatusCode> {
    let page = p.page.unwrap_or(1).max(1) as i64;
    let limit = p.limit.unwrap_or(20) as i64;

    let rows = sqlx::query!(
        "SELECT code, url FROM links ORDER BY id LIMIT $1 OFFSET $2",
        limit,
        (page - 1) * limit
    )
    .fetch_all(&pool)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

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
) -> Result<Response, StatusCode> {
    let row = sqlx::query!("SELECT url FROM links WHERE code = $1", code)
        .fetch_optional(&pool)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    match row {
        Some(row) => Ok((
            StatusCode::FOUND,
            [("Location", row.url.as_str())],
        )
            .into_response()),
        None => Err(StatusCode::NOT_FOUND),
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

    let app = Router::new()
        .route("/health", get(health))
        .route("/links", get(list_links).post(create_link))
        .route("/{code}", get(resolve_link))
        .layer(Extension(pool));

    let listener = tokio::net::TcpListener::bind("0.0.0.0:8000").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
