use anyhow::{anyhow, Result};
use chrono::{DateTime, Local, Utc};
use log::{debug, error, info};
use rusqlite::{params, Connection, OptionalExtension};
use rusqlite_migration::{Migrations, M};
use serde::{Deserialize, Serialize};
use specta::Type;
use std::fs;
use std::path::PathBuf;
use tauri::AppHandle;
use tauri_specta::Event;

/// Database migrations for transcription history.
/// Each migration is applied in order. The library tracks which migrations
/// have been applied using SQLite's user_version pragma.
///
/// Note: For users upgrading from tauri-plugin-sql, migrate_from_tauri_plugin_sql()
/// converts the old _sqlx_migrations table tracking to the user_version pragma,
/// ensuring migrations don't re-run on existing databases.
static MIGRATIONS: &[M] = &[
    M::up(
        "CREATE TABLE IF NOT EXISTS transcription_history (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            file_name TEXT NOT NULL,
            timestamp INTEGER NOT NULL,
            saved BOOLEAN NOT NULL DEFAULT 0,
            title TEXT NOT NULL,
            transcription_text TEXT NOT NULL
        );",
    ),
    M::up("ALTER TABLE transcription_history ADD COLUMN post_processed_text TEXT;"),
    M::up("ALTER TABLE transcription_history ADD COLUMN post_process_prompt TEXT;"),
    M::up("ALTER TABLE transcription_history ADD COLUMN post_process_requested BOOLEAN NOT NULL DEFAULT 0;"),
    // Auditability for the RAM auto-fallback: which model actually produced
    // each entry. Nullable column; pre-existing rows are backfilled to '' so
    // no row is left NULL (empty reads back as `None` in HistoryEntry).
    M::up(
        "ALTER TABLE transcription_history ADD COLUMN model_id TEXT;
         UPDATE transcription_history SET model_id = '' WHERE model_id IS NULL;",
    ),
    // The pp: observability lifecycle's per-run summary (which engine and
    // prompt polished this entry, how it ended, how long it took). All
    // nullable: pre-cycle rows and non-post-processed entries read back as
    // None and render unchanged.
    M::up(
        "ALTER TABLE transcription_history ADD COLUMN post_process_provider TEXT;
         ALTER TABLE transcription_history ADD COLUMN post_process_model TEXT;
         ALTER TABLE transcription_history ADD COLUMN post_process_prompt_id TEXT;
         ALTER TABLE transcription_history ADD COLUMN post_process_outcome TEXT;
         ALTER TABLE transcription_history ADD COLUMN post_process_latency_ms INTEGER;",
    ),
    // v1.6.0 companion provenance: where each entry's audio came from
    // ('local' keyboard session | 'companion' phone session) and where the
    // transcript was delivered ('mac' pasted on the Mac | 'phone' sent to
    // the phone as a TranscriptResult frame). NOT NULL with the legacy
    // defaults, so every pre-migration row reads local/mac (the only
    // distinguishable truth: pre-v1.6.0 companion rows are indistinguishable
    // from local ones and default with them).
    M::up(
        "ALTER TABLE transcription_history ADD COLUMN capture_source TEXT NOT NULL DEFAULT 'local';
         ALTER TABLE transcription_history ADD COLUMN delivery_destination TEXT NOT NULL DEFAULT 'mac';",
    ),
];

#[derive(Clone, Debug, Serialize, Deserialize, Type)]
pub struct PaginatedHistory {
    pub entries: Vec<HistoryEntry>,
    pub has_more: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, Type, tauri_specta::Event)]
#[serde(tag = "action")]
pub enum HistoryUpdatePayload {
    #[serde(rename = "added")]
    Added { entry: HistoryEntry },
    #[serde(rename = "updated")]
    Updated { entry: HistoryEntry },
    #[serde(rename = "deleted")]
    Deleted { id: i64 },
    #[serde(rename = "toggled")]
    Toggled { id: i64 },
}

#[derive(Clone, Debug, Serialize, Deserialize, Type)]
pub struct HistoryEntry {
    pub id: i64,
    pub file_name: String,
    pub timestamp: i64,
    pub saved: bool,
    pub title: String,
    pub transcription_text: String,
    pub post_processed_text: Option<String>,
    pub post_process_prompt: Option<String>,
    pub post_process_requested: bool,
    /// Model id that actually produced `transcription_text` (the resident
    /// model at transcription time - after a RAM auto-fallback this is the
    /// fallback). `None` for pre-migration entries or when the model was
    /// unknown (e.g. a failed transcription saved for retry).
    pub model_id: Option<String>,
    /// The pp: run summary (the post-process lifecycle, WS3): which
    /// provider/model/prompt polished this entry, how the run ended
    /// (`applied` | `skipped:<reason>` | `failed:<class>`), and its
    /// latency. `None` for pre-cycle rows and entries whose dictation ran
    /// without post-processing.
    pub post_process_provider: Option<String>,
    pub post_process_model: Option<String>,
    pub post_process_prompt_id: Option<String>,
    pub post_process_outcome: Option<String>,
    pub post_process_latency_ms: Option<i64>,
    /// v1.6.0: where the audio came from, "local" (keyboard session) or
    /// "companion" (phone session). Pre-migration rows read "local".
    pub capture_source: String,
    /// v1.6.0: where the transcript was (or, for a failed entry, was meant
    /// to be) delivered, "mac" (pasted on the Mac) or "phone" (sent to the
    /// phone). Pre-migration rows read "mac".
    pub delivery_destination: String,
}

/// One entry of the phone-facing recent list (v1.6.0): the deliverable text
/// of a companion-source history row (post-processed when the session
/// polished it, else the raw transcript) plus the entry timestamp. The
/// RecentHistory wire frame and the get_recent_companion_entries command
/// both carry these.
#[derive(Clone, Debug, Serialize, Deserialize, Type)]
pub struct RecentCompanionEntry {
    pub text: String,
    pub timestamp: i64,
}

pub struct HistoryManager {
    app_handle: AppHandle,
    recordings_dir: PathBuf,
    db_path: PathBuf,
}

