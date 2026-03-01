use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use ulid::Ulid;

use crate::activity::ActivityDraft;
use crate::types::{
    ActivityItem, EntitySnapshot, EntityType, IndexedEntity, NoteDetail, NoteItem, ParsedEntity,
    ProjectItem, ProjectSourceKind, SearchFilters, SearchResult, TaskFilters, TaskItem,
    TaskPriority, TaskStatus, TaskSyncKind,
};

#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
}

#[derive(Debug, Clone)]
pub struct StoredEntityRecord {
    pub id: String,
    pub entity_type: EntityType,
    pub title: String,
    pub path: PathBuf,
    pub revision: String,
    pub archived: bool,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed creating db parent {}", parent.display()))?;
        }

        let conn = Connection::open(path)
            .with_context(|| format!("failed opening sqlite db at {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(Duration::from_millis(2_000))?;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub fn run_migrations(&self) -> Result<()> {
        self.with_conn_mut(|conn| {
            conn.execute_batch(
                r#"
                CREATE TABLE IF NOT EXISTS schema_migrations (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    name TEXT NOT NULL UNIQUE,
                    applied_at TEXT NOT NULL
                );
                "#,
            )?;

            for (name, sql) in MIGRATIONS {
                let applied: Option<String> = conn
                    .query_row(
                        "SELECT name FROM schema_migrations WHERE name = ?1 LIMIT 1",
                        params![name],
                        |row| row.get(0),
                    )
                    .optional()?;
                if applied.is_some() {
                    continue;
                }

                apply_migration(conn, name, sql)
                    .with_context(|| format!("failed applying migration {name}"))?;
                conn.execute(
                    "INSERT INTO schema_migrations (name, applied_at) VALUES (?1, ?2)",
                    params![name, Utc::now().to_rfc3339()],
                )?;
            }
            Ok(())
        })
    }

    pub fn list_indexed_paths(&self) -> Result<Vec<String>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare("SELECT path FROM files")?;
            let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
    }

    pub fn remove_by_path(&self, path: &Path) -> Result<()> {
        self.remove_paths(&[path.to_path_buf()])
    }

    pub fn remove_paths(&self, paths: &[PathBuf]) -> Result<()> {
        self.with_conn_mut(|conn| {
            let tx = conn.transaction()?;
            for path in paths {
                let path = path.to_string_lossy().to_string();
                remove_by_path_tx(&tx, &path)?;
            }
            tx.commit()?;
            Ok(())
        })
    }

    pub fn upsert_indexed_entity(&self, entity: &IndexedEntity) -> Result<()> {
        self.upsert_indexed_entities(std::slice::from_ref(entity))
    }

    pub fn upsert_indexed_entities(&self, entities: &[IndexedEntity]) -> Result<()> {
        self.with_conn_mut(|conn| {
            let tx = conn.transaction()?;
            for entity in entities {
                upsert_indexed_entity_tx(&tx, entity)?;
            }
            tx.commit()?;
            Ok(())
        })
    }

    pub fn get_entity_record(&self, id_or_path: &str) -> Result<Option<StoredEntityRecord>> {
        self.with_conn(|conn| {
            conn.query_row(
                r#"
                SELECT id, entity_type, title, path, revision, archived
                FROM entities
                WHERE id = ?1 OR path = ?1
                LIMIT 1
                "#,
                params![id_or_path],
                |row| {
                    let entity_type_str: String = row.get(1)?;
                    let entity_type = parse_entity_type(&entity_type_str).map_err(to_sql_err)?;
                    Ok(StoredEntityRecord {
                        id: row.get(0)?,
                        entity_type,
                        title: row.get(2)?,
                        path: PathBuf::from(row.get::<_, String>(3)?),
                        revision: row.get(4)?,
                        archived: row.get::<_, i64>(5)? != 0,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
        })
    }

    pub fn read_entity_snapshot(&self, id_or_path: &str) -> Result<Option<EntitySnapshot>> {
        let Some(record) = self.get_entity_record(id_or_path)? else {
            return Ok(None);
        };

        let raw = std::fs::read_to_string(&record.path)
            .with_context(|| format!("failed reading {}", record.path.display()))?;
        let parsed = crate::markdown::parse_entity_markdown(&raw)?;

        Ok(Some(EntitySnapshot {
            id: record.id,
            entity_type: record.entity_type,
            title: record.title,
            path: record.path.to_string_lossy().to_string(),
            body: parsed.body,
            frontmatter: parsed.frontmatter,
            revision: parsed.revision,
            archived: record.archived,
        }))
    }

    pub fn list_tasks(&self, filters: &TaskFilters, default_limit: usize) -> Result<Vec<TaskItem>> {
        let status = filters.status.as_ref().map(TaskStatus::as_str);
        let priority = filters.priority.as_ref().map(TaskPriority::as_str);
        let limit = filters.limit.unwrap_or(default_limit) as i64;

        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                r#"
                SELECT id, title, project_id, status, priority, assignee, due_at, created_at,
                       sort_order, completed_at, sync_kind, sync_path, sync_key, sync_managed,
                       path, updated_at, revision, archived
                FROM tasks
                WHERE (?1 IS NULL OR status = ?1)
                  AND (?2 IS NULL OR priority = ?2)
                  AND (?3 IS NULL OR project_id = ?3)
                  AND (?4 IS NULL OR assignee = ?4)
                  AND (?5 = 1 OR archived = 0)
                ORDER BY created_at ASC
                LIMIT ?6
                "#,
            )?;

            let rows = stmt.query_map(
                params![
                    status,
                    priority,
                    filters.project_id,
                    filters.assignee,
                    if filters.include_archived { 1 } else { 0 },
                    limit
                ],
                |row| {
                    let status =
                        parse_task_status(&row.get::<_, String>(3)?).map_err(to_sql_err)?;
                    let priority =
                        parse_task_priority(&row.get::<_, String>(4)?).map_err(to_sql_err)?;
                    let due_at = row
                        .get::<_, Option<String>>(6)?
                        .and_then(|v| DateTime::parse_from_rfc3339(&v).ok())
                        .map(|v| v.with_timezone(&Utc));
                    let created_at = DateTime::parse_from_rfc3339(&row.get::<_, String>(7)?)
                        .map_err(|err| to_sql_err(anyhow::Error::new(err)))?
                        .with_timezone(&Utc);
                    let completed_at = row
                        .get::<_, Option<String>>(9)?
                        .and_then(|v| DateTime::parse_from_rfc3339(&v).ok())
                        .map(|v| v.with_timezone(&Utc));
                    let sync_kind = row
                        .get::<_, Option<String>>(10)?
                        .map(|value| parse_task_sync_kind(&value))
                        .transpose()
                        .map_err(to_sql_err)?;
                    let updated_at = DateTime::parse_from_rfc3339(&row.get::<_, String>(15)?)
                        .map_err(|err| to_sql_err(anyhow::Error::new(err)))?
                        .with_timezone(&Utc);

                    Ok(TaskItem {
                        id: row.get(0)?,
                        title: row.get(1)?,
                        project_id: row.get(2)?,
                        status,
                        priority,
                        assignee: row.get(5)?,
                        due_at,
                        created_at,
                        sort_order: row.get(8)?,
                        completed_at,
                        sync_kind,
                        sync_path: row.get(11)?,
                        sync_key: row.get(12)?,
                        sync_managed: row.get::<_, i64>(13)? != 0,
                        path: row.get(14)?,
                        updated_at,
                        revision: row.get(16)?,
                        archived: row.get::<_, i64>(17)? != 0,
                    })
                },
            )?;

            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
    }

    pub fn list_notes(&self, limit: usize, include_archived: bool) -> Result<Vec<NoteItem>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                r#"
                SELECT id, title, project_id, path, updated_at, revision, archived
                FROM notes
                WHERE (?1 = 1 OR archived = 0)
                ORDER BY updated_at DESC
                LIMIT ?2
                "#,
            )?;

            let rows = stmt.query_map(
                params![if include_archived { 1 } else { 0 }, limit as i64],
                |row| {
                    let updated_at = DateTime::parse_from_rfc3339(&row.get::<_, String>(4)?)
                        .map_err(|err| to_sql_err(anyhow::Error::new(err)))?
                        .with_timezone(&Utc);
                    Ok(NoteItem {
                        id: row.get(0)?,
                        title: row.get(1)?,
                        project_id: row.get(2)?,
                        path: row.get(3)?,
                        updated_at,
                        revision: row.get(5)?,
                        archived: row.get::<_, i64>(6)? != 0,
                    })
                },
            )?;

            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
    }

    pub fn list_note_details_for_project(
        &self,
        project_id: &str,
        limit: usize,
        include_archived: bool,
    ) -> Result<Vec<NoteDetail>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                r#"
                SELECT n.id, n.project_id, e.title, e.body, e.path, n.updated_at, n.revision, n.archived
                FROM notes n
                INNER JOIN entities e ON e.id = n.id
                WHERE n.project_id = ?1
                  AND (?2 = 1 OR n.archived = 0)
                ORDER BY n.updated_at DESC
                LIMIT ?3
                "#,
            )?;

            let rows = stmt.query_map(
                params![project_id, if include_archived { 1 } else { 0 }, limit as i64],
                |row| {
                    let updated_at = DateTime::parse_from_rfc3339(&row.get::<_, String>(5)?)
                        .map_err(|err| to_sql_err(anyhow::Error::new(err)))?
                        .with_timezone(&Utc);

                    Ok(NoteDetail {
                        id: row.get(0)?,
                        project_id: row.get(1)?,
                        title: row.get(2)?,
                        body: row.get(3)?,
                        path: row.get(4)?,
                        updated_at,
                        revision: row.get(6)?,
                        archived: row.get::<_, i64>(7)? != 0,
                    })
                },
            )?;

            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
    }

    pub fn list_projects(&self, limit: usize, include_archived: bool) -> Result<Vec<ProjectItem>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                r#"
                SELECT id, title, status, owner, source_kind, source_locator, sync_source_key,
                       last_synced_at, last_sync_summary, path, updated_at, revision, archived
                FROM projects
                WHERE (?1 = 1 OR archived = 0)
                ORDER BY updated_at DESC
                LIMIT ?2
                "#,
            )?;

            let rows = stmt.query_map(
                params![if include_archived { 1 } else { 0 }, limit as i64],
                |row| {
                    let updated_at = DateTime::parse_from_rfc3339(&row.get::<_, String>(10)?)
                        .map_err(|err| to_sql_err(anyhow::Error::new(err)))?
                        .with_timezone(&Utc);
                    let last_synced_at = row
                        .get::<_, Option<String>>(7)?
                        .and_then(|v| DateTime::parse_from_rfc3339(&v).ok())
                        .map(|v| v.with_timezone(&Utc));
                    Ok(ProjectItem {
                        id: row.get(0)?,
                        title: row.get(1)?,
                        status: row.get(2)?,
                        owner: row.get(3)?,
                        source_kind: row
                            .get::<_, Option<String>>(4)?
                            .map(|value| parse_project_source_kind(&value))
                            .transpose()
                            .map_err(to_sql_err)?,
                        source_locator: row.get(5)?,
                        sync_source_key: row.get(6)?,
                        last_synced_at,
                        last_sync_summary: row.get(8)?,
                        path: row.get(9)?,
                        updated_at,
                        revision: row.get(11)?,
                        archived: row.get::<_, i64>(12)? != 0,
                    })
                },
            )?;

            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
    }

    pub fn list_recent_activity(
        &self,
        since: Option<DateTime<Utc>>,
        limit: usize,
    ) -> Result<Vec<ActivityItem>> {
        let since = since.map(|v| v.to_rfc3339());

        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                r#"
                SELECT event_id, occurred_at, request_id, actor_kind, actor_id, action, entity_type,
                       entity_id, file_path, before_revision, after_revision, summary, git_branch, git_commit
                FROM activity_events
                WHERE (?1 IS NULL OR occurred_at >= ?1)
                ORDER BY occurred_at DESC
                LIMIT ?2
                "#,
            )?;

            let rows = stmt.query_map(params![since, limit as i64], |row| {
                let entity_type = row
                    .get::<_, Option<String>>(6)?
                    .and_then(|v| parse_entity_type(&v).ok());
                let occurred_at = DateTime::parse_from_rfc3339(&row.get::<_, String>(1)?)
                    .map_err(|err| to_sql_err(anyhow::Error::new(err)))?
                    .with_timezone(&Utc);

                Ok(ActivityItem {
                    event_id: row.get(0)?,
                    occurred_at,
                    request_id: row.get(2)?,
                    actor_kind: row.get(3)?,
                    actor_id: row.get(4)?,
                    action: row.get(5)?,
                    entity_type,
                    entity_id: row.get(7)?,
                    file_path: row.get(8)?,
                    before_revision: row.get(9)?,
                    after_revision: row.get(10)?,
                    summary: row.get(11)?,
                    git_branch: row.get(12)?,
                    git_commit: row.get(13)?,
                })
            })?;

            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
    }

    pub fn search_context(
        &self,
        query: &str,
        filters: &SearchFilters,
        limit: usize,
    ) -> Result<Vec<SearchResult>> {
        let trimmed = query.trim();
        if trimmed.is_empty() {
            return Ok(Vec::new());
        }
        let fts_query = sanitize_fts_query(trimmed);

        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                r#"
                SELECT e.id, e.entity_type, e.title, e.path, e.revision,
                       bm25(fts_documents) AS score,
                       snippet(fts_documents, 3, '', '', '…', 20) AS snippet
                FROM fts_documents
                JOIN entities e ON e.id = fts_documents.entity_id
                WHERE fts_documents MATCH ?1
                  AND (?2 IS NULL OR e.entity_type = ?2)
                  AND (?3 IS NULL OR e.project_id = ?3)
                  AND (?4 = 1 OR e.archived = 0)
                ORDER BY score
                LIMIT ?5
                "#,
            )?;

            let entity_type_filter = filters.entity_type.map(|v| v.as_str().to_string());

            let rows = stmt.query_map(
                params![
                    fts_query,
                    entity_type_filter,
                    filters.project_id,
                    if filters.include_archived { 1 } else { 0 },
                    limit as i64
                ],
                |row| {
                    let entity_type =
                        parse_entity_type(&row.get::<_, String>(1)?).map_err(to_sql_err)?;
                    Ok(SearchResult {
                        id: row.get(0)?,
                        entity_type,
                        title: row.get(2)?,
                        path: row.get(3)?,
                        revision: row.get(4)?,
                        score: row.get(5)?,
                        snippet: row.get::<_, Option<String>>(6)?.unwrap_or_default(),
                    })
                },
            )?;

            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
    }

    pub fn record_activity(&self, draft: ActivityDraft) -> Result<String> {
        self.record_activities(vec![draft])?
            .into_iter()
            .next()
            .context("batch activity insert returned no event ids")
    }

    pub fn record_activities(&self, drafts: Vec<ActivityDraft>) -> Result<Vec<String>> {
        self.with_conn_mut(|conn| {
            let tx = conn.transaction()?;
            let mut event_ids = Vec::with_capacity(drafts.len());
            for draft in drafts {
                let event_id = Ulid::new().to_string();
                insert_activity_tx(&tx, &event_id, draft)?;
                event_ids.push(event_id);
            }
            tx.commit()?;
            Ok(event_ids)
        })
    }

    pub fn all_entity_ids(&self) -> Result<Vec<String>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare("SELECT id FROM entities")?;
            let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
    }

    pub fn parse_entity_from_disk(
        &self,
        id_or_path: &str,
    ) -> Result<Option<(StoredEntityRecord, ParsedEntity)>> {
        let Some(record) = self.get_entity_record(id_or_path)? else {
            return Ok(None);
        };
        let raw = std::fs::read_to_string(&record.path)
            .with_context(|| format!("failed reading {}", record.path.display()))?;
        let parsed = crate::markdown::parse_entity_markdown(&raw)?;
        Ok(Some((record, parsed)))
    }

    fn with_conn<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Connection) -> Result<T>,
    {
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow::anyhow!("database mutex poisoned"))?;
        f(&conn)
    }

    fn with_conn_mut<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&mut Connection) -> Result<T>,
    {
        let mut conn = self
            .conn
            .lock()
            .map_err(|_| anyhow::anyhow!("database mutex poisoned"))?;
        f(&mut conn)
    }
}

