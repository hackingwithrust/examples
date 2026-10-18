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

#[derive(Serialize)]
struct ResolvedLink {
    code: String,
    target: Option<String>,
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
    match sqlx::query_scalar::<_, i32>("SELECT 1").fetch_one(&pool).await {
        Ok(_) => Ok("ok"),
        Err(_) => Err(StatusCode::SERVICE_UNAVAILABLE),
    }
}

async fn create_link(Json(input): Json<CreateLink>) -> ApiResponse<Link> {
    // Nothing is stored yet - step 8 adds the insert, step 9 the code
    // generation. The stub code keeps the response shape right.
    ApiResponse::with_status(
        StatusCode::CREATED,
        Link {
            code: "stub".to_string(),
            url: input.url,
        },
    )
}

async fn list_links(Query(p): Query<Pagination>) -> ApiResponse<LinkPage> {
    // Still hardcoded - step 8 replaces this with a real query against
    // the links table.
    let links = vec![Link {
        code: "9xK2mQz".to_string(),
        url: "https://example.com/very/long/path".to_string(),
    }];
    ApiResponse::ok(LinkPage {
        page: p.page.unwrap_or(1),
        limit: p.limit.unwrap_or(20),
        links,
    })
}

async fn resolve_link(Path(code): Path<String>) -> ApiResponse<ResolvedLink> {
    // Nothing is stored yet, so there is nothing to look the target up
    // in. The table arrives with step 6's migration, the real lookup
    // with step 8.
    ApiResponse::ok(ResolvedLink {
        code,
        target: None,
    })
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

    let app = Router::new()
        .route("/health", get(health))
        .route("/links", get(list_links).post(create_link))
        .route("/{code}", get(resolve_link))
        .layer(Extension(pool));

    let listener = tokio::net::TcpListener::bind("0.0.0.0:8000").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
