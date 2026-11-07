use std::sync::Arc;

use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use orbit_platform::{
    HttpPlatformLayer, OriginPolicy, PasswordService, TestDatabase, TimestampMillis,
};
use orbit_server::{
    auth_routes::CookieMode,
    integration_routes::{IntegrationState, integration_router},
    repositories::{
        api_tokens::ApiTokenRepository,
        identity::{IdentityRepository, SetupRequest},
        tasks::TaskRepository,
    },
};
use serde_json::Value;
use tower::ServiceExt;

#[tokio::test]
async fn sync_is_queued_only_in_polling_mode_and_reports_pending_handoff() {
    let database = TestDatabase::new().await.unwrap();
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
    let workspace = setup.workspace_id.to_string();
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
    let path = format!("/api/v1/workspaces/{workspace}/github/sync");
    let cookie = format!("__Host-orbit_session={}", setup.session.token);
    let request = |method: &str| {
        Request::builder()
            .method(method)
            .uri(&path)
            .header(header::COOKIE, &cookie)
            .header(header::ORIGIN, "https://orbit.test")
            .body(Body::empty())
            .unwrap()
    };
    assert_eq!(
        app.clone().oneshot(request("POST")).await.unwrap().status(),
        StatusCode::CONFLICT
    );
    sqlx::query(
        "INSERT INTO github_workspace_settings (workspace_id, sync_mode) VALUES ($1, 'polling')",
    )
    .bind(&workspace)
    .execute(database.pool())
    .await
    .unwrap();
    assert_eq!(
        app.clone().oneshot(request("POST")).await.unwrap().status(),
        StatusCode::ACCEPTED
    );
    assert_eq!(
        app.clone().oneshot(request("POST")).await.unwrap().status(),
        StatusCode::ACCEPTED
    );
    let response = app.oneshot(request("GET")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let status: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(status["sync_mode"], "polling");
    assert_eq!(status["sync_pending"], true);
    assert!(status["transition_target"].is_null());
    assert!(status["next_poll_at"].is_null());
}