fn remove_by_path_tx(tx: &rusqlite::Transaction<'_>, path: &str) -> Result<()> {
    let entity_id: Option<String> = tx
        .query_row(
            "SELECT entity_id FROM files WHERE path = ?1 LIMIT 1",
            params![path],
            |row| row.get(0),
        )
        .optional()?;

    tx.execute("DELETE FROM files WHERE path = ?1", params![path])?;

    if let Some(entity_id) = entity_id {
        tx.execute(
            "DELETE FROM entity_links WHERE source_id = ?1",
            params![&entity_id],
        )?;
        tx.execute("DELETE FROM tasks WHERE id = ?1", params![&entity_id])?;
        tx.execute("DELETE FROM projects WHERE id = ?1", params![&entity_id])?;
        tx.execute("DELETE FROM notes WHERE id = ?1", params![&entity_id])?;
        tx.execute("DELETE FROM entities WHERE id = ?1", params![&entity_id])?;
        tx.execute(
            "DELETE FROM fts_documents WHERE entity_id = ?1",
            params![&entity_id],
        )?;
    }

    Ok(())
}

fn upsert_indexed_entity_tx(tx: &rusqlite::Transaction<'_>, entity: &IndexedEntity) -> Result<()> {
    let path = entity.path.to_string_lossy().to_string();
    let tags_json = serde_json::to_string(&entity.tags).context("failed serializing tags")?;
    let now = Utc::now().to_rfc3339();

    tx.execute(
        r#"
        INSERT INTO entities (
            id, entity_type, title, path, body, project_id, status, priority, assignee, due_at,
            owner, tags, created_at, updated_at, revision, archived
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
        ON CONFLICT(id) DO UPDATE SET
            entity_type = excluded.entity_type,
            title = excluded.title,
            path = excluded.path,
            body = excluded.body,
            project_id = excluded.project_id,
            status = excluded.status,
            priority = excluded.priority,
            assignee = excluded.assignee,
            due_at = excluded.due_at,
            owner = excluded.owner,
            tags = excluded.tags,
            created_at = excluded.created_at,
            updated_at = excluded.updated_at,
            revision = excluded.revision,
            archived = excluded.archived
        "#,
        params![
            entity.id,
            entity.entity_type.as_str(),
            entity.title,
            path,
            entity.body,
            entity.project_id,
            entity.status,
            entity.priority,
            entity.assignee,
            entity.due_at.map(|v| v.to_rfc3339()),
            entity.owner,
            tags_json,
            entity.created_at.to_rfc3339(),
            entity.updated_at.to_rfc3339(),
            entity.revision,
            if entity.archived { 1 } else { 0 },
        ],
    )?;

    match entity.entity_type {
        EntityType::Task => {
            tx.execute(
                r#"
                INSERT INTO tasks (
                    id, project_id, status, priority, assignee, due_at, path, title,
                    created_at, updated_at, revision, archived, sort_order, completed_at,
                    sync_kind, sync_path, sync_key, sync_managed
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)
                ON CONFLICT(id) DO UPDATE SET
                    project_id = excluded.project_id,
                    status = excluded.status,
                    priority = excluded.priority,
                    assignee = excluded.assignee,
                    due_at = excluded.due_at,
                    path = excluded.path,
                    title = excluded.title,
                    created_at = excluded.created_at,
                    updated_at = excluded.updated_at,
                    revision = excluded.revision,
                    archived = excluded.archived,
                    sort_order = excluded.sort_order,
                    completed_at = excluded.completed_at,
                    sync_kind = excluded.sync_kind,
                    sync_path = excluded.sync_path,
                    sync_key = excluded.sync_key,
                    sync_managed = excluded.sync_managed
                "#,
                params![
                    entity.id,
                    entity.project_id,
                    entity.status,
                    entity.priority,
                    entity.assignee,
                    entity.due_at.map(|v| v.to_rfc3339()),
                    path,
                    entity.title,
                    entity.created_at.to_rfc3339(),
                    entity.updated_at.to_rfc3339(),
                    entity.revision,
                    if entity.archived { 1 } else { 0 },
                    entity.sort_order,
                    entity.completed_at.map(|v| v.to_rfc3339()),
                    entity.sync_kind.as_ref().map(TaskSyncKind::as_str),
                    entity.sync_path,
                    entity.sync_key,
                    if entity.sync_managed { 1 } else { 0 },
                ],
            )?;
            tx.execute("DELETE FROM projects WHERE id = ?1", params![entity.id])?;
            tx.execute("DELETE FROM notes WHERE id = ?1", params![entity.id])?;
        }
        EntityType::Project => {
            tx.execute(
                r#"
                INSERT INTO projects (
                    id, status, owner, source_kind, source_locator, sync_source_key,
                    last_synced_at, last_sync_summary, path, title,
                    created_at, updated_at, revision, archived
                )
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
                ON CONFLICT(id) DO UPDATE SET
                    status = excluded.status,
                    owner = excluded.owner,
                    source_kind = excluded.source_kind,
                    source_locator = excluded.source_locator,
                    sync_source_key = excluded.sync_source_key,
                    last_synced_at = excluded.last_synced_at,
                    last_sync_summary = excluded.last_sync_summary,
                    path = excluded.path,
                    title = excluded.title,
                    created_at = excluded.created_at,
                    updated_at = excluded.updated_at,
                    revision = excluded.revision,
                    archived = excluded.archived
                "#,
                params![
                    entity.id,
                    entity.status,
                    entity.owner,
                    entity.source_kind.as_ref().map(ProjectSourceKind::as_str),
                    entity.source_locator,
                    entity.sync_source_key,
                    entity.last_synced_at.map(|v| v.to_rfc3339()),
                    entity.last_sync_summary,
                    path,
                    entity.title,
                    entity.created_at.to_rfc3339(),
                    entity.updated_at.to_rfc3339(),
                    entity.revision,
                    if entity.archived { 1 } else { 0 },
                ],
            )?;
            tx.execute("DELETE FROM tasks WHERE id = ?1", params![entity.id])?;
            tx.execute("DELETE FROM notes WHERE id = ?1", params![entity.id])?;
        }
        EntityType::Note => {
            tx.execute(
                r#"
                INSERT INTO notes (id, project_id, path, title, created_at, updated_at, revision, archived)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                ON CONFLICT(id) DO UPDATE SET
                    project_id = excluded.project_id,
                    path = excluded.path,
                    title = excluded.title,
                    created_at = excluded.created_at,
                    updated_at = excluded.updated_at,
                    revision = excluded.revision,
                    archived = excluded.archived
                "#,
                params![
                    entity.id,
                    entity.project_id,
                    path,
                    entity.title,
                    entity.created_at.to_rfc3339(),
                    entity.updated_at.to_rfc3339(),
                    entity.revision,
                    if entity.archived { 1 } else { 0 },
                ],
            )?;
            tx.execute("DELETE FROM tasks WHERE id = ?1", params![entity.id])?;
            tx.execute("DELETE FROM projects WHERE id = ?1", params![entity.id])?;
        }
    }

    tx.execute(
        r#"
        INSERT INTO files (path, entity_id, entity_type, mtime, revision, indexed_at, archived)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
        ON CONFLICT(path) DO UPDATE SET
            entity_id = excluded.entity_id,
            entity_type = excluded.entity_type,
            mtime = excluded.mtime,
            revision = excluded.revision,
            indexed_at = excluded.indexed_at,
            archived = excluded.archived
        "#,
        params![
            path,
            entity.id,
            entity.entity_type.as_str(),
            entity.updated_at.timestamp(),
            entity.revision,
            now,
            if entity.archived { 1 } else { 0 },
        ],
    )?;

    tx.execute(
        "DELETE FROM entity_links WHERE source_id = ?1",
        params![entity.id],
    )?;
    for link in &entity.links {
        tx.execute(
            r#"
            INSERT INTO entity_links (source_id, target_type, target_id, raw)
            VALUES (?1, ?2, ?3, ?4)
            "#,
            params![
                entity.id,
                link.target_type.as_str(),
                link.target_id,
                link.raw
            ],
        )?;
    }

    tx.execute(
        "DELETE FROM fts_documents WHERE entity_id = ?1",
        params![entity.id],
    )?;
    tx.execute(
        "INSERT INTO fts_documents (entity_id, entity_type, title, body, tags) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            entity.id,
            entity.entity_type.as_str(),
            entity.title,
            entity.body,
            serde_json::to_string(&entity.tags)?
        ],
    )?;

    Ok(())
}

