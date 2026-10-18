use axum::{
    Json, Router,
    extract::{Extension, Path, Query},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, postgres::PgPoolOptions};

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
    // The code is still the stub from step 4 - generating a real one is
    // step 9. Everything else about the insert is final.
    let row = sqlx::query!(
        "INSERT INTO links (code, url) VALUES ('stub', $1) RETURNING id, code, url",
        input.url
    )
    .fetch_one(&pool)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(ApiResponse::with_status(
        StatusCode::CREATED,
        Link {
            code: row.code,
            url: row.url,
        },
    ))
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
