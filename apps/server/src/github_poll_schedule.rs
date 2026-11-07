//! Durable per-workspace GitHub polling leases and scheduling.
//!
//! Times are Unix milliseconds. All writers, including configuration writers and poll
//! result writers, must use `Database::immediate_transaction`. A lease is not a mutex
//! over an HTTP request: an expired worker can still be running. Callers must stop work
//! on failed renewal and fence every persisted effect in the *same transaction* as that
//! effect using `is_current`; remote side effects cannot be fenced by this module.

use orbit_platform::{Backend, Database, Row};
use sqlx::{Any, Transaction};

#[derive(Clone, Debug)]
pub struct Claim {
    pub workspace_id: String,
    pub owner: String,
    pub generation: i64,
    pub config_version: i64,
    pub mode_epoch: i64,
}

/// Header values already parsed by the HTTP client. Reset is an absolute Unix time in
/// seconds; retry-after is a duration in seconds (or an HTTP-date converted by the caller).
#[derive(Clone, Copy, Debug, Default)]
pub struct RateLimit {
    pub remaining: Option<u64>,
    pub reset_epoch_seconds: Option<i64>,
    pub retry_after_seconds: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct Outcome<'a> {
    pub success: bool,
    pub status: &'a str,
    pub result: Option<&'a str>,
    pub items_seen: i64,
    pub items_updated: i64,
    pub rate_limit: RateLimit,
}

async fn now(tx: &mut Transaction<'_, Any>, backend: Backend) -> Result<i64, sqlx::Error> {
    let sql = match backend {
        Backend::Sqlite => {
            "SELECT CAST(strftime('%s', 'now') AS INTEGER) * 1000 + CAST(substr(strftime('%f', 'now'), 4, 3) AS INTEGER)"
        }
        Backend::Postgres => "SELECT (EXTRACT(EPOCH FROM clock_timestamp()) * 1000)::BIGINT",
    };
    sqlx::query_scalar(sql).fetch_one(&mut **tx).await
}

// Stable per generation, avoiding synchronized polls without needing a random dependency.
fn interval_ms(workspace: &str, generation: i64, minutes: i64) -> i64 {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in workspace.bytes().chain(generation.to_le_bytes()) {
        hash = (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3);
    }
    let base = minutes.clamp(1, 15) * 60_000;
    // ±10%, bounded by the configured scheduler's 1–15 minute limits.
    let spread = base / 10;
    (base - spread + (hash % (2 * spread + 1) as u64) as i64).clamp(60_000, 900_000)
}

fn deadline(now: i64, limit: RateLimit) -> Option<i64> {
    let reset = if limit.remaining == Some(0) {
        limit.reset_epoch_seconds.map(|s| s.saturating_mul(1000))
    } else {
        None
    };
    let retry = limit
        .retry_after_seconds
        .map(|s| now.saturating_add(i64::try_from(s).unwrap_or(i64::MAX).saturating_mul(1000)));
    reset.into_iter().chain(retry).filter(|v| *v > now).max()
}

// `next_poll_at` otherwise contains a nonnegative Unix timestamp. This durable
// marker distinguishes a coalesced manual request from a normal due poll, even
// across restarts and while a lease is active.
const SYNC_NOW_PENDING: i64 = -1;
const MANUAL_MIN_SPACING_MS: i64 = 60_000;

fn due(
    now: i64,
    next: Option<i64>,
    backoff: Option<i64>,
    last_started: Option<i64>,
    same_version: bool,
) -> bool {
    if next == Some(SYNC_NOW_PENDING) {
        // A manual request bypasses Orbit backoff, but cannot cause a tight retry
        // loop when repeatedly submitted. GitHub's deadline is checked separately.
        return last_started
            .is_none_or(|started| now >= started.saturating_add(MANUAL_MIN_SPACING_MS));
    }
    !same_version || (!backoff.is_some_and(|v| v > now) && !next.is_some_and(|v| v > now))
}

