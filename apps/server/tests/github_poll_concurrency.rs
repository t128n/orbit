use std::time::Duration;

use orbit_platform::{Backend, Id, PasswordService, TestDatabase, TimestampMillis};
use orbit_server::github_poll_schedule::{self, Outcome, RateLimit};
use orbit_server::repositories::identity::{IdentityRepository, SetupRequest};
use orbit_server::repositories::tasks::{GithubWorkItem, PollFence, TaskRepository};

struct Fixture {
    database: TestDatabase,
    workspace: Id,
    user: Id,
    project: Id,
}

async fn fixture() -> Fixture {
    let database = TestDatabase::new().await.unwrap();
    assert_eq!(
        database.backend(),
        Backend::Postgres,
        "run with ORBIT_TEST_POSTGRES_URL"
    );
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
    sqlx::query(
        "INSERT INTO github_workspace_settings (workspace_id, sync_mode) VALUES ($1, 'polling')",
    )
    .bind(setup.workspace_id.to_string())
    .execute(database.pool())
    .await
    .unwrap();
    Fixture {
        database,
        workspace: setup.workspace_id,
        user: setup.user_id,
        project: setup.project_id,
    }
}

fn item(project_id: Id) -> GithubWorkItem {
    GithubWorkItem {
        repository: "acme/repo".into(),
        number: 42,
        project_id,
        title: "Observed".into(),
        description: String::new(),
        kind: "issue",
        state: "open",
        state_changed: false,
    }
}

#[tokio::test]
async fn competing_claims_and_expired_generation_are_fenced() {
    let f = fixture().await;
    let workspace = f.workspace.to_string();
    let (a, b) = tokio::join!(
        github_poll_schedule::try_claim(&f.database, &workspace, "a", 600_000),
        github_poll_schedule::try_claim(&f.database, &workspace, "b", 600_000),
    );
    let claims: Vec<_> = [a.unwrap(), b.unwrap()].into_iter().flatten().collect();
    assert_eq!(claims.len(), 1, "only one simultaneous claimant may win");
    let old = &claims[0];
    sqlx::query("UPDATE github_poll_state SET lease_expires_at = 0 WHERE workspace_id = $1")
        .bind(&workspace)
        .execute(f.database.pool())
        .await
        .unwrap();
    let current = github_poll_schedule::try_claim(&f.database, &workspace, "replacement", 600_000)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(current.generation, old.generation + 1);
    assert!(
        !github_poll_schedule::renew(&f.database, old, 600_000)
            .await
            .unwrap()
    );
    assert!(
        !github_poll_schedule::release(&f.database, old)
            .await
            .unwrap()
    );
    let outcome = Outcome {
        success: true,
        status: "ok",
        result: None,
        items_seen: 1,
        items_updated: 1,
        rate_limit: RateLimit::default(),
    };
    assert!(
        !github_poll_schedule::finish(&f.database, old, &outcome)
            .await
            .unwrap()
    );
    let tasks = TaskRepository::new((*f.database).clone());
    assert!(
        tasks
            .observe_github_work_item(
                f.workspace,
                f.user,
                item(f.project),
                "old",
                TimestampMillis::now(),
                &PollFence::from(old)
            )
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        tasks
            .observe_github_work_item(
                f.workspace,
                f.user,
                item(f.project),
                "new",
                TimestampMillis::now(),
                &PollFence::from(&current)
            )
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        github_poll_schedule::finish(&f.database, &current, &outcome)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn mode_transition_serializes_claim_and_epoch_fences_old_poll() {
    let f = fixture().await;
    let workspace = f.workspace.to_string();
    let claim = github_poll_schedule::try_claim(&f.database, &workspace, "worker", 600_000)
        .await
        .unwrap()
        .unwrap();
    let mut tx = f.database.immediate_transaction().await.unwrap();
    sqlx::query("UPDATE github_workspace_settings SET sync_mode = 'webhooks', mode_epoch = mode_epoch + 1 WHERE workspace_id = $1")
        .bind(&workspace).execute(&mut *tx).await.unwrap();
    let database = (*f.database).clone();
    let task_workspace = workspace.clone();
    let mut blocked = tokio::spawn(async move {
        github_poll_schedule::try_claim(&database, &task_workspace, "contender", 600_000)
            .await
            .unwrap()
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(150), &mut blocked)
            .await
            .is_err(),
        "claim must wait for mode writer"
    );
    tx.commit().await.unwrap();
    assert!(
        blocked.await.unwrap().is_none(),
        "webhook mode must not start a poll"
    );
    let tasks = TaskRepository::new((*f.database).clone());
    let fence = PollFence::from(&claim);
    assert!(
        tasks
            .observe_github_work_item(
                f.workspace,
                f.user,
                item(f.project),
                "stale",
                TimestampMillis::now(),
                &fence
            )
            .await
            .unwrap()
            .is_none()
    );
    // Keep config_version and lease unchanged to isolate the epoch's ABA protection.
    sqlx::query("UPDATE github_workspace_settings SET sync_mode = 'polling', mode_epoch = mode_epoch + 1 WHERE workspace_id = $1")
        .bind(&workspace).execute(f.database.pool()).await.unwrap();
    assert!(
        !github_poll_schedule::renew(&f.database, &claim, 600_000)
            .await
            .unwrap()
    );
    assert!(
        tasks
            .observe_github_work_item(
                f.workspace,
                f.user,
                item(f.project),
                "stale",
                TimestampMillis::now(),
                &fence
            )
            .await
            .unwrap()
            .is_none()
    );
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM github_issue_links WHERE workspace_id = $1")
            .bind(&workspace)
            .fetch_one(f.database.pool())
            .await
            .unwrap();
    assert_eq!(count, 0);
    let mut current = fence;
    current.mode_epoch += 2;
    assert!(
        tasks
            .observe_github_work_item(
                f.workspace,
                f.user,
                item(f.project),
                "current",
                TimestampMillis::now(),
                &current
            )
            .await
            .unwrap()
            .is_some()
    );
}
