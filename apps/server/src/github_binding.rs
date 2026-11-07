//! Persistent GitHub host identity for this database.
use crate::auth_routes::two_factor::pending_may_use_github;
use orbit_platform::{Database, GitHubHost};
use sqlx::{Any, Transaction};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum GitHubBindingError {
    #[error(
        "changing hosts with existing GitHub data is not supported in this release: database is bound to {bound}, configured host is {configured}"
    )]
    Mismatch { bound: String, configured: String },
    #[error("GitHub host binding is missing")]
    Missing,
    #[error("GitHub host binding database error: {0}")]
    Database(#[from] sqlx::Error),
}

/// Bind on first startup. Legacy GitHub records belong to the default host.
pub async fn bind(database: &Database, host: &GitHubHost) -> Result<(), GitHubBindingError> {
    let mut tx = database.immediate_transaction().await?;
    let existing: Option<String> =
        sqlx::query_scalar("SELECT origin FROM github_host_binding WHERE singleton = 1")
            .fetch_optional(&mut *tx)
            .await?;
    if existing.is_none() {
        let legacy: i64 = sqlx::query_scalar("SELECT CAST(CASE WHEN EXISTS (SELECT 1 FROM github_apps) OR EXISTS (SELECT 1 FROM github_app_registrations) OR EXISTS (SELECT 1 FROM github_installations) OR EXISTS (SELECT 1 FROM github_installation_repositories) OR EXISTS (SELECT 1 FROM github_project_connections) OR EXISTS (SELECT 1 FROM github_issue_links) OR EXISTS (SELECT 1 FROM github_pull_links) OR EXISTS (SELECT 1 FROM sign_in_providers WHERE provider = 'github') OR EXISTS (SELECT 1 FROM user_identities WHERE provider = 'github') THEN 1 ELSE 0 END AS BIGINT)")
            .fetch_one(&mut *tx).await?;
        let pending_github = if legacy == 0 {
            let payloads: Vec<String> =
                sqlx::query_scalar("SELECT payload_json FROM auth_pending WHERE expires_at > $1")
                    .bind(orbit_platform::TimestampMillis::now().as_millis())
                    .fetch_all(&mut *tx)
                    .await?;
            payloads
                .iter()
                .any(|payload| pending_may_use_github(payload))
        } else {
            false
        };
        let origin = if legacy != 0 || pending_github {
            GitHubHost::default()
        } else {
            host.clone()
        };
        sqlx::query("INSERT INTO github_host_binding (singleton, origin) VALUES (1, $1)")
            .bind(origin.web_origin())
            .execute(&mut *tx)
            .await?;
        // Persist the legacy binding even when this startup must be refused.
        tx.commit().await?;
        return compare(origin.web_origin(), host);
    }
    compare(existing.as_deref().expect("checked above"), host)?;
    tx.commit().await?;
    Ok(())
}

fn compare(bound: &str, host: &GitHubHost) -> Result<(), GitHubBindingError> {
    if bound == host.web_origin() {
        Ok(())
    } else {
        Err(GitHubBindingError::Mismatch {
            bound: bound.to_owned(),
            configured: host.web_origin().to_owned(),
        })
    }
}

/// Check the persistent binding in an existing transaction. Use an immediate transaction for writes.
pub async fn check_in_transaction(
    tx: &mut Transaction<'_, Any>,
    host: &GitHubHost,
) -> Result<(), GitHubBindingError> {
    let bound: Option<String> =
        sqlx::query_scalar("SELECT origin FROM github_host_binding WHERE singleton = 1")
            .fetch_optional(&mut **tx)
            .await?;
    compare(bound.as_deref().ok_or(GitHubBindingError::Missing)?, host)
}

/// Check the persistent binding without changing it.
pub async fn check(database: &Database, host: &GitHubHost) -> Result<(), GitHubBindingError> {
    let bound: Option<String> =
        sqlx::query_scalar("SELECT origin FROM github_host_binding WHERE singleton = 1")
            .fetch_optional(database.pool())
            .await?;
    compare(bound.as_deref().ok_or(GitHubBindingError::Missing)?, host)
}

#[cfg(test)]
mod tests {
    use super::*;
    use orbit_platform::TestDatabase;

    #[test]
    fn pending_classification_is_conservative() {
        for payload in [
            r#"{"kind":"provider_authorization","flow":{"provider":"google"}}"#,
            r#"{"kind":"passkey_login"}"#,
            r#"{"kind":"unknown"}"#,
            "not json",
        ] {
            assert!(pending_may_use_github(payload), "{payload}");
        }
        for payload in [
            r#"{"kind":"provider_authorization","flow":{"provider":"google","intent":{"kind":"sign_in"},"client_challenge":"challenge","redirect":{"kind":"web"},"return_to":null},"pkce_verifier":"verifier"}"#,
            r#"{"kind":"provider_handoff","flow":{"provider":"google","intent":{"kind":"sign_in"},"client_challenge":"challenge","redirect":{"kind":"web"},"return_to":null},"outcome":{"Err":"denied"}}"#,
            r#"{"kind":"login","user":{"id":"01890f47-0000-7000-8000-000000000001","email":"a@example.test","display_name":"A"},"first_factor":{"kind":"password","password_hash":"hash","replacement_hash":null},"attempts":0}"#,
        ] {
            assert!(!pending_may_use_github(payload), "{payload}");
        }
    }