/// Try to claim a particular due workspace. `owner` must identify this worker instance,
/// not merely the server; use a fresh unpredictable ID on every process start.
/// `lease_ms` must be positive. Expired claims can be taken over with a higher fence.
pub async fn try_claim(
    database: &Database,
    workspace: &str,
    owner: &str,
    lease_ms: i64,
) -> Result<Option<Claim>, sqlx::Error> {
    if owner.is_empty() || lease_ms <= 0 {
        return Err(sqlx::Error::Protocol("invalid GitHub polling lease".into()));
    }
    let mut tx = database.immediate_transaction().await?;
    let now = now(&mut tx, database.backend()).await?;
    let settings = sqlx::query(
        "SELECT sync_mode, config_version, mode_epoch FROM github_workspace_settings WHERE workspace_id = $1",
    )
    .bind(workspace)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(settings) = settings else {
        tx.commit().await?;
        return Ok(None);
    };
    let mode: String = settings.get("sync_mode");
    let version: i64 = settings.get("config_version");
    let mode_epoch: i64 = settings.get("mode_epoch");
    if mode != "polling" {
        tx.commit().await?;
        return Ok(None);
    }
    sqlx::query("INSERT INTO github_poll_state (workspace_id) VALUES ($1) ON CONFLICT (workspace_id) DO NOTHING")
        .bind(workspace).execute(&mut *tx).await?;
    let row = sqlx::query("SELECT generation, config_version, lease_expires_at, next_poll_at, rate_limit_not_before, backoff_until, last_started_at FROM github_poll_state WHERE workspace_id = $1")
        .bind(workspace).fetch_one(&mut *tx).await?;
    let expires: Option<i64> = row.try_get("lease_expires_at")?;
    let next: Option<i64> = row.try_get("next_poll_at")?;
    let rate: Option<i64> = row.try_get("rate_limit_not_before")?;
    let backoff: Option<i64> = row.try_get("backoff_until")?;
    let last_started: Option<i64> = row.try_get("last_started_at")?;
    let old_version: i64 = row.get("config_version");
    if expires.is_some_and(|v| v > now)
        || rate.is_some_and(|v| v > now)
        || !due(now, next, backoff, last_started, old_version == version)
    {
        tx.commit().await?;
        return Ok(None);
    }
    let generation: i64 = row.get("generation");
    let generation = generation
        .checked_add(1)
        .ok_or_else(|| sqlx::Error::Protocol("GitHub polling generation exhausted".into()))?;
    sqlx::query("UPDATE github_poll_state SET generation = $1, config_version = $2, lease_owner = $3, lease_expires_at = $4, next_poll_at = NULL, last_started_at = $5 WHERE workspace_id = $6")
        .bind(generation).bind(version).bind(owner).bind(now.saturating_add(lease_ms)).bind(now).bind(workspace)
        .execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(Some(Claim {
        workspace_id: workspace.into(),
        owner: owner.into(),
        generation,
        config_version: version,
        mode_epoch,
    }))
}

/// Verify a fence inside the same immediate transaction as the caller's writes.
/// Configuration changes invalidate old claims even if the lease has not expired.
pub async fn is_current(
    tx: &mut Transaction<'_, Any>,
    backend: Backend,
    claim: &Claim,
) -> Result<bool, sqlx::Error> {
    let now = now(tx, backend).await?;
    let row = sqlx::query("SELECT s.generation, s.config_version, s.lease_owner, s.lease_expires_at, c.config_version AS current_version, c.mode_epoch, c.sync_mode FROM github_poll_state s JOIN github_workspace_settings c ON c.workspace_id = s.workspace_id WHERE s.workspace_id = $1")
        .bind(&claim.workspace_id).fetch_optional(&mut **tx).await?;
    Ok(row.is_some_and(|r| {
        r.get::<i64, _>("generation") == claim.generation
            && r.get::<i64, _>("config_version") == claim.config_version
            && r.get::<i64, _>("current_version") == claim.config_version
            && r.get::<i64, _>("mode_epoch") == claim.mode_epoch
            && r.get::<String, _>("sync_mode") == "polling"
            && r.get::<Option<String>, _>("lease_owner").as_deref() == Some(&claim.owner)
            && r.get::<Option<i64>, _>("lease_expires_at")
                .is_some_and(|v| v > now)
    }))
}