fn insert_activity_tx(
    tx: &rusqlite::Transaction<'_>,
    event_id: &str,
    draft: ActivityDraft,
) -> Result<()> {
    tx.execute(
        r#"
        INSERT INTO activity_events (
            event_id, occurred_at, request_id, actor_kind, actor_id, action, entity_type, entity_id,
            file_path, before_revision, after_revision, summary, git_branch, git_commit
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
        "#,
        params![
            event_id,
            draft.occurred_at.to_rfc3339(),
            draft.request_id,
            draft.actor.kind,
            draft.actor.id,
            draft.action,
            draft.entity_type.map(|v| v.as_str().to_string()),
            draft.entity_id,
            draft.file_path,
            draft.before_revision,
            draft.after_revision,
            draft.summary,
            draft.git_branch,
            draft.git_commit,
        ],
    )?;
    Ok(())
}

fn sanitize_fts_query(raw: &str) -> String {
    let escaped = raw.replace('"', "\"\"");
    format!("\"{escaped}\"")
}

fn parse_entity_type(value: &str) -> Result<EntityType> {
    EntityType::parse(value).context("invalid entity_type value in sqlite")
}

fn parse_task_status(value: &str) -> Result<TaskStatus> {
    let encoded = format!("\"{}\"", value);
    serde_json::from_str::<TaskStatus>(&encoded).context("invalid task status value in sqlite")
}

fn parse_task_priority(value: &str) -> Result<TaskPriority> {
    let encoded = format!("\"{}\"", value);
    serde_json::from_str::<TaskPriority>(&encoded).context("invalid task priority value in sqlite")
}

fn parse_task_sync_kind(value: &str) -> Result<TaskSyncKind> {
    let encoded = format!("\"{}\"", value);
    serde_json::from_str::<TaskSyncKind>(&encoded).context("invalid task sync kind value in sqlite")
}

fn parse_project_source_kind(value: &str) -> Result<ProjectSourceKind> {
    let encoded = format!("\"{}\"", value);
    serde_json::from_str::<ProjectSourceKind>(&encoded)
        .context("invalid project source kind value in sqlite")
}

fn to_sql_err(err: anyhow::Error) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        0,
        rusqlite::types::Type::Text,
        Box::new(std::io::Error::other(err.to_string())),
    )
}

