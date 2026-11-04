#![allow(dead_code)]
//! Shared helpers for the API integration tests.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use axum::Router;
use eschess_api::{app, AppState, Config};
use eschess_notification::CapturingMailer;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

pub const PASSWORD: &str = "correct horse battery";

pub struct Api {
    pub app: Router,
    pub pool: PgPool,
    pub state: AppState,
}

impl Api {
    pub fn new(pool: PgPool) -> Api {
        Api::with_config(pool, Config::default())
    }

    pub fn with_config(pool: PgPool, config: Config) -> Api {
        let state = AppState {
            pool: pool.clone(),
            config: Arc::new(config),
            mailer: Arc::new(CapturingMailer::default()),
            hub: eschess_api::hub::Hub::new(),
        };
        Api {
            app: app(state.clone()),
            pool,
            state,
        }
    }

    /// Starts a real HTTP and WebSocket server on a free local port, as one server process would, with its own list of
    /// connected players and its own listener for game changes. Returns its address.
    pub async fn serve(&self) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        eschess_api::hub::spawn_listener(self.state.hub.clone(), self.pool.clone());
        let router = self.app.clone();
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        addr
    }

    /// Another server process over the same database: all game state lives there, so it can carry on any game.
    pub fn another_server(&self) -> Api {
        Api::with_config(self.pool.clone(), (*self.state.config).clone())
    }

    pub async fn call(
        &self,
        method: Method,
        path: &str,
        token: Option<&str>,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut req = Request::builder().method(method).uri(path);
        if let Some(t) = token {
            req = req.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        let req = match body {
            Some(b) => req
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(b.to_string()))
                .unwrap(),
            None => req.body(Body::empty()).unwrap(),
        };
        let res = self.app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    pub async fn get(&self, path: &str, token: &str) -> (StatusCode, Value) {
        self.call(Method::GET, path, Some(token), None).await
    }

    pub async fn post(&self, path: &str, token: &str, body: Value) -> (StatusCode, Value) {
        self.call(Method::POST, path, Some(token), Some(body)).await
    }

    /// Signs a player up and returns their access token.
    pub async fn player(&self, name: &str) -> String {
        let (status, body) = self.call(Method::POST, "/v1/auth/signup", None, Some(json!({ "email": format!("{name}@example.com"), "username": name, "password": PASSWORD }))).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        body["access_token"].as_str().unwrap().to_string()
    }

    pub async fn count(&self, sql: &str) -> i64 {
        sqlx::query_scalar(sql).fetch_one(&self.pool).await.unwrap()
    }
}

pub fn code(body: &Value) -> &str {
    body["error"]["code"].as_str().unwrap_or("")
}

pub fn id_of(body: &Value) -> Uuid {
    body["id"].as_str().unwrap().parse().unwrap()
}
