use axum::{
    Json, Router,
    extract::{Path, Query},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use serde::{Deserialize, Serialize};

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

async fn health() -> &'static str {
    "ok"
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
    // The database arrives in step 5; until then the listing has one
    // hardcoded row so the JSON shape is visible.
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
    // No database yet, so there is nothing to look the target up in.
    // Step 8 replaces this with the real lookup.
    ApiResponse::ok(ResolvedLink {
        code,
        target: None,
    })
}

#[tokio::main]
async fn main() {
    let app = Router::new()
        .route("/health", get(health))
        .route("/links", get(list_links).post(create_link))
        .route("/{code}", get(resolve_link));

    let listener = tokio::net::TcpListener::bind("0.0.0.0:8000").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
