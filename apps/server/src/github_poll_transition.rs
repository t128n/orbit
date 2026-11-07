//! Durable mode handoff primitives. These are deliberately not wired into the API
//! until a worker applies every poll action with the same fencing guarantees.
//!
//! A pending transition survives process failure. Only a generation started after
//! the request can complete it; an already running poll is never a final poll.

use orbit_platform::{Database, Row};

use crate::github_poll::PollOutcome;
use crate::github_poll_schedule::{self, Claim};

/// Start polling, or request a final poll before switching back to webhooks.
/// Returns false for an invalid interval or when the requested mode is already active
/// and no pending handoff needs cancellation.
pub(crate) async fn request(
    database: &Database,
    workspace: &str,
    target: &str,
    interval: i64,
) -> Result<bool, sqlx::Error> {
    if !matches!(target, "polling" | "webhooks") || !(1..=15).contains(&interval) {
        return Ok(false);
    }
    let mut tx = database.immediate_transaction().await?;
    sqlx::query("INSERT INTO github_workspace_settings (workspace_id) VALUES ($1) ON CONFLICT (workspace_id) DO NOTHING")
        .bind(workspace).execute(&mut *tx).await?;
    let settings = sqlx::query("SELECT sync_mode, config_version, mode_epoch FROM github_workspace_settings WHERE workspace_id = $1")
        .bind(workspace).fetch_one(&mut *tx).await?;
    let mode: String = settings.get("sync_mode");
    if mode == target {
        // An explicit request for the active mode cancels any pending handoff.
        // Serialize this with completion so a stale final poll cannot switch modes.
        let removed = sqlx::query("DELETE FROM github_poll_transition WHERE workspace_id = $1")
            .bind(workspace).execute(&mut *tx).await?.rows_affected();
        tx.commit().await?;
        return Ok(removed != 0);
    }
    let pending = sqlx::query("SELECT target_mode, config_version, mode_epoch, poll_interval_minutes FROM github_poll_transition WHERE workspace_id = $1")
        .bind(workspace).fetch_optional(&mut *tx).await?;
    if pending.is_some_and(|row| row.get::<String, _>("target_mode") == target
        && row.get::<i64, _>("config_version") == settings.get::<i64, _>("config_version")
        && row.get::<i64, _>("mode_epoch") == settings.get::<i64, _>("mode_epoch")
        && row.get::<i64, _>("poll_interval_minutes") == interval)
    {
        tx.commit().await?;
        return Ok(true);
    }
    sqlx::query("INSERT INTO github_poll_state (workspace_id) VALUES ($1) ON CONFLICT (workspace_id) DO NOTHING")
        .bind(workspace).execute(&mut *tx).await?;
    let generation: i64 = sqlx::query_scalar("SELECT generation FROM github_poll_state WHERE workspace_id = $1")
        .bind(workspace).fetch_one(&mut *tx).await?;
    let minimum = generation.checked_add(1).ok_or_else(|| sqlx::Error::Protocol("GitHub polling generation exhausted".into()))?;
    let mut version: i64 = settings.get("config_version");
    let mut epoch: i64 = settings.get("mode_epoch");
    if target == "polling" {
        version = version.checked_add(1).ok_or_else(|| sqlx::Error::Protocol("GitHub configuration version exhausted".into()))?;
        epoch = epoch.checked_add(1).ok_or_else(|| sqlx::Error::Protocol("GitHub mode epoch exhausted".into()))?;
        sqlx::query("UPDATE github_workspace_settings SET sync_mode = 'polling', poll_interval_minutes = $1, config_version = $2, mode_epoch = $3 WHERE workspace_id = $4")
            .bind(interval).bind(version).bind(epoch).bind(workspace).execute(&mut *tx).await?;
    }
    sqlx::query("INSERT INTO github_poll_transition (workspace_id, target_mode, config_version, mode_epoch, minimum_generation, poll_interval_minutes) VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (workspace_id) DO UPDATE SET target_mode = excluded.target_mode, config_version = excluded.config_version, mode_epoch = excluded.mode_epoch, minimum_generation = excluded.minimum_generation, poll_interval_minutes = excluded.poll_interval_minutes")
        .bind(workspace).bind(target).bind(version).bind(epoch).bind(minimum).bind(interval).execute(&mut *tx).await?;
    // Coalesce with any in-flight poll. The scheduler retains this marker when it finishes.
    // Rate limits and minimum manual spacing still apply.
    sqlx::query("UPDATE github_poll_state SET next_poll_at = -1 WHERE workspace_id = $1")
        .bind(workspace).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use orbit_platform::{PasswordService, TestDatabase, TimestampMillis};
    use crate::github_poll::RepositoryError;
    use crate::repositories::identity::{IdentityRepository, SetupRequest};

    #[tokio::test]
    async fn handoff_requires_new_complete_fenced_poll_and_survives_failure() {
        let database = TestDatabase::new().await.unwrap();
        let identity = IdentityRepository::new((*database).clone());
        let now = TimestampMillis::now();
        identity.store_setup_token("setup", TimestampMillis::from_millis(now.as_millis() + 60_000)).await.unwrap();
        let setup = identity.complete_setup(SetupRequest {
            token: "setup".into(), email: "owner@example.com".into(), display_name: "Owner".into(),
            password_hash: PasswordService::default().hash("correct horse battery").unwrap(),
            workspace_name: "Orbit".into(), project_name: "General".into(),
        }, now).await.unwrap();
        let workspace = setup.workspace_id.to_string();
        assert!(request(&database, &workspace, "polling", 5).await.unwrap());
        let initial = github_poll_schedule::try_claim(&database, &workspace, "initial", 600_000).await.unwrap().unwrap();
        assert!(complete_applied(&database, &initial, &PollOutcome::default()).await.unwrap());
        assert!(request(&database, &workspace, "webhooks", 5).await.unwrap());
        assert!(request(&database, &workspace, "webhooks", 5).await.unwrap());
        assert!(!complete_applied(&database, &initial, &PollOutcome::default()).await.unwrap());
        github_poll_schedule::release(&database, &initial).await.unwrap();
        // A restarted scheduler can take the durable request after the spacing period.
        sqlx::query("UPDATE github_poll_state SET last_started_at = 0 WHERE workspace_id = $1").bind(&workspace).execute(database.pool()).await.unwrap();
        let final_claim = github_poll_schedule::try_claim(&database, &workspace, "restart", 600_000).await.unwrap().unwrap();
        let mut incomplete = PollOutcome::default();
        incomplete.errors.push(RepositoryError { repository: "a/b".into(), message: "failed".into() });
        assert!(!complete_applied(&database, &final_claim, &incomplete).await.unwrap());
        let mode: String = sqlx::query_scalar("SELECT sync_mode FROM github_workspace_settings WHERE workspace_id = $1").bind(&workspace).fetch_one(database.pool()).await.unwrap();
        assert_eq!(mode, "polling");
        assert!(complete_applied(&database, &final_claim, &PollOutcome::default()).await.unwrap());
        let mode: String = sqlx::query_scalar("SELECT sync_mode FROM github_workspace_settings WHERE workspace_id = $1").bind(&workspace).fetch_one(database.pool()).await.unwrap();
        assert_eq!(mode, "webhooks");
    }

    #[tokio::test]
    async fn requesting_current_mode_cancels_pending_handoff() {
        let database = TestDatabase::new().await.unwrap();
        let identity = IdentityRepository::new((*database).clone());
        let now = TimestampMillis::now();
        identity.store_setup_token("setup", TimestampMillis::from_millis(now.as_millis() + 60_000)).await.unwrap();
        let setup = identity.complete_setup(SetupRequest {
            token: "setup".into(), email: "owner@example.com".into(), display_name: "Owner".into(),
            password_hash: PasswordService::default().hash("correct horse battery").unwrap(),
            workspace_name: "Orbit".into(), project_name: "General".into(),
        }, now).await.unwrap();
        let workspace = setup.workspace_id.to_string();
        assert!(request(&database, &workspace, "polling", 5).await.unwrap());
        assert!(request(&database, &workspace, "webhooks", 5).await.unwrap());
        let claim = github_poll_schedule::try_claim(&database, &workspace, "worker", 600_000).await.unwrap().unwrap();
        assert!(request(&database, &workspace, "polling", 5).await.unwrap());
        assert!(!complete_applied(&database, &claim, &PollOutcome::default()).await.unwrap());
        let mode: String = sqlx::query_scalar("SELECT sync_mode FROM github_workspace_settings WHERE workspace_id = $1").bind(&workspace).fetch_one(database.pool()).await.unwrap();
        assert_eq!(mode, "polling");
    }
}

