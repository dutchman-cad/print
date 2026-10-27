//! Shared assertions for the public controls contract.
#![allow(dead_code)]
use axum::{body::Body, http::Request, Router};
use dutchman_print::{config::Config, printers::Printers};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt as _;

pub const ALL: &[&str] = &["start", "pause", "resume", "cancel", "stop", "clear"];

pub fn app(config: Config) -> (tempfile::TempDir, Router) {
    app_backend(Arc::new(Printers::new(config).unwrap()))
}
pub fn app_backend(
    backend: Arc<dyn dutchman_print::backend::Backend>,
) -> (tempfile::TempDir, Router) {
    let dir = tempfile::tempdir().unwrap();
    let mut settings = dutchman_print::ServerConfig::default();
    settings.storage.data_dir = dir.path().join("api");
    settings.storage.min_free = 0;
    std::fs::create_dir_all(&settings.storage.data_dir).unwrap();
    let app = dutchman_print::router(backend, settings);
    (dir, app)
}
pub async fn listed(app: &Router, expected: &[&str]) {
    let response = app
        .clone()
        .oneshot(
            Request::get("/api/v1/printers")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    let printers: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(printers[0]["controls"], json!(expected));
}
pub async fn unsupported(app: &Router, printer: &str, actions: &[&str]) {
    for action in actions {
        let request = Request::post("/api/v1/confirmations")
            .header("Content-Type", "application/json")
            .body(Body::from(
                json!({"printer":printer,"action":action}).to_string(),
            ))
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), 409, "{action}");
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(!body["error"].as_str().unwrap().is_empty());
        if !matches!(*action, "start" | "cancel") {
            let response = app
                .clone()
                .oneshot(
                    Request::post(format!("/api/v1/printers/{printer}/{action}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), 409, "{action} route");
        }
    }
}