fn apply_migration(conn: &mut Connection, name: &str, sql: &str) -> Result<()> {
    match name {
        "002_project_sources_and_task_order" => apply_project_source_and_task_order_migration(conn),
        "003_project_and_task_sync_metadata" => apply_project_and_task_sync_migration(conn),
        _ => conn.execute_batch(sql).map_err(Into::into),
    }
}

fn apply_project_source_and_task_order_migration(conn: &mut Connection) -> Result<()> {
    if !has_column(conn, "tasks", "sort_order")? {
        conn.execute_batch("ALTER TABLE tasks ADD COLUMN sort_order INTEGER NOT NULL DEFAULT 0;")?;
    }
    if !has_column(conn, "tasks", "completed_at")? {
        conn.execute_batch("ALTER TABLE tasks ADD COLUMN completed_at TEXT;")?;
    }
    if !has_column(conn, "projects", "source_kind")? {
        conn.execute_batch("ALTER TABLE projects ADD COLUMN source_kind TEXT;")?;
    }
    if !has_column(conn, "projects", "source_locator")? {
        conn.execute_batch("ALTER TABLE projects ADD COLUMN source_locator TEXT;")?;
    }
    conn.execute_batch(
        r#"
        CREATE INDEX IF NOT EXISTS idx_tasks_project_sort_order ON tasks(project_id, sort_order);
        CREATE INDEX IF NOT EXISTS idx_tasks_project_completed_at ON tasks(project_id, completed_at);
        "#,
    )?;
    Ok(())
}