impl HistoryManager {
    pub fn new(app_handle: &AppHandle) -> Result<Self> {
        // Create recordings directory in app data dir
        let app_data_dir = crate::portable::app_data_dir(app_handle)?;
        let recordings_dir = app_data_dir.join("recordings");
        let db_path = app_data_dir.join("history.db");

        // Ensure recordings directory exists
        if !recordings_dir.exists() {
            fs::create_dir_all(&recordings_dir)?;
            debug!("Created recordings directory: {:?}", recordings_dir);
        }

        let manager = Self {
            app_handle: app_handle.clone(),
            recordings_dir,
            db_path,
        };

        // Initialize database and run migrations synchronously
        manager.init_database()?;

        Ok(manager)
    }

    fn init_database(&self) -> Result<()> {
        info!("Initializing database at {:?}", self.db_path);

        let mut conn = Connection::open(&self.db_path)?;

        // Handle migration from tauri-plugin-sql to rusqlite_migration
        // tauri-plugin-sql used _sqlx_migrations table, rusqlite_migration uses user_version pragma
        self.migrate_from_tauri_plugin_sql(&conn)?;

        // Create migrations object and run to latest version
        let migrations = Migrations::new(MIGRATIONS.to_vec());

        // Validate migrations in debug builds
        #[cfg(debug_assertions)]
        migrations.validate().expect("Invalid migrations");

        // Get current version before migration
        let version_before: i32 =
            conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        debug!("Database version before migration: {}", version_before);

        // Apply any pending migrations
        migrations.to_latest(&mut conn)?;

        // Get version after migration
        let version_after: i32 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;

        if version_after > version_before {
            info!(
                "Database migrated from version {} to {}",
                version_before, version_after
            );
        } else {
            debug!("Database already at latest version {}", version_after);
        }

        Ok(())
    }

    /// Migrate from tauri-plugin-sql's migration tracking to rusqlite_migration's.
    /// tauri-plugin-sql used a _sqlx_migrations table, while rusqlite_migration uses
    /// SQLite's user_version pragma. This function checks if the old system was in use
    /// and sets the user_version accordingly so migrations don't re-run.
    fn migrate_from_tauri_plugin_sql(&self, conn: &Connection) -> Result<()> {
        // Check if the old _sqlx_migrations table exists
        let has_sqlx_migrations: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name='_sqlx_migrations'",
                [],
                |row| row.get(0),
            )
            .unwrap_or(false);

        if !has_sqlx_migrations {
            return Ok(());
        }

        // Check current user_version
        let current_version: i32 =
            conn.pragma_query_value(None, "user_version", |row| row.get(0))?;

        if current_version > 0 {
            // Already migrated to rusqlite_migration system
            return Ok(());
        }

        // Get the highest version from the old migrations table
        let old_version: i32 = conn
            .query_row(
                "SELECT COALESCE(MAX(version), 0) FROM _sqlx_migrations WHERE success = 1",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);

        if old_version > 0 {
            info!(
                "Migrating from tauri-plugin-sql (version {}) to rusqlite_migration",
                old_version
            );

            // Set user_version to match the old migration state
            conn.pragma_update(None, "user_version", old_version)?;

            // Optionally drop the old migrations table (keeping it doesn't hurt)
            // conn.execute("DROP TABLE IF EXISTS _sqlx_migrations", [])?;

            info!(
                "Migration tracking converted: user_version set to {}",
                old_version
            );
        }