    #[tokio::test]
    async fn unexpired_pending_flows_preserve_legacy_host_binding() {
        for (payload, unexpired, expected_github) in [
            (
                r#"{"kind":"provider_handoff","flow":{"provider":"github","intent":{"kind":"sign_in"},"client_challenge":"challenge","redirect":{"kind":"web"},"return_to":null},"outcome":{"Ok":{"provider":"github","subject":"123","verified_email":null,"email":null,"display_name":"A"}}}"#,
                true,
                true,
            ),
            (
                r#"{"kind":"login","user":{"id":"01890f47-0000-7000-8000-000000000001","email":"a@example.test","display_name":"A"},"first_factor":{"kind":"provider","profile":{"provider":"github","subject":"123","verified_email":null,"email":null,"display_name":"A"},"link":true},"attempts":0}"#,
                true,
                true,
            ),
            (
                r#"{"kind":"provider_authorization","flow":{"provider":"google","intent":{"kind":"sign_in"},"client_challenge":"challenge","redirect":{"kind":"web"},"return_to":null},"pkce_verifier":"verifier"}"#,
                true,
                false,
            ),
            (
                r#"{"kind":"provider_handoff","flow":{"provider":"github"}}"#,
                false,
                false,
            ),
            ("invalid", true, true),
        ] {
            let db = TestDatabase::new().await.unwrap();
            let now = orbit_platform::TimestampMillis::now().as_millis();
            sqlx::query("INSERT INTO auth_pending (token_hash, expires_at, payload_json) VALUES ($1, $2, $3)")
                .bind("pending")
                .bind(if unexpired { now + 60_000 } else { now - 1 })
                .bind(payload)
                .execute(db.pool()).await.unwrap();
            let host: GitHubHost = "https://enterprise.example".parse().unwrap();
            let result = bind(&db, &host).await;
            if expected_github {
                assert!(
                    matches!(result, Err(GitHubBindingError::Mismatch { .. })),
                    "{payload}"
                );
                check(&db, &GitHubHost::default()).await.unwrap();
            } else {
                result.unwrap();
                check(&db, &host).await.unwrap();
            }
            let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM auth_pending")
                .fetch_one(db.pool())
                .await
                .unwrap();
            assert_eq!(remaining, 1);
        }
    }

    #[tokio::test]
    async fn binding_is_persistent_and_serialized() {
        let db = TestDatabase::new().await.unwrap();
        let first: GitHubHost = "https://enterprise.example".parse().unwrap();
        let second: GitHubHost = "https://other.example".parse().unwrap();
        let (a, b) = tokio::join!(bind(&db, &first), bind(&db, &second));
        assert_ne!(a.is_ok(), b.is_ok());
        let winner = if a.is_ok() { &first } else { &second };
        let loser = if a.is_ok() { &second } else { &first };
        check(&db, winner).await.unwrap();
        let error = check(&db, loser).await.unwrap_err().to_string();
        assert!(error.contains(winner.web_origin()) && error.contains(loser.web_origin()));
        assert!(
            error.contains(
                "changing hosts with existing GitHub data is not supported in this release"
            )
        );
        let mut tx = db.immediate_transaction().await.unwrap();
        check_in_transaction(&mut tx, winner).await.unwrap();
        tx.commit().await.unwrap();
    }

    #[tokio::test]
    async fn legacy_provider_binds_default_even_on_mismatch() {
        let db = TestDatabase::new().await.unwrap();
        sqlx::query("INSERT INTO sign_in_providers (provider, client_id, client_secret, created_at, updated_at) VALUES ('github', 'id', $1, 0, 0)")
            .bind(vec![1_u8]).execute(db.pool()).await.unwrap();
        let enterprise: GitHubHost = "https://enterprise.example".parse().unwrap();
        assert!(matches!(
            bind(&db, &enterprise).await,
            Err(GitHubBindingError::Mismatch { .. })
        ));
        check(&db, &GitHubHost::default()).await.unwrap();
    }