fn apply_project_and_task_sync_migration(conn: &mut Connection) -> Result<()> {
    if !has_column(conn, "tasks", "sync_kind")? {
        conn.execute_batch("ALTER TABLE tasks ADD COLUMN sync_kind TEXT;")?;
    }
    if !has_column(conn, "tasks", "sync_path")? {
        conn.execute_batch("ALTER TABLE tasks ADD COLUMN sync_path TEXT;")?;
    }
    if !has_column(conn, "tasks", "sync_key")? {
        conn.execute_batch("ALTER TABLE tasks ADD COLUMN sync_key TEXT;")?;
    }
    if !has_column(conn, "tasks", "sync_managed")? {
        conn.execute_batch(
            "ALTER TABLE tasks ADD COLUMN sync_managed INTEGER NOT NULL DEFAULT 0;",
        )?;
    }
    if !has_column(conn, "projects", "sync_source_key")? {
        conn.execute_batch("ALTER TABLE projects ADD COLUMN sync_source_key TEXT;")?;
    }
    if !has_column(conn, "projects", "last_synced_at")? {
        conn.execute_batch("ALTER TABLE projects ADD COLUMN last_synced_at TEXT;")?;
    }
    if !has_column(conn, "projects", "last_sync_summary")? {
        conn.execute_batch("ALTER TABLE projects ADD COLUMN last_sync_summary TEXT;")?;
    }
    conn.execute_batch(
        r#"
        CREATE INDEX IF NOT EXISTS idx_tasks_project_sync_key ON tasks(project_id, sync_path, sync_key);
        CREATE INDEX IF NOT EXISTS idx_projects_sync_source_key ON projects(sync_source_key);
        "#,
    )?;
    Ok(())
}

