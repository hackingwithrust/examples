use axum::{
    Router,
    extract::{Path, Query},
    routing::get,
};
use serde::Deserialize;

#[derive(Deserialize)]
struct Pagination {
    page: Option<usize>,
    limit: Option<usize>,
}

async fn health() -> &'static str {
    "ok"
}

async fn list_links(Query(p): Query<Pagination>) -> String {
    format!("page {}, limit {}", p.page.unwrap_or(1), p.limit.unwrap_or(20))
}

async fn resolve_link(Path(code): Path<String>) -> String {
    format!("will redirect /{code} to its target once the database exists")
}

#[tokio::main]
async fn main() {
    let app = Router::new()
        .route("/health", get(health))
        .route("/links", get(list_links))
        .route("/{code}", get(resolve_link));

    let listener = tokio::net::TcpListener::bind("0.0.0.0:8000").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