    #[tokio::test]
    async fn missing_binding_and_same_host_concurrent_startup() {
        let db = TestDatabase::new().await.unwrap();
        let host: GitHubHost = "https://enterprise.example".parse().unwrap();
        assert!(matches!(
            check(&db, &host).await,
            Err(GitHubBindingError::Missing)
        ));
        let (a, b) = tokio::join!(bind(&db, &host), bind(&db, &host));
        a.unwrap();
        b.unwrap();
        bind(&db, &host).await.unwrap();
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM github_host_binding")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn non_github_identity_data_does_not_force_default() {
        let db = TestDatabase::new().await.unwrap();
        sqlx::query("INSERT INTO users (id, email, normalized_email, display_name, password_hash, created_at, updated_at) VALUES ('u', 'a@example.test', 'a@example.test', 'A', '', 0, 0)")
            .execute(db.pool()).await.unwrap();
        sqlx::query("INSERT INTO sign_in_providers (provider, client_id, client_secret, created_at, updated_at) VALUES ('google', 'id', $1, 0, 0)")
            .bind(vec![1_u8]).execute(db.pool()).await.unwrap();
        sqlx::query("INSERT INTO user_identities (provider, subject, user_id, created_at) VALUES ('google', 'subject', 'u', 0)")
            .execute(db.pool()).await.unwrap();
        let host: GitHubHost = "https://enterprise.example".parse().unwrap();
        bind(&db, &host).await.unwrap();
        check(&db, &host).await.unwrap();
    }

    #[tokio::test]
    async fn legacy_records_bind_default() {
        // Each fixture uses a fresh database. Child installation rows require their GitHub app
        // parent by foreign key; issue links, pull links, registrations and identities stand alone.
        for source in [
            "app",
            "registration",
            "installation",
            "repository",
            "connection",
            "issue",
            "pull",
            "identity",
        ] {
            let db = TestDatabase::new().await.unwrap();
            let mut tx = db.immediate_transaction().await.unwrap();
            for statement in [
                "INSERT INTO users (id, email, normalized_email, display_name, password_hash, created_at, updated_at) VALUES ('u', 'a@example.test', 'a@example.test', 'A', '', 0, 0)",
                "INSERT INTO workspaces (id, name, owner_membership_id, created_at, updated_at) VALUES ('w', 'W', 'm', 0, 0)",
                "INSERT INTO memberships (id, workspace_id, user_id, role, created_at, updated_at) VALUES ('m', 'w', 'u', 'owner', 0, 0)",
                "INSERT INTO projects (id, workspace_id, name, project_key, color, created_at, updated_at) VALUES ('p', 'w', 'P', 'P', '#000000', 0, 0)",
                "INSERT INTO task_statuses (id, workspace_id, project_id, name, description, color, category, position, created_at, updated_at) VALUES ('s', 'w', 'p', 'S', '', '#000000', 'unstarted', 0, 0, 0)",
                "INSERT INTO tasks (id, workspace_id, project_id, status_id, title, creator_id, created_at, updated_at) VALUES ('t', 'w', 'p', 's', 'T', 'u', 0, 0)",
            ] {
                sqlx::query(statement).execute(&mut *tx).await.unwrap();
            }
            tx.commit().await.unwrap();
            if matches!(source, "app" | "installation" | "repository" | "connection") {
                sqlx::query("INSERT INTO github_apps (workspace_id, app_id, slug, private_key_encrypted, webhook_secret_encrypted, created_at) VALUES ('w', 1, 'app', $1, $2, 0)")
                    .bind(vec![1_u8]).bind(vec![2_u8]).execute(db.pool()).await.unwrap();
            }
            if matches!(source, "installation" | "repository" | "connection") {
                sqlx::query("INSERT INTO github_installations (installation_id, workspace_id, account_login) VALUES (1, 'w', 'account')")
                    .execute(db.pool()).await.unwrap();
            }
            let statement = match source {
                "registration" => {
                    sqlx::query("INSERT INTO github_app_registrations (state_hash, workspace_id, expires_at, public_origin) VALUES ($1, 'w', 1, '')")
                        .bind(vec![1_u8]).execute(db.pool()).await.unwrap();
                    None
                }
                "repository" => Some(
                    "INSERT INTO github_installation_repositories (installation_id, repository) VALUES (1, 'o/r')",
                ),
                "connection" => Some(
                    "INSERT INTO github_project_connections (project_id, workspace_id, installation_id, repository, label) VALUES ('p', 'w', 1, 'o/r', 'label')",
                ),
                "issue" => Some(
                    "INSERT INTO github_issue_links (workspace_id, repository, issue_number, task_id) VALUES ('w', 'o/r', 1, 't')",
                ),
                "pull" => Some(
                    "INSERT INTO github_pull_links (workspace_id, repository, pull_number, task_id, title, url, state, updated_at) VALUES ('w', 'o/r', 1, 't', 'title', 'url', 'open', 0)",
                ),
                "identity" => Some(
                    "INSERT INTO user_identities (provider, subject, user_id, created_at) VALUES ('github', 'subject', 'u', 0)",
                ),
                _ => None,
            };
            if let Some(statement) = statement {
                sqlx::query(statement).execute(db.pool()).await.unwrap();
            }
            let enterprise: GitHubHost = "https://enterprise.example".parse().unwrap();
            let error = bind(&db, &enterprise).await.unwrap_err();
            assert!(
                matches!(error, GitHubBindingError::Mismatch { .. }),
                "{source}: {error}"
            );
            check(&db, &GitHubHost::default()).await.unwrap();
        }
    }
}