        Ok(())
    }

    fn get_connection(&self) -> Result<Connection> {
        Ok(Connection::open(&self.db_path)?)
    }

    fn map_history_entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<HistoryEntry> {
        Ok(HistoryEntry {
            id: row.get("id")?,
            file_name: row.get("file_name")?,
            timestamp: row.get("timestamp")?,
            saved: row.get("saved")?,
            title: row.get("title")?,
            transcription_text: row.get("transcription_text")?,
            post_processed_text: row.get("post_processed_text")?,
            post_process_prompt: row.get("post_process_prompt")?,
            post_process_requested: row.get("post_process_requested")?,
            // Legacy rows are backfilled to '' by the migration; read both
            // NULL and '' as "unknown" so the UI never shows an empty badge.
            model_id: row
                .get::<_, Option<String>>("model_id")?
                .filter(|model| !model.is_empty()),
            post_process_provider: row.get("post_process_provider")?,
            post_process_model: row.get("post_process_model")?,
            post_process_prompt_id: row.get("post_process_prompt_id")?,
            post_process_outcome: row.get("post_process_outcome")?,
            post_process_latency_ms: row.get("post_process_latency_ms")?,
            capture_source: row.get("capture_source")?,
            delivery_destination: row.get("delivery_destination")?,
        })
    }

    pub fn recordings_dir(&self) -> &std::path::Path {
        &self.recordings_dir
    }

    /// Save a new history entry to the database.
    /// The WAV file should already have been written to the recordings directory.
    /// `model_id` is the model that actually produced the transcription (the
    /// resident model at transcription time - after a RAM auto-fallback this
    /// is the fallback), recorded per entry for auditability.
    /// `post_process_summary` carries the pp: run's summary (provider,
    /// model, prompt, outcome, latency) for the row detail.
    /// `capture_source` is "local" or "companion"; `delivery_destination`
    /// is "mac" or "phone" (v1.6.0 provenance columns).
    pub fn save_entry(
        &self,
        file_name: String,
        transcription_text: String,
        post_process_requested: bool,
        post_processed_text: Option<String>,
        post_process_prompt: Option<String>,
        model_id: Option<String>,
        post_process_summary: Option<crate::post_process_runs::PostProcessRunSummary>,
        capture_source: String,
        delivery_destination: String,
    ) -> Result<HistoryEntry> {
        let timestamp = Utc::now().timestamp();
        let title = self.format_timestamp_title(timestamp);

        let conn = self.get_connection()?;
        let entry = Self::insert_entry_with_conn(
            &conn,
            file_name,
            transcription_text,
            post_process_requested,
            post_processed_text,
            post_process_prompt,
            model_id,
            post_process_summary,
            capture_source,
            delivery_destination,
            timestamp,
            title,
        )?;

        debug!("Saved history entry with id {}", entry.id);

        self.cleanup_old_entries()?;

        // Emit typed event for real-time frontend updates
        if let Err(e) = (HistoryUpdatePayload::Added {
            entry: entry.clone(),
        })
        .emit(&self.app_handle)
        {
            error!("Failed to emit history-updated event: {}", e);
        }

        Ok(entry)
    }

    /// INSERT a new history row and return the resulting entry. Split out of
    /// [`save_entry`](Self::save_entry) so tests can exercise the write path
    /// (including the model id) against an in-memory connection.
    fn insert_entry_with_conn(
        conn: &Connection,
        file_name: String,
        transcription_text: String,
        post_process_requested: bool,
        post_processed_text: Option<String>,
        post_process_prompt: Option<String>,
        model_id: Option<String>,
        post_process_summary: Option<crate::post_process_runs::PostProcessRunSummary>,
        capture_source: String,
        delivery_destination: String,
        timestamp: i64,
        title: String,
    ) -> Result<HistoryEntry> {
        // Unknown model writes as '' (never NULL) so the migration's
        // no-NULL invariant holds for new rows too.
        let stored_model_id = model_id.clone().unwrap_or_default();
        let (pp_provider, pp_model, pp_prompt_id, pp_outcome, pp_latency) =
            match &post_process_summary {
                Some(summary) => (
                    Some(summary.provider_id.clone()),
                    Some(summary.model.clone()),
                    Some(summary.prompt_id.clone()),
                    Some(summary.outcome.clone()),
                    Some(summary.latency_ms as i64),
                ),
                None => (None, None, None, None, None),
            };
        conn.execute(
            "INSERT INTO transcription_history (
                file_name,
                timestamp,
                saved,
                title,
                transcription_text,
                post_processed_text,
                post_process_prompt,
                post_process_requested,
                model_id,
                post_process_provider,
                post_process_model,
                post_process_prompt_id,
                post_process_outcome,
                post_process_latency_ms,
                capture_source,
                delivery_destination
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
            params![
                &file_name,
                timestamp,
                false,
                &title,
                &transcription_text,
                &post_processed_text,
                &post_process_prompt,
                post_process_requested,
                &stored_model_id,
                &pp_provider,
                &pp_model,
                &pp_prompt_id,
                &pp_outcome,
                &pp_latency,
                &capture_source,
                &delivery_destination,
            ],
        )?;

        Ok(HistoryEntry {
            id: conn.last_insert_rowid(),
            file_name,
            timestamp,
            saved: false,
            title,
            transcription_text,
            post_processed_text,
            post_process_prompt,
            post_process_requested,
            model_id: (!stored_model_id.is_empty()).then_some(stored_model_id),
            post_process_provider: pp_provider,
            post_process_model: pp_model,
            post_process_prompt_id: pp_prompt_id,
            post_process_outcome: pp_outcome,
            post_process_latency_ms: pp_latency,
            capture_source,
            delivery_destination,
        })
    }

    /// The phone-facing recent list (v1.6.0): the last `limit`
    /// companion-source entries with non-empty deliverable text, newest
    /// first. The deliverable text is the post-processed transcript when
    /// the session polished it, else the raw transcription; entries whose
    /// deliverable text is blank (failed sessions saved for retry) are
    /// skipped - the phone list shows what was actually dictated. Split on
    /// a connection so tests exercise the real query in memory.
    fn recent_companion_entries_with_conn(
        conn: &Connection,
        limit: usize,
    ) -> Result<Vec<RecentCompanionEntry>> {
        let mut stmt = conn.prepare(
            "SELECT COALESCE(NULLIF(post_processed_text, ''), transcription_text) AS deliverable,
                    timestamp
             FROM transcription_history
             WHERE capture_source = 'companion'
               AND TRIM(COALESCE(NULLIF(post_processed_text, ''), transcription_text)) != ''
             ORDER BY id DESC
             LIMIT ?1",
        )?;
        let entries = stmt
            .query_map(params![limit as i64], |row| {
                Ok(RecentCompanionEntry {
                    text: row.get("deliverable")?,
                    timestamp: row.get("timestamp")?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(entries)
    }

    /// The phone-facing recent list (see
    /// [`recent_companion_entries_with_conn`](Self::recent_companion_entries_with_conn)):
    /// the last companion-source entries, newest first.
    pub fn recent_companion_entries(&self, limit: usize) -> Result<Vec<RecentCompanionEntry>> {
        let conn = self.get_connection()?;
        Self::recent_companion_entries_with_conn(&conn, limit)
    }

    /// Update an existing history entry with new transcription results (used by retry).
    /// `model_id` is the model that produced this retry's transcription.
    pub fn update_transcription(
        &self,
        id: i64,
        transcription_text: String,
        post_processed_text: Option<String>,
        post_process_prompt: Option<String>,
        model_id: Option<String>,
    ) -> Result<HistoryEntry> {
        let conn = self.get_connection()?;
        let updated = conn.execute(
            "UPDATE transcription_history
             SET transcription_text = ?1,
                 post_processed_text = ?2,
                 post_process_prompt = ?3,
                 model_id = ?4
             WHERE id = ?5",
            params![
                transcription_text,
                post_processed_text,
                post_process_prompt,
                model_id.unwrap_or_default(),
                id
            ],
        )?;

        if updated == 0 {
            return Err(anyhow!("History entry {} not found", id));
        }

        let entry = conn
            .query_row(
                "SELECT id, file_name, timestamp, saved, title, transcription_text, post_processed_text, post_process_prompt, post_process_requested, model_id,
                post_process_provider, post_process_model, post_process_prompt_id,
                post_process_outcome, post_process_latency_ms,
                capture_source, delivery_destination
                 FROM transcription_history WHERE id = ?1",
                params![id],
                Self::map_history_entry,
            )?;

        debug!("Updated transcription for history entry {}", id);

        if let Err(e) = (HistoryUpdatePayload::Updated {
            entry: entry.clone(),
        })
        .emit(&self.app_handle)
        {
            error!("Failed to emit history-updated event: {}", e);
        }

        Ok(entry)
    }

    pub fn cleanup_old_entries(&self) -> Result<()> {
        self.cleanup_old_entries_with(
            crate::settings::get_recording_retention_period(&self.app_handle),
            crate::settings::get_history_limit(&self.app_handle),
        )
    }

    /// KB-202: [`Self::cleanup_old_entries`] with the two store reads
    /// hoisted into explicit parameters, so the settings commands can run
    /// the cleanup against the WOULD-BE values BEFORE persisting (each
    /// command passes its incoming value for the field it changes and the
    /// store's current value for the other). The passes themselves are
    /// unchanged - by-count deletes beyond `history_limit`, by-time honors
    /// `retention_period` - they already took these as parameters; only the
    /// dispatcher used to re-read the store.
    pub fn cleanup_old_entries_with(
        &self,
        retention_period: crate::settings::RecordingRetentionPeriod,
        history_limit: usize,
    ) -> Result<()> {
        match retention_period {
            crate::settings::RecordingRetentionPeriod::Never => {
                // Don't delete anything
                Ok(())
            }
            crate::settings::RecordingRetentionPeriod::PreserveLimit => {
                // Use the old count-based logic with history_limit
                self.cleanup_by_count(history_limit)
            }
            _ => {
                // Use time-based logic
                self.cleanup_by_time(retention_period)
            }
        }
    }

    fn delete_entries_and_files(&self, entries: &[(i64, String)]) -> Result<usize> {
        if entries.is_empty() {
            return Ok(0);
        }

        let conn = self.get_connection()?;
        let mut deleted_count = 0;

        for (id, file_name) in entries {
            // Delete database entry
            conn.execute(
                "DELETE FROM transcription_history WHERE id = ?1",
                params![id],
            )?;

            // Delete WAV file
            let file_path = self.recordings_dir.join(file_name);
            if file_path.exists() {
                if let Err(e) = fs::remove_file(&file_path) {
                    error!("Failed to delete WAV file {}: {}", file_name, e);
                } else {
                    debug!("Deleted old WAV file: {}", file_name);
                    deleted_count += 1;
                }
            }
        }

        Ok(deleted_count)
    }

    fn cleanup_by_count(&self, limit: usize) -> Result<()> {
        let conn = self.get_connection()?;

        // Get all entries that are not saved, ordered by timestamp desc
        let mut stmt = conn.prepare(
            "SELECT id, file_name FROM transcription_history WHERE saved = 0 ORDER BY timestamp DESC"
        )?;

        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, i64>("id")?, row.get::<_, String>("file_name")?))
        })?;

        let mut entries: Vec<(i64, String)> = Vec::new();
        for row in rows {
            entries.push(row?);
        }

        if entries.len() > limit {
            let entries_to_delete = &entries[limit..];
            let deleted_count = self.delete_entries_and_files(entries_to_delete)?;

            if deleted_count > 0 {
                debug!("Cleaned up {} old history entries by count", deleted_count);
            }
        }

        Ok(())
    }

    fn cleanup_by_time(
        &self,
        retention_period: crate::settings::RecordingRetentionPeriod,
    ) -> Result<()> {
        let conn = self.get_connection()?;

        // Calculate cutoff timestamp (current time minus retention period)
        let now = Utc::now().timestamp();
        let cutoff_timestamp = match retention_period {
            crate::settings::RecordingRetentionPeriod::Days3 => now - (3 * 24 * 60 * 60), // 3 days in seconds
            crate::settings::RecordingRetentionPeriod::Weeks2 => now - (2 * 7 * 24 * 60 * 60), // 2 weeks in seconds
            crate::settings::RecordingRetentionPeriod::Months3 => now - (3 * 30 * 24 * 60 * 60), // 3 months in seconds (approximate)
            _ => unreachable!("Should not reach here"),
        };

        // Get all unsaved entries older than the cutoff timestamp
        let mut stmt = conn.prepare(
            "SELECT id, file_name FROM transcription_history WHERE saved = 0 AND timestamp < ?1",
        )?;

        let rows = stmt.query_map(params![cutoff_timestamp], |row| {
            Ok((row.get::<_, i64>("id")?, row.get::<_, String>("file_name")?))
        })?;

        let mut entries_to_delete: Vec<(i64, String)> = Vec::new();
        for row in rows {
            entries_to_delete.push(row?);
        }

        let deleted_count = self.delete_entries_and_files(&entries_to_delete)?;

        if deleted_count > 0 {
            debug!(
                "Cleaned up {} old history entries based on retention period",
                deleted_count
            );
        }

        Ok(())
    }

    pub async fn get_history_entries(
        &self,
        cursor: Option<i64>,
        limit: Option<usize>,
    ) -> Result<PaginatedHistory> {
        let conn = self.get_connection()?;
        let limit = limit.map(|l| l.min(100));

        let mut entries: Vec<HistoryEntry> = match (cursor, limit) {
            (Some(cursor_id), Some(lim)) => {
                let fetch_count = (lim + 1) as i64;
                let mut stmt = conn.prepare(
                    "SELECT id, file_name, timestamp, saved, title, transcription_text, post_processed_text, post_process_prompt, post_process_requested, model_id,
                post_process_provider, post_process_model, post_process_prompt_id,
                post_process_outcome, post_process_latency_ms,
                capture_source, delivery_destination
                     FROM transcription_history
                     WHERE id < ?1
                     ORDER BY id DESC
                     LIMIT ?2",
                )?;
                let result = stmt
                    .query_map(params![cursor_id, fetch_count], Self::map_history_entry)?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                result
            }
            (None, Some(lim)) => {
                let fetch_count = (lim + 1) as i64;
                let mut stmt = conn.prepare(
                    "SELECT id, file_name, timestamp, saved, title, transcription_text, post_processed_text, post_process_prompt, post_process_requested, model_id,
                post_process_provider, post_process_model, post_process_prompt_id,
                post_process_outcome, post_process_latency_ms,
                capture_source, delivery_destination
                     FROM transcription_history
                     ORDER BY id DESC
                     LIMIT ?1",
                )?;
                let result = stmt
                    .query_map(params![fetch_count], Self::map_history_entry)?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                result
            }
            (_, None) => {
                let mut stmt = conn.prepare(
                    "SELECT id, file_name, timestamp, saved, title, transcription_text, post_processed_text, post_process_prompt, post_process_requested, model_id,
                post_process_provider, post_process_model, post_process_prompt_id,
                post_process_outcome, post_process_latency_ms,
                capture_source, delivery_destination
                     FROM transcription_history
                     ORDER BY id DESC",
                )?;
                let result = stmt
                    .query_map([], Self::map_history_entry)?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                result
            }
        };

        let has_more = limit.is_some_and(|lim| entries.len() > lim);
        if has_more {
            entries.pop();
        }

        Ok(PaginatedHistory { entries, has_more })
    }

    #[cfg(test)]
    fn get_latest_entry_with_conn(conn: &Connection) -> Result<Option<HistoryEntry>> {
        let mut stmt = conn.prepare(
            "SELECT
                id,
                file_name,
                timestamp,
                saved,
                title,
                transcription_text,
                post_processed_text,
                post_process_prompt,
                post_process_requested,
                model_id,
                post_process_provider,
                post_process_model,
                post_process_prompt_id,
                post_process_outcome,
                post_process_latency_ms,
                capture_source,
                delivery_destination
             FROM transcription_history
             ORDER BY timestamp DESC
             LIMIT 1",
        )?;

        let entry = stmt.query_row([], Self::map_history_entry).optional()?;
        Ok(entry)
    }

    /// Get the latest entry with non-empty transcription text.
    pub fn get_latest_completed_entry(&self) -> Result<Option<HistoryEntry>> {
        let conn = self.get_connection()?;
        Self::get_latest_completed_entry_with_conn(&conn)
    }

    fn get_latest_completed_entry_with_conn(conn: &Connection) -> Result<Option<HistoryEntry>> {
        let mut stmt = conn.prepare(
            "SELECT
                id,
                file_name,
                timestamp,
                saved,
                title,
                transcription_text,
                post_processed_text,
                post_process_prompt,
                post_process_requested,
                model_id,
                post_process_provider,
                post_process_model,
                post_process_prompt_id,
                post_process_outcome,
                post_process_latency_ms,
                capture_source,
                delivery_destination
             FROM transcription_history
             WHERE transcription_text != ''
             ORDER BY timestamp DESC
             LIMIT 1",
        )?;

        let entry = stmt.query_row([], Self::map_history_entry).optional()?;
        Ok(entry)
    }

    pub async fn toggle_saved_status(&self, id: i64) -> Result<()> {
        let conn = self.get_connection()?;

        // Get current saved status
        let current_saved: bool = conn.query_row(
            "SELECT saved FROM transcription_history WHERE id = ?1",
            params![id],
            |row| row.get("saved"),
        )?;

        let new_saved = !current_saved;

        conn.execute(
            "UPDATE transcription_history SET saved = ?1 WHERE id = ?2",
            params![new_saved, id],
        )?;

        debug!("Toggled saved status for entry {}: {}", id, new_saved);

        // Emit history updated event
        if let Err(e) = (HistoryUpdatePayload::Toggled { id }).emit(&self.app_handle) {
            error!("Failed to emit history-updated event: {}", e);
        }

        Ok(())
    }

    pub fn get_audio_file_path(&self, file_name: &str) -> PathBuf {
        self.recordings_dir.join(file_name)
    }

    /// The last `limit` completed entries (non-empty transcript), newest
    /// first, for the tray's Recent Dictations submenu (W1-L8). Same
    /// completed filter as [`Self::get_latest_completed_entry`]; a
    /// menu-input seam the tray refreshes OFF the UI thread (see
    /// `tray::refresh_tray_history`), never synchronously from the menu
    /// builder.
    pub fn recent_completed_entries(&self, limit: usize) -> Result<Vec<HistoryEntry>> {
        let conn = self.get_connection()?;
        Self::recent_completed_entries_with_conn(&conn, limit)
    }

    /// The `_with_conn` twin of [`Self::recent_completed_entries`], so the
    /// completed filter, newest-first ordering, and limit are unit-testable
    /// against the same in-memory schema the sibling queries pin (see
    /// `get_latest_completed_entry_skips_empty_entries`).
    fn recent_completed_entries_with_conn(
        conn: &Connection,
        limit: usize,
    ) -> Result<Vec<HistoryEntry>> {
        let mut stmt = conn.prepare(
            "SELECT
                id,
                file_name,
                timestamp,
                saved,
                title,
                transcription_text,
                post_processed_text,
                post_process_prompt,
                post_process_requested,
                model_id,
                post_process_provider,
                post_process_model,
                post_process_prompt_id,
                post_process_outcome,
                post_process_latency_ms,
                capture_source,
                delivery_destination
             FROM transcription_history
             WHERE transcription_text != ''
             ORDER BY timestamp DESC
             LIMIT ?1",
        )?;
        let entries = stmt
            .query_map(params![limit as i64], Self::map_history_entry)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(entries)
    }

    pub async fn get_entry_by_id(&self, id: i64) -> Result<Option<HistoryEntry>> {
        let conn = self.get_connection()?;
        let mut stmt = conn.prepare(
            "SELECT
                id,
                file_name,
                timestamp,
                saved,
                title,
                transcription_text,
                post_processed_text,
                post_process_prompt,
                post_process_requested,
                model_id,
                post_process_provider,
                post_process_model,
                post_process_prompt_id,
                post_process_outcome,
                post_process_latency_ms,
                capture_source,
                delivery_destination
             FROM transcription_history
             WHERE id = ?1",
        )?;

        let entry = stmt.query_row([id], Self::map_history_entry).optional()?;

        Ok(entry)
    }

    pub async fn delete_entry(&self, id: i64) -> Result<()> {
        let conn = self.get_connection()?;

        // Get the entry to find the file name
        if let Some(entry) = self.get_entry_by_id(id).await? {
            // Delete the audio file first
            let file_path = self.get_audio_file_path(&entry.file_name);
            if file_path.exists() {
                if let Err(e) = fs::remove_file(&file_path) {
                    error!("Failed to delete audio file {}: {}", entry.file_name, e);
                    // Continue with database deletion even if file deletion fails
                }
            }
        }

        // Delete from database
        conn.execute(
            "DELETE FROM transcription_history WHERE id = ?1",
            params![id],
        )?;

        debug!("Deleted history entry with id: {}", id);

        // Emit history updated event
        if let Err(e) = (HistoryUpdatePayload::Deleted { id }).emit(&self.app_handle) {
            error!("Failed to emit history-updated event: {}", e);
        }

        Ok(())
    }

    fn format_timestamp_title(&self, timestamp: i64) -> String {
        if let Some(utc_datetime) = DateTime::from_timestamp(timestamp, 0) {
            // Convert UTC to local timezone
            let local_datetime = utc_datetime.with_timezone(&Local);
            local_datetime.format("%B %e, %Y - %l:%M%p").to_string()
        } else {
            format!("Recording {}", timestamp)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::{params, Connection};

    fn setup_conn() -> Connection {
        let conn = Connection::open_in_memory().expect("open in-memory db");
        conn.execute_batch(
            "CREATE TABLE transcription_history (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                file_name TEXT NOT NULL,
                timestamp INTEGER NOT NULL,
                saved BOOLEAN NOT NULL DEFAULT 0,
                title TEXT NOT NULL,
                transcription_text TEXT NOT NULL,
                post_processed_text TEXT,
                post_process_prompt TEXT,
                post_process_requested BOOLEAN NOT NULL DEFAULT 0,
                model_id TEXT,
                post_process_provider TEXT,
                post_process_model TEXT,
                post_process_prompt_id TEXT,
                post_process_outcome TEXT,
                post_process_latency_ms INTEGER,
                capture_source TEXT NOT NULL DEFAULT 'local',
                delivery_destination TEXT NOT NULL DEFAULT 'mac'
            );",
        )
        .expect("create transcription_history table");
        conn
    }

    fn insert_entry(conn: &Connection, timestamp: i64, text: &str, post_processed: Option<&str>) {
        conn.execute(
            "INSERT INTO transcription_history (
                file_name,
                timestamp,
                saved,
                title,
                transcription_text,
                post_processed_text,
                post_process_prompt,
                post_process_requested
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                format!("voxbar-{}.wav", timestamp),
                timestamp,
                false,
                format!("Recording {}", timestamp),
                text,
                post_processed,
                Option::<String>::None,
                false,
            ],
        )
        .expect("insert history entry");
    }

    #[test]
    fn get_latest_entry_returns_none_when_empty() {
        let conn = setup_conn();
        let entry = HistoryManager::get_latest_entry_with_conn(&conn).expect("fetch latest entry");
        assert!(entry.is_none());
    }

    #[test]
    fn get_latest_entry_returns_newest_entry() {
        let conn = setup_conn();
        insert_entry(&conn, 100, "first", None);
        insert_entry(&conn, 200, "second", Some("processed"));

        let entry = HistoryManager::get_latest_entry_with_conn(&conn)
            .expect("fetch latest entry")
            .expect("entry exists");

        assert_eq!(entry.timestamp, 200);
        assert_eq!(entry.transcription_text, "second");
        assert_eq!(entry.post_processed_text.as_deref(), Some("processed"));
    }

    #[test]
    fn get_latest_completed_entry_skips_empty_entries() {
        let conn = setup_conn();
        insert_entry(&conn, 100, "completed", None);
        insert_entry(&conn, 200, "", None);

        let entry = HistoryManager::get_latest_completed_entry_with_conn(&conn)
            .expect("fetch latest completed entry")
            .expect("completed entry exists");

        assert_eq!(entry.timestamp, 100);
        assert_eq!(entry.transcription_text, "completed");
    }

    /// W1-L8: the Recent Dictations submenu's feed. Pins the whole query
    /// contract against the same in-memory schema as the sibling above:
    /// the completed filter (empty transcripts never surface), newest-first
    /// ordering, and the caller's limit.
    #[test]
    fn recent_completed_entries_skip_empty_order_newest_first_and_limit() {
        let conn = setup_conn();

        // Empty history: no rows, which the tray renders as the localized
        // empty-state placeholder.
        let empty = HistoryManager::recent_completed_entries_with_conn(&conn, 5)
            .expect("query empty history");
        assert!(empty.is_empty(), "no rows => the tray renders the empty state");

        insert_entry(&conn, 100, "first", None);
        insert_entry(&conn, 300, "", None); // in-flight or failed: never listed
        insert_entry(&conn, 200, "second", Some("polished second"));
        insert_entry(&conn, 400, "third", None);
        insert_entry(&conn, 500, "fourth", None);

        // Newest first, empty transcript skipped, both text columns mapped.
        let entries = HistoryManager::recent_completed_entries_with_conn(&conn, 5)
            .expect("query recent completed entries");
        let timestamps: Vec<i64> = entries.iter().map(|e| e.timestamp).collect();
        assert_eq!(timestamps, vec![500, 400, 200, 100]);
        assert_eq!(entries[0].transcription_text, "fourth");
        assert_eq!(
            entries[1].post_processed_text.as_deref(),
            None,
            "entries[1] is 'third' (raw-only), not the polished one"
        );
        assert_eq!(entries[2].transcription_text, "second");
        assert_eq!(
            entries[2].post_processed_text.as_deref(),
            Some("polished second"),
            "the FINAL text columns ride along for the tray's copy action"
        );

        // The limit truncates from the newest end: asking for 2 keeps the
        // two newest completed entries.
        let limited = HistoryManager::recent_completed_entries_with_conn(&conn, 2)
            .expect("query limited recent completed entries");
        let timestamps: Vec<i64> = limited.iter().map(|e| e.timestamp).collect();
        assert_eq!(timestamps, vec![500, 400]);
    }

    /// Version-forward migration test: a database at the previous version
    /// (no model_id column) gains the column via the migration chain, the
    /// column stays nullable, and pre-existing rows are backfilled to ''
    /// (never left NULL).
    #[test]
    fn migration_adds_nullable_model_id_and_backfills_empty() {
        let mut conn = Connection::open_in_memory().expect("open in-memory db");

        // Build the previous database state: every migration except the
        // model_id one and the later pp-summary and provenance ones (which
        // must not run here either, or this row would gain those columns
        // too), tracked via user_version exactly like a live DB.
        let previous = Migrations::new(MIGRATIONS[..MIGRATIONS.len() - 3].to_vec());
        previous
            .to_latest(&mut conn)
            .expect("apply pre-model_id migrations");
        insert_entry(&conn, 100, "legacy entry", None);

        // Roll forward with the full chain; it continues from user_version.
        let latest = Migrations::new(MIGRATIONS.to_vec());
        latest
            .to_latest(&mut conn)
            .expect("apply latest migrations");

        let version: i32 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("read user_version");
        assert_eq!(version as usize, MIGRATIONS.len());

        // The column exists and is nullable...
        let notnull: i32 = conn
            .query_row(
                "SELECT [notnull] FROM pragma_table_info('transcription_history')
                 WHERE name = 'model_id'",
                [],
                |row| row.get(0),
            )
            .expect("query pragma_table_info for model_id");
        assert_eq!(notnull, 0, "model_id must remain a nullable column");

        // ...and the legacy row reads back as '' (backfilled), which the row
        // mapper surfaces as None ("unknown").
        let stored: Option<String> = conn
            .query_row(
                "SELECT model_id FROM transcription_history WHERE timestamp = 100",
                [],
                |row| row.get(0),
            )
            .expect("query legacy row");
        assert_eq!(stored.as_deref(), Some(""));
        let mapped = HistoryManager::get_latest_entry_with_conn(&conn)
            .expect("fetch latest entry")
            .expect("entry exists");
        assert_eq!(mapped.model_id, None);
    }

    /// Save path: the actually-used model id lands in the row. After a RAM
    /// auto-fallback the pipeline passes the fallback's id here - this pins
    /// that whatever id the pipeline threads through ends up persisted.
    #[test]
    fn save_path_records_the_actually_used_model_id() {
        let conn = setup_conn();

        let entry = HistoryManager::insert_entry_with_conn(
            &conn,
            "voxbar-200.wav".to_string(),
            "hello".to_string(),
            false,
            None,
            None,
            Some("whisper-large-v3-turbo".to_string()),
            None,
            "local".to_string(),
            "mac".to_string(),
            200,
            "Recording 200".to_string(),
        )
        .expect("insert entry with model");

        let in_row: Option<String> = conn
            .query_row(
                "SELECT model_id FROM transcription_history WHERE id = ?1",
                params![entry.id],
                |row| row.get(0),
            )
            .expect("query saved row");
        assert_eq!(in_row.as_deref(), Some("whisper-large-v3-turbo"));
        assert_eq!(entry.model_id.as_deref(), Some("whisper-large-v3-turbo"));

        // Unknown model (failed transcription saved for retry) writes as ''
        // - never NULL - and reads back as None.
        let unknown = HistoryManager::insert_entry_with_conn(
            &conn,
            "voxbar-300.wav".to_string(),
            String::new(),
            false,
            None,
            None,
            None,
            None,
            "local".to_string(),
            "mac".to_string(),
            300,
            "Recording 300".to_string(),
        )
        .expect("insert entry without model");

        let raw: Option<String> = conn
            .query_row(
                "SELECT model_id FROM transcription_history WHERE id = ?1",
                params![unknown.id],
                |row| row.get(0),
            )
            .expect("query saved row");
        assert_eq!(raw.as_deref(), Some(""), "unknown model is stored as ''");
        assert_eq!(unknown.model_id, None);
    }

    /// Version-forward migration test: a database at the previous version
    /// (no pp summary columns) gains the five columns via the migration
    /// chain, they are all nullable, and pre-existing rows read back with
    /// every new field as None (pre-cycle rows render unchanged).
    #[test]
    fn migration_adds_pp_columns_nullable_and_old_rows_default_none() {
        let mut conn = Connection::open_in_memory().expect("open in-memory db");

        // Build the previous database state: every migration except the pp
        // summary and provenance ones, tracked via user_version exactly
        // like a live DB.
        let previous = Migrations::new(MIGRATIONS[..MIGRATIONS.len() - 2].to_vec());
        previous
            .to_latest(&mut conn)
            .expect("apply pre-pp-summary migrations");
        insert_entry(&conn, 100, "legacy entry", None);

        // Roll forward with the full chain; it continues from user_version.
        let latest = Migrations::new(MIGRATIONS.to_vec());
        latest
            .to_latest(&mut conn)
            .expect("apply latest migrations");

        let version: i32 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("read user_version");
        assert_eq!(version as usize, MIGRATIONS.len());

        for column in [
            "post_process_provider",
            "post_process_model",
            "post_process_prompt_id",
            "post_process_outcome",
            "post_process_latency_ms",
        ] {
            let notnull: i32 = conn
                .query_row(
                    "SELECT [notnull] FROM pragma_table_info('transcription_history')
                     WHERE name = ?1",
                    params![column],
                    |row| row.get(0),
                )
                .unwrap_or_else(|e| panic!("query pragma_table_info for {column}: {e}"));
            assert_eq!(notnull, 0, "{column} must remain a nullable column");
        }

        // The legacy row maps back with every new field None.
        let mapped = HistoryManager::get_latest_entry_with_conn(&conn)
            .expect("fetch latest entry")
            .expect("entry exists");
        assert_eq!(mapped.transcription_text, "legacy entry");
        assert_eq!(mapped.post_process_provider, None);
        assert_eq!(mapped.post_process_model, None);
        assert_eq!(mapped.post_process_prompt_id, None);
        assert_eq!(mapped.post_process_outcome, None);
        assert_eq!(mapped.post_process_latency_ms, None);
    }

    /// Round-trip: a run summary (e.g. the local Qwen model with the
    /// Hinglish prompt) lands in the row and reads back field for field.
    #[test]
    fn pp_summary_round_trips() {
        let conn = setup_conn();
        let entry = HistoryManager::insert_entry_with_conn(
            &conn,
            "voxbar-400.wav".to_string(),
            "namaste duniya".to_string(),
            true,
            Some("namaste duniya.".to_string()),
            None,
            Some("whisper-large-v3-turbo".to_string()),
            Some(crate::post_process_runs::PostProcessRunSummary {
                provider_id: "local".to_string(),
                model: "Qwen/Qwen3-1.7B".to_string(),
                prompt_id: "hinglish-clean".to_string(),
                outcome: "applied".to_string(),
                latency_ms: 2340,
            }),
            "local".to_string(),
            "mac".to_string(),
            400,
            "Recording 400".to_string(),
        )
        .expect("insert entry with pp summary");

        assert_eq!(entry.post_process_provider.as_deref(), Some("local"));
        assert_eq!(entry.post_process_model.as_deref(), Some("Qwen/Qwen3-1.7B"));
        assert_eq!(
            entry.post_process_prompt_id.as_deref(),
            Some("hinglish-clean")
        );
        assert_eq!(entry.post_process_outcome.as_deref(), Some("applied"));
        assert_eq!(entry.post_process_latency_ms, Some(2340));

        let readback = HistoryManager::get_latest_entry_with_conn(&conn)
            .expect("fetch latest")
            .expect("entry exists");
        assert_eq!(readback.post_process_model, entry.post_process_model);
        assert_eq!(readback.post_process_outcome, entry.post_process_outcome);
        assert_eq!(
            readback.post_process_latency_ms,
            entry.post_process_latency_ms
        );
    }

    /// Version-forward migration test (v1.6.0 provenance columns): a
    /// database at the previous version (no capture_source /
    /// delivery_destination) gains both columns via the migration chain,
    /// they are NOT NULL with the legacy defaults, and pre-existing rows
    /// read back as local/mac.
    #[test]
    fn migration_adds_provenance_columns_and_defaults_legacy_rows() {
        let mut conn = Connection::open_in_memory().expect("open in-memory db");

        // Build the previous database state: every migration except the
        // provenance one, tracked via user_version exactly like a live DB.
        let previous = Migrations::new(MIGRATIONS[..MIGRATIONS.len() - 1].to_vec());
        previous
            .to_latest(&mut conn)
            .expect("apply pre-provenance migrations");
        insert_entry(&conn, 100, "legacy entry", None);

        // Roll forward with the full chain; it continues from user_version.
        let latest = Migrations::new(MIGRATIONS.to_vec());
        latest
            .to_latest(&mut conn)
            .expect("apply latest migrations");

        let version: i32 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("read user_version");
        assert_eq!(version as usize, MIGRATIONS.len());

        for column in ["capture_source", "delivery_destination"] {
            let notnull: i32 = conn
                .query_row(
                    "SELECT [notnull] FROM pragma_table_info('transcription_history')
                     WHERE name = ?1",
                    params![column],
                    |row| row.get(0),
                )
                .unwrap_or_else(|e| panic!("query pragma_table_info for {column}: {e}"));
            assert_eq!(notnull, 1, "{column} must be NOT NULL (it always has a value)");
        }

        // The legacy row reads back with the legacy defaults.
        let mapped = HistoryManager::get_latest_entry_with_conn(&conn)
            .expect("fetch latest entry")
            .expect("entry exists");
        assert_eq!(mapped.transcription_text, "legacy entry");
        assert_eq!(mapped.capture_source, "local");
        assert_eq!(mapped.delivery_destination, "mac");
    }

    /// Save path: the provenance pair lands in the row and reads back
    /// field for field, for both the local/mac and companion/phone shapes.
    #[test]
    fn save_path_records_capture_source_and_delivery_destination() {
        let conn = setup_conn();

        let companion = HistoryManager::insert_entry_with_conn(
            &conn,
            "voxbar-500.wav".to_string(),
            "from the phone".to_string(),
            false,
            None,
            None,
            Some("whisper-large-v3-turbo".to_string()),
            None,
            "companion".to_string(),
            "phone".to_string(),
            500,
            "Recording 500".to_string(),
        )
        .expect("insert companion entry");
        assert_eq!(companion.capture_source, "companion");
        assert_eq!(companion.delivery_destination, "phone");

        let row: (String, String) = conn
            .query_row(
                "SELECT capture_source, delivery_destination FROM transcription_history WHERE id = ?1",
                params![companion.id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("query saved row");
        assert_eq!(row, ("companion".to_string(), "phone".to_string()));

        // The plain keyboard shape round-trips identically.
        let local = HistoryManager::insert_entry_with_conn(
            &conn,
            "voxbar-600.wav".to_string(),
            "from the keyboard".to_string(),
            false,
            None,
            None,
            None,
            None,
            "local".to_string(),
            "mac".to_string(),
            600,
            "Recording 600".to_string(),
        )
        .expect("insert local entry");
        assert_eq!(local.capture_source, "local");
        assert_eq!(local.delivery_destination, "mac");
    }

    /// The phone-facing recent list (v1.6.0): companion entries only,
    /// newest first, capped at the limit, with the post-processed text
    /// preferred over the raw transcript, blank (failed-for-retry) entries
    /// skipped, and an empty history reading as an empty list.
    #[test]
    fn recent_companion_entries_exact_contract() {
        let conn = setup_conn();

        let insert = |id: i64, source: &str, raw: &str, processed: Option<&str>| {
            conn.execute(
                "INSERT INTO transcription_history (
                    file_name, timestamp, saved, title, transcription_text,
                    post_processed_text, post_process_prompt, post_process_requested,
                    capture_source, delivery_destination
                ) VALUES (?1, ?2, 0, ?3, ?4, ?5, NULL, 0, ?6, 'mac')",
                params![
                    format!("voxbar-{id}.wav"),
                    id,
                    format!("Recording {id}"),
                    raw,
                    processed,
                    source,
                ],
            )
            .expect("insert entry");
        };

        insert(100, "local", "keyboard session", None);
        insert(200, "companion", "raw two", Some("polished two"));
        insert(300, "companion", "raw three", None);
        insert(400, "companion", "", None); // failed session: skipped
        insert(500, "companion", "raw five", Some("polished five"));
        insert(600, "companion", "raw six", None);

        // Newest first, processed preferred, blanks skipped, local excluded.
        let entries = HistoryManager::recent_companion_entries_with_conn(&conn, 5)
            .expect("query recent companion entries");
        assert_eq!(
            entries
                .iter()
                .map(|e| (e.text.as_str(), e.timestamp))
                .collect::<Vec<_>>(),
            vec![
                ("raw six", 600),
                ("polished five", 500),
                ("raw three", 300),
                ("polished two", 200),
            ],
            "newest first; processed text wins; blank and local rows never appear"
        );

        // The limit window caps the list (the protocol's RecentHistory
        // frame carries 5).
        let capped = HistoryManager::recent_companion_entries_with_conn(&conn, 2)
            .expect("query capped");
        assert_eq!(
            capped.iter().map(|e| e.timestamp).collect::<Vec<_>>(),
            vec![600, 500]
        );

        // An empty history is an empty list, not an error.
        let empty_conn = setup_conn();
        assert!(HistoryManager::recent_companion_entries_with_conn(&empty_conn, 5)
            .expect("query empty history")
            .is_empty());
    }
}