fn has_column(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
    for row in rows {
        if row? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

const MIGRATIONS: &[(&str, &str)] = &[
    (
        "001_base",
        r#"
    CREATE TABLE IF NOT EXISTS files (
        path TEXT PRIMARY KEY,
        entity_id TEXT NOT NULL,
        entity_type TEXT NOT NULL,
        mtime INTEGER,
        revision TEXT NOT NULL,
        indexed_at TEXT NOT NULL,
        archived INTEGER NOT NULL DEFAULT 0
    );

    CREATE TABLE IF NOT EXISTS entities (
        id TEXT PRIMARY KEY,
        entity_type TEXT NOT NULL,
        title TEXT NOT NULL,
        path TEXT NOT NULL,
        body TEXT NOT NULL,
        project_id TEXT,
        status TEXT,
        priority TEXT,
        assignee TEXT,
        due_at TEXT,
        owner TEXT,
        tags TEXT NOT NULL DEFAULT '[]',
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL,
        revision TEXT NOT NULL,
        archived INTEGER NOT NULL DEFAULT 0
    );

    CREATE TABLE IF NOT EXISTS tasks (
        id TEXT PRIMARY KEY,
        project_id TEXT NOT NULL,
        status TEXT NOT NULL,
        priority TEXT NOT NULL,
        assignee TEXT NOT NULL,
        due_at TEXT,
        path TEXT NOT NULL,
        title TEXT NOT NULL,
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL,
        revision TEXT NOT NULL,
        archived INTEGER NOT NULL DEFAULT 0,
        sort_order INTEGER NOT NULL DEFAULT 0,
        completed_at TEXT,
        sync_kind TEXT,
        sync_path TEXT,
        sync_key TEXT,
        sync_managed INTEGER NOT NULL DEFAULT 0
    );

    CREATE TABLE IF NOT EXISTS projects (
        id TEXT PRIMARY KEY,
        status TEXT NOT NULL,
        owner TEXT,
        source_kind TEXT,
        source_locator TEXT,
        sync_source_key TEXT,
        last_synced_at TEXT,
        last_sync_summary TEXT,
        path TEXT NOT NULL,
        title TEXT NOT NULL,
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL,
        revision TEXT NOT NULL,
        archived INTEGER NOT NULL DEFAULT 0
    );

    CREATE TABLE IF NOT EXISTS notes (
        id TEXT PRIMARY KEY,
        project_id TEXT,
        path TEXT NOT NULL,
        title TEXT NOT NULL,
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL,
        revision TEXT NOT NULL,
        archived INTEGER NOT NULL DEFAULT 0
    );

    CREATE TABLE IF NOT EXISTS entity_links (
        source_id TEXT NOT NULL,
        target_type TEXT NOT NULL,
        target_id TEXT NOT NULL,
        raw TEXT NOT NULL,
        PRIMARY KEY(source_id, target_type, target_id, raw)
    );

    CREATE TABLE IF NOT EXISTS activity_events (
        event_id TEXT PRIMARY KEY,
        occurred_at TEXT NOT NULL,
        request_id TEXT NOT NULL,
        actor_kind TEXT NOT NULL,
        actor_id TEXT NOT NULL,
        action TEXT NOT NULL,
        entity_type TEXT,
        entity_id TEXT,
        file_path TEXT,
        before_revision TEXT,
        after_revision TEXT,
        summary TEXT NOT NULL,
        git_branch TEXT,
        git_commit TEXT
    );

    CREATE VIRTUAL TABLE IF NOT EXISTS fts_documents USING fts5(
        entity_id UNINDEXED,
        entity_type UNINDEXED,
        title,
        body,
        tags,
        tokenize='porter unicode61'
    );

    CREATE INDEX IF NOT EXISTS idx_entities_path ON entities(path);
    CREATE INDEX IF NOT EXISTS idx_entities_project ON entities(project_id);
    CREATE INDEX IF NOT EXISTS idx_tasks_status ON tasks(status);
    CREATE INDEX IF NOT EXISTS idx_tasks_project ON tasks(project_id);
    CREATE INDEX IF NOT EXISTS idx_tasks_project_sort_order ON tasks(project_id, sort_order);
    CREATE INDEX IF NOT EXISTS idx_tasks_project_completed_at ON tasks(project_id, completed_at);
    CREATE INDEX IF NOT EXISTS idx_tasks_project_sync_key ON tasks(project_id, sync_path, sync_key);
    CREATE INDEX IF NOT EXISTS idx_projects_sync_source_key ON projects(sync_source_key);
    CREATE INDEX IF NOT EXISTS idx_notes_project ON notes(project_id);
    CREATE INDEX IF NOT EXISTS idx_activity_occurred_at ON activity_events(occurred_at);
    "#,
    ),
    ("002_project_sources_and_task_order", ""),
    ("003_project_and_task_sync_metadata", ""),
];
