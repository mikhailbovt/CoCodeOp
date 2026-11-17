use office_domain::BearerDigest;
use office_protocol::{OperationsMetrics, ReadOperationsMetrics};
use sqlx::types::Json;

use crate::{
    Store,
    identity::{IdentityError, workspace_context},
};

impl Store {
    pub async fn operations_metrics(
        &self,
        digest: &BearerDigest,
        request: ReadOperationsMetrics,
    ) -> Result<(OperationsMetrics, bool), IdentityError> {
        if !(60..=604_800).contains(&request.window_seconds) {
            return Err(IdentityError::InvalidInput);
        }
        let mut tx = self.pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
            .execute(&mut *tx)
            .await?;
        let context = workspace_context(&mut tx, digest, request.workspace_id).await?;
        if context.agent_id().is_some() {
            return Err(IdentityError::Forbidden);
        }
        sqlx::query("SELECT set_config('office.metrics_read', 'on', true)")
            .execute(&mut *tx)
            .await?;
        let owner = context.access.member.0;
        let operator = sqlx::query_scalar::<_, bool>(
            "SELECT COALESCE(bootstrap_user_id=$1, false) FROM office_installation WHERE singleton",
        )
        .bind(owner)
        .fetch_one(&mut *tx)
        .await?;
        let report = sqlx::query_scalar::<_, Json<OperationsMetrics>>(include_str!(
            "operations_metrics.sql"
        ))
        .bind(request.workspace_id.0)
        .bind(owner)
        .bind(f64::from(request.window_seconds))
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok((report.0, operator))
    }
}