/// Renew only a still-valid claim; a lease cannot be resurrected after expiration.
pub async fn renew(database: &Database, claim: &Claim, lease_ms: i64) -> Result<bool, sqlx::Error> {
    if lease_ms <= 0 {
        return Err(sqlx::Error::Protocol("invalid GitHub polling lease".into()));
    }
    let mut tx = database.immediate_transaction().await?;
    if !is_current(&mut tx, database.backend(), claim).await? {
        tx.commit().await?;
        return Ok(false);
    }
    let now = now(&mut tx, database.backend()).await?;
    // Recheck against the later timestamp, so a lease that expired during validation is not revived.
    let updated = sqlx::query("UPDATE github_poll_state SET lease_expires_at = $1 WHERE workspace_id = $2 AND generation = $3 AND lease_owner = $4 AND lease_expires_at > $5")
        .bind(now.saturating_add(lease_ms)).bind(&claim.workspace_id).bind(claim.generation).bind(&claim.owner).bind(now)
        .execute(&mut *tx).await?.rows_affected();
    tx.commit().await?;
    Ok(updated == 1)
}

/// Coalesce requests into one pending poll, including while a poll is active. Manual
/// requests bypass Orbit backoff, but respect rate limits and minimum start spacing.
/// Returns false when polling is not configured.
pub async fn sync_now(database: &Database, workspace: &str) -> Result<bool, sqlx::Error> {
    let mut tx = database.immediate_transaction().await?;
    let settings =
        sqlx::query("SELECT sync_mode FROM github_workspace_settings WHERE workspace_id = $1")
            .bind(workspace)
            .fetch_optional(&mut *tx)
            .await?;
    if !settings.is_some_and(|r| r.get::<String, _>("sync_mode") == "polling") {
        tx.commit().await?;
        return Ok(false);
    }
    sqlx::query("INSERT INTO github_poll_state (workspace_id, next_poll_at) VALUES ($1, $2) ON CONFLICT (workspace_id) DO UPDATE SET next_poll_at = $2")
        .bind(workspace).bind(SYNC_NOW_PENDING).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(true)
}

/// Complete and release a valid lease. Call only after the poll's remote work has ended.
/// A stale claim releases only its own generation, without recording its outcome.
/// An outstanding sync-now request is retained.
/// Failure backoff doubles from one minute, capped at fifteen minutes. A successful
/// response may still impose a rate limit; reset/retry-after always take precedence.
pub async fn finish(
    database: &Database,
    claim: &Claim,
    outcome: &Outcome<'_>,
) -> Result<bool, sqlx::Error> {
    let mut tx = database.immediate_transaction().await?;
    if !is_current(&mut tx, database.backend(), claim).await? {
        let now = now(&mut tx, database.backend()).await?;
        sqlx::query("UPDATE github_poll_state SET lease_owner = NULL, lease_expires_at = NULL, next_poll_at = COALESCE(next_poll_at, $1) WHERE workspace_id = $2 AND generation = $3 AND lease_owner = $4")
            .bind(now).bind(&claim.workspace_id).bind(claim.generation).bind(&claim.owner)
            .execute(&mut *tx).await?;
        tx.commit().await?;
        return Ok(false);
    }
    let now = now(&mut tx, database.backend()).await?;
    let row = sqlx::query("SELECT s.consecutive_failures, s.next_poll_at, s.rate_limit_not_before, c.poll_interval_minutes FROM github_poll_state s JOIN github_workspace_settings c ON c.workspace_id = s.workspace_id WHERE s.workspace_id = $1")
        .bind(&claim.workspace_id).fetch_one(&mut *tx).await?;
    let failures: i64 = row.get("consecutive_failures");
    let failures = if outcome.success {
        0
    } else {
        failures.saturating_add(1)
    };
    let backoff = if outcome.success {
        None
    } else {
        Some(now.saturating_add(
            (60_000_i64 * (1_i64 << failures.saturating_sub(1).clamp(0, 4))).min(900_000),
        ))
    };
    let old_rate: Option<i64> = row.try_get("rate_limit_not_before")?;
    let rate = old_rate
        .filter(|v| *v > now)
        .into_iter()
        .chain(deadline(now, outcome.rate_limit))
        .max();
    let pending: Option<i64> = row.try_get("next_poll_at")?;
    let minutes: i64 = row.get("poll_interval_minutes");
    let next = pending.unwrap_or_else(|| {
        backoff.unwrap_or_else(|| {
            now.saturating_add(interval_ms(&claim.workspace_id, claim.generation, minutes))
        })
    });
    let updated = sqlx::query("UPDATE github_poll_state SET lease_owner = NULL, lease_expires_at = NULL, next_poll_at = $1, rate_limit_not_before = $2, backoff_until = $3, consecutive_failures = $4, last_finished_at = $5, last_status = $6, last_result = $7, last_items_seen = $8, last_items_updated = $9 WHERE workspace_id = $10 AND generation = $11 AND lease_owner = $12 AND lease_expires_at > $5")
        .bind(next).bind(rate).bind(backoff).bind(failures).bind(now).bind(outcome.status).bind(outcome.result)
        .bind(outcome.items_seen).bind(outcome.items_updated).bind(&claim.workspace_id).bind(claim.generation).bind(&claim.owner)
        .execute(&mut *tx).await?.rows_affected();
    tx.commit().await?;
    Ok(updated == 1)
}

