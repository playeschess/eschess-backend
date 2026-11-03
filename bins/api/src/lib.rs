use axum::{routing::get, Json, Router};
use serde_json::{json, Value};

/// The API's routes. Kept separate from `main` so tests can call it without opening a network port.
pub fn app() -> Router {
    Router::new().route("/health", get(health))
}

async fn health() -> Json<Value> {
    Json(json!({ "status": "ok", "service": "eschess-api", "version": env!("CARGO_PKG_VERSION") }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    #[tokio::test]
    async fn health_reports_ok() {
        let res = app()
            .oneshot(Request::get("/health").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body: Value =
            serde_json::from_slice(&res.into_body().collect().await.unwrap().to_bytes()).unwrap();
        assert_eq!(body["status"], "ok");
        assert_eq!(body["service"], "eschess-api");
    }

    #[tokio::test]
    async fn unknown_routes_are_404() {
        let res = app()
            .oneshot(Request::get("/nope").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }
}
