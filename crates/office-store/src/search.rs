use std::collections::HashSet;

use office_domain::{Action, BearerDigest, ChannelId, MessageId, NoteId};
use office_protocol::{Search, SearchHit, SearchResult, SearchSource};
use uuid::Uuid;

use crate::{
    Store,
    data_scope::authorize_scope,
    identity::{IdentityError, workspace_context},
};

#[derive(sqlx::FromRow)]
struct HitRow {
    id: Uuid,
    scope_id: Uuid,
    thread_id: Option<Uuid>,
    revision: Option<i64>,
    title: Option<String>,
    excerpt: String,
    score: f32,
}

impl Store {
    pub async fn search(
        &self,
        digest: &BearerDigest,
        request: Search,
    ) -> Result<SearchResult, IdentityError> {
        let limit = request.limit.unwrap_or(20);
        let query = request.query.trim();
        if query.is_empty()
            || query.len() > 256
            || query.contains('\0')
            || !(1..=50).contains(&limit)
        {
            return Err(IdentityError::InvalidInput);
        }
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, request.workspace_id).await?;
        let mut rows = sqlx::query_as::<_, HitRow>(
            "WITH terms AS MATERIALIZED (SELECT websearch_to_tsquery('simple', $2) AS query),
             visible AS MATERIALIZED (
                 SELECT n.id, n.id AS scope_id, NULL::uuid AS thread_id, r.revision, r.title,
                        r.body, r.document, r.created_at
                 FROM notes n JOIN note_revisions r ON r.workspace_id = n.workspace_id
                    AND r.note_id = n.id AND r.revision = n.current_revision
                 CROSS JOIN terms
                 WHERE n.workspace_id = $1 AND r.document @@ terms.query
                 UNION ALL
                 SELECT m.id, m.channel_id AS scope_id, m.thread_id, NULL::bigint AS revision,
                        NULL::text AS title, m.body, m.document, m.created_at
                 FROM messages m CROSS JOIN terms
                 WHERE m.workspace_id = $1 AND m.document @@ terms.query
             ),
             ranked AS MATERIALIZED (
                 SELECT visible.*, ts_rank_cd(visible.document, terms.query) AS score FROM visible CROSS JOIN terms
                 ORDER BY score DESC, created_at DESC, id DESC LIMIT $3
             )
             SELECT ranked.id, scope_id, thread_id, revision, title, score,
                    ts_headline('simple', body, terms.query,
                       'StartSel=[[, StopSel=]], MinWords=6, MaxWords=24, MaxFragments=2') AS excerpt
             FROM ranked CROSS JOIN terms ORDER BY score DESC, created_at DESC, id DESC",
        ).bind(request.workspace_id.0).bind(query).bind(i64::from(limit) + 1).fetch_all(&mut *tx).await?;
        let truncated = rows.len() > usize::from(limit);
        rows.truncate(usize::from(limit));
        let mut checked = HashSet::new();
        let mut hits = Vec::with_capacity(rows.len());
        for row in rows {
            if checked.insert(row.scope_id) {
                authorize_scope(&mut tx, context, row.scope_id, Action::ReadData).await?;
            }
            let source = match (row.revision, row.thread_id) {
                (Some(revision), None) => SearchSource::Note {
                    note_id: NoteId(row.id),
                    revision,
                },
                (None, Some(thread)) => SearchSource::Message {
                    message_id: MessageId(row.id),
                    channel_id: ChannelId(row.scope_id),
                    thread_id: MessageId(thread),
                },
                _ => return Err(IdentityError::InvalidInput),
            };
            hits.push(SearchHit {
                source,
                title: row.title,
                excerpt: row.excerpt,
                score: row.score,
            });
        }
        tx.commit().await?;
        Ok(SearchResult { hits, truncated })
    }
}
