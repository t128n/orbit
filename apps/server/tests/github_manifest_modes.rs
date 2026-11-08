use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use orbit_platform::{
    HttpPlatformLayer, OriginPolicy, PasswordService, TestDatabase, TimestampMillis,
};
use orbit_server::auth_routes::CookieMode;
use orbit_server::integration_routes::{IntegrationState, integration_router};
use orbit_server::repositories::api_tokens::ApiTokenRepository;
use orbit_server::repositories::identity::{IdentityRepository, SetupRequest};
use orbit_server::repositories::tasks::TaskRepository;
use serde_json::{Value, json};
use tower::ServiceExt;

#[tokio::test]
async fn registration_manifest_respects_workspace_sync_mode() {
    let database = TestDatabase::new().await.unwrap();
    orbit_server::github_binding::bind(&database, &orbit_platform::GitHubHost::default())
        .await
        .unwrap();
    let identity = IdentityRepository::new((*database).clone());
    let now = TimestampMillis::now();
    identity
        .store_setup_token(
            "setup",
            TimestampMillis::from_millis(now.as_millis() + 60_000),
        )
        .await
        .unwrap();
    let setup = identity
        .complete_setup(
            SetupRequest {
                token: "setup".into(),
                email: "owner@example.com".into(),
                display_name: "Owner".into(),
                password_hash: PasswordService::default()
                    .hash("correct horse battery")
                    .unwrap(),
                workspace_name: "Orbit".into(),
                project_name: "General".into(),
            },
            now,
        )
        .await
        .unwrap();
    let state = IntegrationState::new(
        Arc::new(ApiTokenRepository::new((*database).clone())),
        Arc::new(TaskRepository::new((*database).clone())),
    )
    .with_github_settings(
        Arc::new(identity),
        CookieMode::secure(),
        "https://orbit.test".into(),
        false,
        Some([7; 32]),
    );
    let app = integration_router(state).layer(HttpPlatformLayer::new(OriginPolicy::new(
        "https://orbit.test",
    )));
    let path = format!("/api/v1/workspaces/{}/github/manifest", setup.workspace_id);
    let cookie = format!("__Host-orbit_session={}", setup.session.token);
    for mode in [None, Some("webhooks"), Some("polling")] {
        if let Some(mode) = mode {
            sqlx::query("INSERT INTO github_workspace_settings (workspace_id, sync_mode) VALUES ($1, $2) ON CONFLICT (workspace_id) DO UPDATE SET sync_mode = excluded.sync_mode")
                .bind(setup.workspace_id.to_string()).bind(mode).execute(database.pool()).await.unwrap();
        }
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(&path)
                    .header(header::COOKIE, &cookie)
                    .header(header::ORIGIN, "https://orbit.test")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let result: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        let mut expected = json!({
            "name": "Orbit", "url": "https://orbit.test",
            "redirect_url": "https://orbit.test/api/v1/integrations/github/manifest/callback",
            "setup_url": format!("https://orbit.test/settings/github?workspace={}&github=installed", setup.workspace_id),
            "public": false, "default_permissions": {"issues": "read", "pull_requests": "read"},
            "default_events": if mode == Some("polling") { json!([]) } else { json!(["issues", "pull_request"]) }
        });
        if mode != Some("polling") {
            expected["hook_attributes"] = json!({
                "url": "https://orbit.test/api/v1/integrations/github/webhook",
                "active": true
            });
        }
        assert_eq!(result["manifest"], expected);
        let stored: Option<String> = sqlx::query_scalar(
            "SELECT sync_mode FROM github_workspace_settings WHERE workspace_id = $1",
        )
        .bind(setup.workspace_id.to_string())
        .fetch_optional(database.pool())
        .await
        .unwrap();
        assert_eq!(stored.as_deref(), mode);
    }
}
