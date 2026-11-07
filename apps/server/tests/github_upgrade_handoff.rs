//! Upgrade and rolling-server regressions for GitHub handoffs written before host binding.
use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use orbit_platform::{
    Config, Database, DatabaseConfig, EnvironmentMode, MigrationRunner, TimestampMillis,
};
use orbit_server::app::App;
use serde_json::json;
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tower::ServiceExt;

const HANDOFF: &str = "legacy-handoff-token";
const VERIFIER: &str = "legacy-client-verifier";

fn app_config(root: &TempDir) -> Config {
    let mut config = Config {
        environment: EnvironmentMode::Development,
        app_key: Some(orbit_platform::AppKey::new([1; 32])),
        ..Config::default()
    };
    config.data.database = root.path().join("data/orbit.sqlite");
    config.data.attachments = root.path().join("attachments");
    config.data.backups = root.path().join("backups");
    config.github = "https://enterprise.example".parse().unwrap();
    config
}

fn payload() -> String {
    json!({
        "kind": "provider_handoff",
        "flow": {"provider": "github", "intent": {"kind": "sign_in"},
            "client_challenge": URL_SAFE_NO_PAD.encode(Sha256::digest(VERIFIER.as_bytes())),
            "redirect": {"kind": "web"}, "return_to": null},
        "outcome": {"Ok": {"provider": "github", "subject": "123",
            "verified_email": "new@example.com", "email": "new@example.com", "display_name": "Legacy"}}
    }).to_string()
}

async fn seed(database: &Database, orphaned: bool) {
    if !orphaned {
        sqlx::query("INSERT INTO sign_in_providers (provider, client_id, client_secret, created_at, updated_at) VALUES ('github', 'legacy', $1, 0, 0)")
            .bind(vec![1_u8]).execute(database.pool()).await.unwrap();
    }
    sqlx::query(
        "INSERT INTO auth_pending (token_hash, expires_at, payload_json) VALUES ($1, $2, $3)",
    )
    .bind(format!("{:x}", Sha256::digest(HANDOFF.as_bytes())))
    .bind(TimestampMillis::now().as_millis() + 600_000)
    .bind(payload())
    .execute(database.pool())
    .await
    .unwrap();
}

async fn assert_preserved(database: &Database) {
    let pending: (String, i64) =
        sqlx::query_as("SELECT payload_json, expires_at FROM auth_pending")
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert_eq!(pending.0, payload());
    assert!(pending.1 > TimestampMillis::now().as_millis());
    let identities: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM user_identities WHERE provider = 'github'")
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert_eq!(identities, 0);
}

// Database::open maps this unique path to an isolated Postgres database in backend tests.
// Drop it explicitly, as this fixture needs to close the database before App::build reopens it.
async fn cleanup_postgres(name: Option<String>) {
    if let Some(name) = name {
        use sqlx::Connection;
        assert!(
            name.starts_with("orbit_t_")
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        );
        let mut connection =
            sqlx::PgConnection::connect(&std::env::var("ORBIT_TEST_POSTGRES_URL").unwrap())
                .await
                .unwrap();
        sqlx::query(&format!("DROP DATABASE \"{name}\" WITH (FORCE)"))
            .execute(&mut connection)
            .await
            .unwrap();
    }
}

async fn postgres_name(database: &Database) -> Option<String> {
    if orbit_platform::testing_postgres() {
        Some(
            sqlx::query_scalar("SELECT current_database()")
                .fetch_one(database.pool())
                .await
                .unwrap(),
        )
    } else {
        None
    }
}

#[tokio::test]
async fn legacy_handoffs_refuse_enterprise_upgrade_and_rolling_exchange() {
    for orphaned in [false, true] {
        let root = TempDir::new().unwrap();
        let config = app_config(&root);
        let db_config = DatabaseConfig::for_data(&config.data);
        let db = Database::open(&db_config).await.unwrap();
        let name = postgres_name(&db).await;
        MigrationRunner::embedded_through("legacy", 77)
            .run(&db)
            .await
            .unwrap();
        seed(&db, orphaned).await;
        assert_preserved(&db).await;
        drop(db);

        let error = App::build(config.clone()).await.unwrap_err().to_string();
        assert!(
            error.contains("https://github.com") && error.contains("https://enterprise.example"),
            "{error}"
        );
        let db = Database::open(&db_config).await.unwrap();
        let version: i64 = sqlx::query_scalar("SELECT MAX(version) FROM schema_migrations")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(version, 78);
        let origin: String =
            sqlx::query_scalar("SELECT origin FROM github_host_binding WHERE singleton = 1")
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(origin, "https://github.com");
        assert_preserved(&db).await;
        drop(db);
        cleanup_postgres(name).await;

        // An already-running Enterprise server must also fail closed if the binding changes
        // underneath it (e.g. another server establishes the legacy binding during a rollout).
        let root = TempDir::new().unwrap();
        let app = App::build(app_config(&root)).await.unwrap();
        let name = postgres_name(app.database()).await;
        seed(app.database(), orphaned).await;
        sqlx::query(
            "UPDATE github_host_binding SET origin = 'https://github.com' WHERE singleton = 1",
        )
        .execute(app.database().pool())
        .await
        .unwrap();
        let response = app
            .router()
            .oneshot(
                Request::post("/api/v1/auth/providers/exchange")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        json!({"handoff": HANDOFF, "code_verifier": VERIFIER}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_preserved(app.database()).await;
        drop(app);
        cleanup_postgres(name).await;
    }
}
