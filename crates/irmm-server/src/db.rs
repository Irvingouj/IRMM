//! Every query runs inside a transaction that has dropped to the `irmm_app` role,
//! so Postgres row-level security is the last line of tenant isolation.

use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

pub type Tx = Transaction<'static, Postgres>;

/// Transaction scoped to one tenant.
pub async fn tenant_tx(pool: &PgPool, tenant: Uuid) -> sqlx::Result<Tx> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL ROLE irmm_app").execute(&mut *tx).await?;
    sqlx::query("SELECT set_config('app.tenant_id', $1, true)")
        .bind(tenant.to_string())
        .execute(&mut *tx)
        .await?;
    Ok(tx)
}

/// Transaction for cross-tenant system lookups (login by email, agent by node id).
pub async fn system_tx(pool: &PgPool) -> sqlx::Result<Tx> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL ROLE irmm_app").execute(&mut *tx).await?;
    sqlx::query("SELECT set_config('app.bypass', 'on', true)").execute(&mut *tx).await?;
    Ok(tx)
}

pub async fn audit(
    tx: &mut Tx,
    tenant: Uuid,
    user: Option<(Uuid, &str)>,
    action: &str,
    target: Option<String>,
    detail: serde_json::Value,
) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO audit_log (tenant_id, user_id, actor_email, action, target, detail) VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(tenant)
    .bind(user.map(|u| u.0))
    .bind(user.map(|u| u.1.to_owned()))
    .bind(action)
    .bind(target)
    .bind(detail)
    .execute(&mut **tx)
    .await?;
    Ok(())
}