/// Abandon a claim without recording success or failure. A newer generation is never released.
/// Configuration changes do not prevent the old owner from releasing its own lease.
pub async fn release(database: &Database, claim: &Claim) -> Result<bool, sqlx::Error> {
    let mut tx = database.immediate_transaction().await?;
    let now = now(&mut tx, database.backend()).await?;
    let updated = sqlx::query("UPDATE github_poll_state SET lease_owner = NULL, lease_expires_at = NULL, next_poll_at = COALESCE(next_poll_at, $1) WHERE workspace_id = $2 AND generation = $3 AND lease_owner = $4")
        .bind(now).bind(&claim.workspace_id).bind(claim.generation).bind(&claim.owner).execute(&mut *tx).await?.rows_affected();
    tx.commit().await?;
    Ok(updated == 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_requests_bypass_backoff_but_are_spaced() {
        let now = 1_000_000;
        assert!(due(
            now,
            Some(SYNC_NOW_PENDING),
            Some(now + 900_000),
            None,
            true
        ));
        assert!(!due(
            now,
            Some(SYNC_NOW_PENDING),
            Some(now + 900_000),
            Some(now - 59_999),
            true
        ));
        assert!(due(
            now,
            Some(SYNC_NOW_PENDING),
            Some(now + 900_000),
            Some(now - 60_000),
            true
        ));
        assert!(!due(now, Some(now - 1), Some(now + 1), None, true));
        assert!(!due(now, Some(now + 1), None, None, true));
        assert!(due(now, Some(now + 1), None, None, false));
        assert!(due(now, Some(now + 1), Some(now + 1), None, false));
    }

    #[test]
    fn jitter_is_bounded_and_varies() {
        for minutes in 1..=15 {
            for generation in 0..100 {
                assert!((60_000..=900_000).contains(&interval_ms(
                    "workspace",
                    generation,
                    minutes
                )));
            }
        }
        assert_ne!(
            interval_ms("workspace", 1, 5),
            interval_ms("workspace", 2, 5)
        );
    }

    #[test]
    fn rate_limit_uses_latest_applicable_deadline() {
        let now = 1_000_000;
        assert_eq!(
            deadline(
                now,
                RateLimit {
                    remaining: Some(0),
                    reset_epoch_seconds: Some(1100),
                    retry_after_seconds: Some(120)
                }
            ),
            Some(1_120_000)
        );
        assert_eq!(
            deadline(
                now,
                RateLimit {
                    remaining: Some(1),
                    reset_epoch_seconds: Some(1100),
                    retry_after_seconds: None
                }
            ),
            None
        );
        assert_eq!(
            deadline(
                now,
                RateLimit {
                    remaining: Some(0),
                    reset_epoch_seconds: Some(900),
                    retry_after_seconds: None
                }
            ),
            None
        );
    }
}