/// Called only after every action and discovery update has been applied successfully
/// under `claim`'s fence. Incomplete GitHub responses cannot complete a handoff.
/// Returns false if the request, configuration, mode epoch, or lease changed.
pub(crate) async fn complete_applied(
    database: &Database,
    claim: &Claim,
    outcome: &PollOutcome,
) -> Result<bool, sqlx::Error> {
    if !outcome.errors.is_empty() {
        return Ok(false);
    }
    let mut tx = database.immediate_transaction().await?;
    if !github_poll_schedule::is_current(&mut tx, database.backend(), claim).await? {
        tx.commit().await?;
        return Ok(false);
    }
    let transition = sqlx::query("SELECT target_mode, config_version, mode_epoch, minimum_generation, poll_interval_minutes FROM github_poll_transition WHERE workspace_id = $1")
        .bind(&claim.workspace_id).fetch_optional(&mut *tx).await?;
    let Some(transition) = transition else {
        tx.commit().await?;
        return Ok(false);
    };
    if transition.get::<i64, _>("config_version") != claim.config_version
        || transition.get::<i64, _>("mode_epoch") != claim.mode_epoch
        || claim.generation < transition.get::<i64, _>("minimum_generation")
    {
        tx.commit().await?;
        return Ok(false);
    }
    let target: String = transition.get("target_mode");
    if target == "webhooks" {
        sqlx::query("UPDATE github_workspace_settings SET sync_mode = 'webhooks', poll_interval_minutes = $1, config_version = config_version + 1, mode_epoch = mode_epoch + 1 WHERE workspace_id = $2")
            .bind(transition.get::<i64, _>("poll_interval_minutes")).bind(&claim.workspace_id).execute(&mut *tx).await?;
        sqlx::query("UPDATE github_poll_state SET lease_owner = NULL, lease_expires_at = NULL, next_poll_at = NULL WHERE workspace_id = $1 AND generation = $2 AND lease_owner = $3")
            .bind(&claim.workspace_id).bind(claim.generation).bind(&claim.owner).execute(&mut *tx).await?;
    }
    sqlx::query("DELETE FROM github_poll_transition WHERE workspace_id = $1")
        .bind(&claim.workspace_id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(true)
}
