use anyhow::{anyhow, Result};
use chrono::{DateTime, Local, Utc};
use log::{debug, error, info, warn};
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
    // v1.6.2 W1-L2: WHICH phone a companion entry came from (the hello
    // device name; '' for keyboard sessions and pre-migration rows). The
    // per-phone recent lists filter on it, so one phone's recents show
    // only its own dictations. NOT NULL with the legacy default, so every
    // pre-migration companion row reads '' and appears in no phone's list
    // (the honest truth: the schema recorded no device identity then).
    M::up(
        "ALTER TABLE transcription_history ADD COLUMN capture_device TEXT NOT NULL DEFAULT '';",
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
    /// v1.6.2 W1-L2: which companion device dictated this entry (the hello
    /// device name the session froze). "" for keyboard sessions and
    /// pre-migration rows. The per-phone recent lists filter on it.
    pub capture_device: String,
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

/// KB-179: what a launch-time database recovery wants the user to know.
/// Surfaced through the `get_history_db_recovery` command so the frontend
/// can post one persistent notice (the recovery happens before any webview
/// is listening, so a push would be missed; the pull is the reliable leg).
#[derive(Clone, Debug, Serialize, Deserialize, Type)]
pub struct HistoryDbRecovery {
    /// The file name the unreadable database was quarantined under (inside
    /// the app data dir), so the notice can name where the old data lives.
    pub quarantined_file: String,
}

/// Open (creating if needed) the history database at `db_path` and bring it
/// to the latest migration. Free of the manager so the KB-179 recovery
/// path and its unit tests can run it against a bare path (the corrupt
/// fixture needs no app handle).
fn init_database_at(db_path: &std::path::Path) -> Result<()> {
    let mut conn = Connection::open(db_path)?;

    // Handle migration from tauri-plugin-sql to rusqlite_migration
    // tauri-plugin-sql used _sqlx_migrations table, rusqlite_migration uses user_version pragma
    HistoryManager::migrate_from_tauri_plugin_sql(&conn)?;

    // Create migrations object and run to latest version
    let migrations = Migrations::new(MIGRATIONS.to_vec());

    // Validate migrations in debug builds
    #[cfg(debug_assertions)]
    migrations.validate().expect("Invalid migrations");

    // Get current version before migration
    let version_before: i32 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
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

/// Fleet pass 3 (the KB-179 quarantine scope): classify an init failure as
/// "the database file itself is unreadable" before the quarantine may
/// displace it. Only SQLite's own corruption verdicts qualify - NOTADB
/// (the file is not a SQLite database at all, the garbage-bytes fixture)
/// and CORRUPT. Everything else leaves a HEALTHY file at the live path: a
/// schema version written by a newer build (DatabaseTooFarAhead), a busy
/// or locked database, a transient I/O error. Renaming any of those aside
/// boots the app with an empty history and discards the user's data for
/// no reason, so the classifier fails closed toward NOT quarantining: an
/// unclassified error keeps the file in place and propagates (the visible
/// pre-KB-179 launch failure), because a false quarantine silently loses
/// data while a missed quarantine only fails one launch visibly.
fn init_error_is_corruption(error: &anyhow::Error) -> bool {
    for cause in error.chain() {
        // Version-class, named so the intent survives: a database this
        // binary's MIGRATIONS list does not know is exactly what a future
        // build wrote - healthy by definition.
        if matches!(
            cause.downcast_ref::<rusqlite_migration::Error>(),
            Some(rusqlite_migration::Error::MigrationDefinition(
                rusqlite_migration::MigrationDefinitionError::DatabaseTooFarAhead
            ))
        ) {
            return false;
        }
        if let Some(rusqlite::Error::SqliteFailure(ffi_err, _)) =
            cause.downcast_ref::<rusqlite::Error>()
        {
            if matches!(
                ffi_err.code,
                rusqlite::ErrorCode::NotADatabase | rusqlite::ErrorCode::DatabaseCorrupt
            ) {
                return true;
            }
        }
    }
    false
}

/// KB-179 (operator decision D3): move an unreadable history database out
/// of the way so the next open starts fresh. The first quarantine takes
/// the plain `history.db.corrupt` name; when that already exists (an
/// earlier corrupt file was kept) the timestamp suffix keeps both. A
/// stale rollback journal belonging to the dead database is removed so it
/// can never try to roll back into the fresh one. Returns the quarantine
/// path. Free of the manager so the corrupt-file fixture tests run
/// without an app handle.
fn quarantine_history_db(db_path: &std::path::Path, timestamp_secs: i64) -> Result<PathBuf> {
    let file_name = db_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow!("history database path has no file name"))?;

    let quarantine_path = if db_path
        .parent()
        .map(|dir| dir.join(format!("{}.corrupt", file_name)))
        .map(|plain| plain.exists())
        .unwrap_or(false)
    {
        db_path
            .parent()
            .ok_or_else(|| anyhow!("history database path has no parent dir"))?
            .join(format!("{}.corrupt.{}", file_name, timestamp_secs))
    } else {
        db_path
            .parent()
            .ok_or_else(|| anyhow!("history database path has no parent dir"))?
            .join(format!("{}.corrupt", file_name))
    };

    // The dead database's rollback journal, if one was left behind, belongs
    // to the file being quarantined: drop it rather than let SQLite consider
    // rolling it back into the fresh database that takes this path next.
    let journal_path = db_path
        .parent()
        .ok_or_else(|| anyhow!("history database path has no parent dir"))?
        .join(format!("{}-journal", file_name));
    if journal_path.exists() {
        let _ = fs::remove_file(&journal_path);
    }

    if db_path.exists() {
        fs::rename(db_path, &quarantine_path)?;
    }

    Ok(quarantine_path)
}

pub struct HistoryManager {
    app_handle: AppHandle,
    recordings_dir: PathBuf,
    db_path: PathBuf,
    /// KB-179: set when this launch found the stored history database
    /// unreadable, quarantined it, and started a fresh database. None on
    /// every healthy launch.
    db_recovery: Option<HistoryDbRecovery>,
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

        // KB-179: a corrupted or unwritable history.db used to panic the
        // app at every launch (the caller's expect). Operator decision D3:
        // on init failure quarantine the unreadable file, start a fresh
        // database, and record the recovery so a persistent notice can be
        // surfaced; only a genuinely unrecoverable app data dir (nothing
        // can be renamed or created in it) still fails construction.
        let mut db_recovery = None;
        if let Err(first_error) = init_database_at(&db_path) {
            // Fleet pass 3 (the KB-179 quarantine scope): the quarantine may
            // displace the file ONLY on a corruption verdict. Any other
            // failure leaves a healthy database at the live path - a newer
            // build's schema, a busy database, a transient I/O error - so the
            // error propagates and this launch fails visibly instead of
            // booting with empty history while the data sits renamed aside.
            if !init_error_is_corruption(&first_error) {
                return Err(first_error.context(
                    "history database init failed without a corruption verdict; keeping the file in place",
                ));
            }
            error!(
                "History database at {:?} failed to initialize ({}); quarantining it and starting a fresh database",
                db_path, first_error
            );
            let quarantined = quarantine_history_db(&db_path, Utc::now().timestamp())?;
            init_database_at(&db_path)?;
            db_recovery = Some(HistoryDbRecovery {
                quarantined_file: quarantined
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or_default()
                    .to_string(),
            });
            warn!(
                "Started a fresh history database; the unreadable one was kept as {:?}",
                quarantined
            );
        }

        let manager = Self {
            app_handle: app_handle.clone(),
            recordings_dir,
            db_path,
            db_recovery,
        };

        Ok(manager)
    }

    /// KB-179: the recovery record for this launch, if any (see
    /// [`HistoryDbRecovery`]).
    pub fn db_recovery(&self) -> Option<&HistoryDbRecovery> {
        self.db_recovery.as_ref()
    }

    /// Migrate from tauri-plugin-sql's migration tracking to rusqlite_migration's.
    /// tauri-plugin-sql used _sqlx_migrations table, while rusqlite_migration uses
    /// SQLite's user_version pragma. This function checks if the old system was in use
    /// and sets the user_version accordingly so migrations don't re-run.
    fn migrate_from_tauri_plugin_sql(conn: &Connection) -> Result<()> {
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
            capture_device: row.get("capture_device")?,
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
    /// is "mac" or "phone" (v1.6.0 provenance columns);
    /// `capture_device` is the companion hello device name (v1.6.2 W1-L2,
    /// '' for keyboard sessions).
    #[allow(clippy::too_many_arguments)] // the row's provenance triple rides with the text
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
        capture_device: String,
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
            capture_device,
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
    #[allow(clippy::too_many_arguments)] // the row's provenance triple rides with the text
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
        capture_device: String,
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
                delivery_destination,
                capture_device
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
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
                &capture_device,
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
            capture_device,
        })
    }

    /// The phone-facing recent list (v1.6.0): the last `limit`
    /// companion-source entries with non-empty deliverable text, newest
    /// first. The deliverable text is the post-processed transcript when
    /// the session polished it, else the raw transcription; entries whose
    /// deliverable text is blank (failed sessions saved for retry) are
    /// skipped - the phone list shows what was actually dictated.
    /// Phone-DESTINED transcripts are excluded from the device-agnostic
    /// list (the get_recent_companion_entries command's legacy shape):
    /// that list describes no single phone, and a phone-target dictation
    /// must reach only its own phone. Split on a connection so tests
    /// exercise the real query in memory.
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
               AND COALESCE(delivery_destination, 'mac') != 'phone'
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

    /// v1.6.2 W1-L2: ONE phone's own recent list, filtered by the device
    /// name its sessions froze (the hello name). Because the frame this
    /// feeds is delivered down THAT phone's targeted lane alone, its own
    /// phone-DESTINED dictations are INCLUDED here - they are private to
    /// it, and the v1.6.0 broadcast-era exclusion (which protected
    /// bystanders from seeing them) no longer applies to a per-lane list.
    /// Pre-migration companion rows read capture_device '' and appear in
    /// no phone's list (the schema recorded no device identity then).
    /// Split on a connection so tests exercise the real query in memory.
    fn recent_companion_entries_for_device_with_conn(
        conn: &Connection,
        device: &str,
        limit: usize,
    ) -> Result<Vec<RecentCompanionEntry>> {
        let mut stmt = conn.prepare(
            "SELECT COALESCE(NULLIF(post_processed_text, ''), transcription_text) AS deliverable,
                    timestamp
             FROM transcription_history
             WHERE capture_source = 'companion'
               AND capture_device = ?1
               AND TRIM(COALESCE(NULLIF(post_processed_text, ''), transcription_text)) != ''
             ORDER BY id DESC
             LIMIT ?2",
        )?;
        let entries = stmt
            .query_map(params![device, limit as i64], |row| {
                Ok(RecentCompanionEntry {
                    text: row.get("deliverable")?,
                    timestamp: row.get("timestamp")?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(entries)
    }

    /// The per-phone twin of [`Self::recent_companion_entries`] (v1.6.2
    /// W1-L2): that device's own dictations, newest first.
    pub fn recent_companion_entries_for_device(
        &self,
        device: &str,
        limit: usize,
    ) -> Result<Vec<RecentCompanionEntry>> {
        let conn = self.get_connection()?;
        Self::recent_companion_entries_for_device_with_conn(&conn, device, limit)
    }

    /// Update an existing history entry with new transcription results (used by retry).
    /// `model_id` is the model that produced this retry's transcription.
    /// `post_process_summary` is the retry's pp: run summary; the five
    /// summary columns are rewritten ATOMICALLY with the text (KB-023 +
    /// KB-240: the old update left the previous run's provider/model/
    /// prompt/outcome/latency on the row while swapping the text under
    /// them, so the chip described a run that no longer produced the
    /// entry; a retry whose leg did not run passes None and the columns
    /// clear to NULL instead of keeping the stale run).
    pub fn update_transcription(
        &self,
        id: i64,
        transcription_text: String,
        post_processed_text: Option<String>,
        post_process_prompt: Option<String>,
        model_id: Option<String>,
        post_process_summary: Option<crate::post_process_runs::PostProcessRunSummary>,
    ) -> Result<HistoryEntry> {
        let conn = self.get_connection()?;
        let entry = Self::update_transcription_with_conn(
            &conn,
            id,
            transcription_text,
            post_processed_text,
            post_process_prompt,
            model_id,
            post_process_summary,
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

    /// The `_with_conn` twin of [`Self::update_transcription`], so the
    /// KB-023/KB-240 column contract (all five summary columns move with
    /// the text, NULL when the leg did not run) is pinnable against an
    /// in-memory schema.
    #[allow(clippy::too_many_arguments)] // the retry's whole result rides together
    fn update_transcription_with_conn(
        conn: &Connection,
        id: i64,
        transcription_text: String,
        post_processed_text: Option<String>,
        post_process_prompt: Option<String>,
        model_id: Option<String>,
        post_process_summary: Option<crate::post_process_runs::PostProcessRunSummary>,
    ) -> Result<HistoryEntry> {
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
        let updated = conn.execute(
            "UPDATE transcription_history
             SET transcription_text = ?1,
                 post_processed_text = ?2,
                 post_process_prompt = ?3,
                 model_id = ?4,
                 post_process_provider = ?5,
                 post_process_model = ?6,
                 post_process_prompt_id = ?7,
                 post_process_outcome = ?8,
                 post_process_latency_ms = ?9
             WHERE id = ?10",
            params![
                transcription_text,
                post_processed_text,
                post_process_prompt,
                model_id.unwrap_or_default(),
                pp_provider,
                pp_model,
                pp_prompt_id,
                pp_outcome,
                pp_latency,
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
                capture_source, delivery_destination, capture_device
                 FROM transcription_history WHERE id = ?1",
                params![id],
                Self::map_history_entry,
            )?;

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

    /// Delete the given rows and their WAV files. KB-025: every row that
    /// leaves the database emits its own `Deleted { id }` event - the
    /// cleanup paths used to delete with zero emission while the frontend
    /// listener ignored 'deleted' anyway, so retention-cleaned rows lived
    /// on as ghosts in an already-open History tab. The event fires right
    /// after each row's delete (even when the WAV removal then fails: the
    /// row is gone either way).
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

            // KB-025: the row is gone; tell every listening surface now.
            if let Err(e) = (HistoryUpdatePayload::Deleted { id: *id }).emit(&self.app_handle) {
                error!("Failed to emit history-deleted event: {}", e);
            }

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
        let entries = Self::unsaved_entries_beyond_limit_with_conn(&conn, limit)?;

        if !entries.is_empty() {
            let deleted_count = self.delete_entries_and_files(&entries)?;

            if deleted_count > 0 {
                debug!("Cleaned up {} old history entries by count", deleted_count);
            }
        }

        Ok(())
    }

    /// KB-025: the count-based cleanup's selection, split onto a connection
    /// so the pass is unit-testable (previously NO cleanup test existed).
    /// Unsaved entries only, newest first; everything past the newest
    /// `limit` rows is selected for deletion.
    fn unsaved_entries_beyond_limit_with_conn(
        conn: &Connection,
        limit: usize,
    ) -> Result<Vec<(i64, String)>> {
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
            Ok(entries.split_off(limit))
        } else {
            Ok(Vec::new())
        }
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

        let entries_to_delete =
            Self::unsaved_entries_older_than_with_conn(&conn, cutoff_timestamp)?;

        let deleted_count = self.delete_entries_and_files(&entries_to_delete)?;

        if deleted_count > 0 {
            debug!(
                "Cleaned up {} old history entries based on retention period",
                deleted_count
            );
        }

        Ok(())
    }

    /// KB-025: the time-based cleanup's selection, split onto a connection
    /// so the pass is unit-testable. Unsaved entries strictly older than
    /// the cutoff are selected; saved entries are never touched.
    fn unsaved_entries_older_than_with_conn(
        conn: &Connection,
        cutoff_timestamp: i64,
    ) -> Result<Vec<(i64, String)>> {
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
        Ok(entries_to_delete)
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
                capture_source, delivery_destination, capture_device
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
                capture_source, delivery_destination, capture_device
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
                capture_source, delivery_destination, capture_device
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
                delivery_destination, capture_device
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
                delivery_destination, capture_device
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
                delivery_destination, capture_device
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
                delivery_destination, capture_device
             FROM transcription_history
             WHERE id = ?1",
        )?;

        let entry = stmt.query_row([id], Self::map_history_entry).optional()?;

        Ok(entry)
    }

    pub async fn delete_entry(&self, id: i64) -> Result<()> {
        let conn = self.get_connection()?;

        // Get the entry to find the file name (and remember whether it was
        // a companion row: deleting one must refresh the phones' recent
        // lists, or the deleted transcript stays rendered and copyable on
        // every paired phone until their next dictation or reconnect).
        let mut deleted_companion_row = false;
        if let Some(entry) = self.get_entry_by_id(id).await? {
            deleted_companion_row = entry.capture_source == "companion";
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

        // Refresh the paired phones' recent lists so a deleted transcript
        // disappears everywhere (best effort: the broadcast no-ops without
        // a running companion server, and a query failure logs without
        // failing the delete that already succeeded).
        if deleted_companion_row {
            crate::companion::broadcast_recent_history(&self.app_handle);
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
                delivery_destination TEXT NOT NULL DEFAULT 'mac',
                capture_device TEXT NOT NULL DEFAULT ''
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
        assert!(
            empty.is_empty(),
            "no rows => the tray renders the empty state"
        );

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
        // model_id one and the later pp-summary, provenance, and
        // capture_device ones (which must not run here either, or this row
        // would gain those columns too), tracked via user_version exactly
        // like a live DB.
        let previous = Migrations::new(MIGRATIONS[..MIGRATIONS.len() - 4].to_vec());
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
            String::new(),
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
            String::new(),
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
            String::new(),
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
        // provenance one (and the later capture_device one), tracked via
        // user_version exactly like a live DB.
        let previous = Migrations::new(MIGRATIONS[..MIGRATIONS.len() - 2].to_vec());
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
            assert_eq!(
                notnull, 1,
                "{column} must be NOT NULL (it always has a value)"
            );
        }

        // The legacy row reads back with the legacy defaults.
        let mapped = HistoryManager::get_latest_entry_with_conn(&conn)
            .expect("fetch latest entry")
            .expect("entry exists");
        assert_eq!(mapped.transcription_text, "legacy entry");
        assert_eq!(mapped.capture_source, "local");
        assert_eq!(mapped.delivery_destination, "mac");
    }

    /// Version-forward migration test (v1.6.2 W1-L2): a database at the
    /// previous version (no capture_device) gains the column via the
    /// migration chain, it is NOT NULL with the legacy default, and
    /// pre-existing companion rows read '' - belonging to no phone's
    /// recent list, the honest truth for rows the schema could not
    /// attribute.
    #[test]
    fn migration_adds_capture_device_and_legacy_rows_belong_to_no_phone() {
        let mut conn = Connection::open_in_memory().expect("open in-memory db");

        // Build the previous database state: every migration except the
        // capture_device one, tracked via user_version exactly like a live
        // DB.
        let previous = Migrations::new(MIGRATIONS[..MIGRATIONS.len() - 1].to_vec());
        previous
            .to_latest(&mut conn)
            .expect("apply pre-capture_device migrations");
        insert_entry(&conn, 100, "legacy entry", None);

        // Roll forward with the full chain; it continues from user_version.
        let latest = Migrations::new(MIGRATIONS.to_vec());
        latest
            .to_latest(&mut conn)
            .expect("apply latest migrations");

        let notnull: i32 = conn
            .query_row(
                "SELECT [notnull] FROM pragma_table_info('transcription_history')
                 WHERE name = 'capture_device'",
                [],
                |row| row.get(0),
            )
            .expect("query pragma_table_info for capture_device");
        assert_eq!(
            notnull, 1,
            "capture_device must be NOT NULL (it always has a value, '' for unknown)"
        );

        // The legacy row reads back with the legacy default and appears in
        // no phone's per-device list.
        let mapped = HistoryManager::get_latest_entry_with_conn(&conn)
            .expect("fetch latest entry")
            .expect("entry exists");
        assert_eq!(mapped.capture_device, "");
        let recents =
            HistoryManager::recent_companion_entries_for_device_with_conn(&conn, "Pixel 8", 5)
                .expect("query per-device recents");
        assert!(
            recents.is_empty(),
            "a legacy deviceless row must not appear in any phone's list"
        );
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
            "Pixel 8".to_string(),
            500,
            "Recording 500".to_string(),
        )
        .expect("insert companion entry");
        assert_eq!(companion.capture_source, "companion");
        assert_eq!(companion.delivery_destination, "phone");

        let row: (String, String) = conn
            .query_row(
                "SELECT capture_source, delivery_destination, capture_device FROM transcription_history WHERE id = ?1",
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
            String::new(),
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
    /// skipped, PHONE-DESTINED transcripts excluded (the recents frame is
    /// a broadcast; a phone-target dictation must reach only its own
    /// phone), and an empty history reading as an empty list.
    /// Fleet pass 2 wiring pin: deleting a COMPANION row must refresh the
    /// paired phones' recent lists (broadcast_recent_history), or the
    /// deleted transcript stays rendered and copyable on every phone until
    /// the next dictation or reconnect. The delete path needs a full
    /// manager plus app handle a conn-level test cannot mount, so the
    /// wiring is pinned against this committed source (the same convention
    /// as the coordinator's emit pins): the companion-source flag is
    /// captured from the entry BEFORE the row goes away, and the broadcast
    /// rides the successful delete.
    #[test]
    fn deleting_a_companion_entry_refreshes_the_phone_recents() {
        let source = include_str!("history.rs");
        let at = source
            .find("pub async fn delete_entry")
            .expect("delete_entry");
        let end = at
            + source[at..]
                .find("fn format_timestamp_title")
                .expect("the fn after delete_entry");
        let body = &source[at..end];
        let capture = body
            .find("entry.capture_source == \"companion\"")
            .expect("the delete captures whether the row was companion-source");
        let refresh = body
            .find("crate::companion::broadcast_recent_history(&self.app_handle)")
            .expect("the delete broadcasts the refreshed recents");
        assert!(
            capture < refresh,
            "the source flag must be captured before the row is deleted"
        );
        assert!(
            body.contains("if deleted_companion_row {"),
            "the refresh is scoped to companion rows (a local delete changes no phone list)"
        );
    }

    #[test]
    fn recent_companion_entries_exact_contract() {
        let conn = setup_conn();

        let insert = |id: i64, source: &str, raw: &str, processed: Option<&str>| {
            conn.execute(
                "INSERT INTO transcription_history (
                    file_name, timestamp, saved, title, transcription_text,
                    post_processed_text, post_process_prompt, post_process_requested,
                    capture_source, delivery_destination, capture_device
                ) VALUES (?1, ?2, 0, ?3, ?4, ?5, NULL, 0, ?6, 'mac', '')",
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

        // Phone-destined transcripts. The phone-target dictation already
        // reached exactly its own phone through the targeted lane; listing
        // it in the SHARED (device-agnostic) recents would hand it to every
        // bystander. (The destination column is NOT NULL, so no legacy NULL
        // rows exist.)
        for (id, dest) in [(700i64, "phone"), (800i64, "phone")] {
            conn.execute(
                "INSERT INTO transcription_history (
                    file_name, timestamp, saved, title, transcription_text,
                    post_processed_text, post_process_prompt, post_process_requested,
                    capture_source, delivery_destination, capture_device
                ) VALUES (?1, ?2, 0, ?3, ?4, NULL, NULL, 0, 'companion', ?5, '')",
                params![
                    format!("voxbar-{id}.wav"),
                    id,
                    format!("Recording {id}"),
                    format!("raw {id}"),
                    dest,
                ],
            )
            .expect("insert entry");
        }

        // Newest first, processed preferred, blanks skipped, local and
        // phone-destined rows excluded.
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
            "newest first; processed text wins; blank, local, and phone-destined rows never appear"
        );

        // The limit window caps the list (the protocol's RecentHistory
        // frame carries 5).
        let capped =
            HistoryManager::recent_companion_entries_with_conn(&conn, 2).expect("query capped");
        assert_eq!(
            capped.iter().map(|e| e.timestamp).collect::<Vec<_>>(),
            vec![600, 500]
        );

        // An empty history is an empty list, not an error.
        let empty_conn = setup_conn();
        assert!(
            HistoryManager::recent_companion_entries_with_conn(&empty_conn, 5)
                .expect("query empty history")
                .is_empty()
        );
    }

    /// v1.6.2 W1-L2: the per-phone recent list. One phone's list shows
    /// exactly ITS OWN dictations (filtered by the device name its
    /// sessions froze), INCLUDING its phone-destined ones (private to its
    /// targeted lane), and never another phone's - even a same-named
    /// legacy row (capture_device '') that no phone can claim.
    #[test]
    fn per_device_recents_show_each_phone_only_its_own_sessions() {
        let conn = setup_conn();

        let insert = |id: i64, source: &str, device: &str, dest: &str, raw: &str| {
            conn.execute(
                "INSERT INTO transcription_history (
                    file_name, timestamp, saved, title, transcription_text,
                    post_processed_text, post_process_prompt, post_process_requested,
                    capture_source, delivery_destination, capture_device
                ) VALUES (?1, ?2, 0, ?3, ?4, NULL, NULL, 0, ?5, ?6, ?7)",
                params![
                    format!("voxbar-{id}.wav"),
                    id,
                    format!("Recording {id}"),
                    raw,
                    source,
                    dest,
                    device,
                ],
            )
            .expect("insert entry");
        };

        insert(100, "companion", "Pixel 8", "mac", "pixel mac one");
        insert(200, "companion", "iPhone 15", "mac", "iphone mac two");
        insert(300, "companion", "Pixel 8", "phone", "pixel phone three");
        insert(400, "companion", "iPhone 15", "phone", "iphone phone four");
        insert(500, "local", "", "mac", "keyboard five");
        insert(600, "companion", "", "mac", "legacy deviceless six");

        // Pixel 8's list: its own two sessions, phone-destined included,
        // newest first; nobody else's.
        let pixel =
            HistoryManager::recent_companion_entries_for_device_with_conn(&conn, "Pixel 8", 5)
                .expect("query pixel recents");
        assert_eq!(
            pixel
                .iter()
                .map(|e| (e.text.as_str(), e.timestamp))
                .collect::<Vec<_>>(),
            vec![("pixel phone three", 300), ("pixel mac one", 100)],
            "a phone's recents include its own phone-destined dictations and exclude every other phone's"
        );

        // iPhone 15's list is disjoint.
        let iphone =
            HistoryManager::recent_companion_entries_for_device_with_conn(&conn, "iPhone 15", 5)
                .expect("query iphone recents");
        assert_eq!(
            iphone.iter().map(|e| e.timestamp).collect::<Vec<_>>(),
            vec![400, 200]
        );

        // The deviceless legacy row (capture_device '') belongs to no
        // phone, and a name nothing ever froze gets an empty list.
        let legacy = HistoryManager::recent_companion_entries_for_device_with_conn(&conn, "", 5)
            .expect("query legacy recents");
        assert!(
            !legacy.iter().any(|e| e.text.contains("keyboard")),
            "the keyboard session never appears in any phone list"
        );
        let nobody =
            HistoryManager::recent_companion_entries_for_device_with_conn(&conn, "iPad", 5)
                .expect("query empty recents");
        assert!(nobody.is_empty());

        // The limit caps the per-phone window too.
        let capped =
            HistoryManager::recent_companion_entries_for_device_with_conn(&conn, "Pixel 8", 1)
                .expect("query capped per-phone recents");
        assert_eq!(
            capped.iter().map(|e| e.timestamp).collect::<Vec<_>>(),
            vec![300]
        );
    }

    /// Fleet pass 1: the SAME-PLATFORM collision the fleet traced. The
    /// hello's human name is a UA platform token (two iPhones both hello
    /// "iPhone", every Chrome-Android phone "Linux"), so the identity the
    /// phone's sessions freeze is the id-tagged composite (see
    /// `companion::protocol::device_identity_name`). Two iPhones sharing
    /// the platform name must hold DISJOINT recent lists - private
    /// phone-destined dictations included - and the pre-fix world (both
    /// freezing the bare platform name) is exactly the shared-list leak
    /// this pins away: with distinct composites, neither phone's bare name
    /// matches the other's rows.
    #[test]
    fn same_platform_phones_hold_disjoint_recent_lists() {
        let conn = setup_conn();

        let insert = |id: i64, device: &str, dest: &str, raw: &str| {
            conn.execute(
                "INSERT INTO transcription_history (
                    file_name, timestamp, saved, title, transcription_text,
                    post_processed_text, post_process_prompt, post_process_requested,
                    capture_source, delivery_destination, capture_device
                ) VALUES (?1, ?2, 0, ?3, ?4, NULL, NULL, 0, 'companion', ?5, ?6)",
                params![
                    format!("voxbar-{id}.wav"),
                    id,
                    format!("Recording {id}"),
                    raw,
                    dest,
                    device,
                ],
            )
            .expect("insert entry");
        };

        // The real client shape: two iPhones, identity composites tagged
        // by each phone's persistent device id.
        insert(100, "iPhone#7f3a", "mac", "phone a mac dictation");
        insert(200, "iPhone#7f3a", "phone", "phone a private dictation");
        insert(300, "iPhone#c3d4", "mac", "phone b mac dictation");
        insert(400, "iPhone#c3d4", "phone", "phone b private dictation");

        let list_for = |device: &str| {
            HistoryManager::recent_companion_entries_for_device_with_conn(&conn, device, 5)
                .expect("query recents")
                .into_iter()
                .map(|e| e.text)
                .collect::<Vec<_>>()
        };

        // Phone A's list: its own two, its private phone-destined one
        // included, none of phone B's.
        assert_eq!(
            list_for("iPhone#7f3a"),
            vec!["phone a private dictation", "phone a mac dictation"]
        );
        // Phone B's list is disjoint.
        assert_eq!(
            list_for("iPhone#c3d4"),
            vec!["phone b private dictation", "phone b mac dictation"]
        );
        // A bare platform name (a pre-id client) matches no tagged phone's
        // rows: the leak the finding traced, where both iPhones froze
        // "iPhone" and shared one list, cannot re-form across the tag
        // boundary.
        assert!(list_for("iPhone").is_empty());
    }

    /// KB-179 (operator decision D3): a corrupt history.db must not take
    /// the app down with it. The quarantine seam renames the unreadable
    /// file to history.db.corrupt, the retry init at the same path
    /// succeeds (a fresh database), and the ORIGINAL bytes survive in the
    /// quarantine for inspection. Corrupt-file fixture: garbage bytes
    /// that make SQLite's first pragma fail as "file is not a database".
    #[test]
    fn corrupt_db_is_quarantined_and_a_fresh_one_starts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("history.db");
        let corrupt_bytes = b"this is definitely not a sqlite database, not even close";
        std::fs::write(&db_path, corrupt_bytes).expect("write corrupt fixture");

        // The pre-fix behavior this pins away: init on the corrupt file
        // errors (the caller used to expect() on exactly this).
        assert!(
            init_database_at(&db_path).is_err(),
            "the corrupt fixture must actually fail init"
        );

        let quarantined = quarantine_history_db(&db_path, 1_700_000_000)
            .expect("quarantine the corrupt database");
        assert_eq!(
            quarantined,
            dir.path().join("history.db.corrupt"),
            "the first quarantine takes the plain .corrupt name"
        );
        assert!(
            !db_path.exists(),
            "the corrupt file no longer sits at the live path"
        );
        assert_eq!(
            std::fs::read(&quarantined).expect("read quarantine"),
            corrupt_bytes.to_vec(),
            "the unreadable database is kept, not destroyed"
        );

        // The recovery's second leg: a fresh database initializes at the
        // same path and is queryable.
        init_database_at(&db_path).expect("fresh database initializes");
        let conn = Connection::open(&db_path).expect("open fresh database");
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM transcription_history", [], |row| {
                row.get(0)
            })
            .expect("query fresh database");
        assert_eq!(count, 0, "the fresh database starts empty");
    }

    /// Fleet pass 3 (the KB-179 quarantine scope): the classifier accepts
    /// ONLY SQLite's own corruption verdicts. A version-class error (a
    /// database this binary's MIGRATIONS does not know - what a future
    /// build wrote), a busy database, and an unclassified error must all
    /// read as "keep the file in place", because each of those leaves a
    /// healthy database at the live path.
    #[test]
    fn quarantine_classifier_only_accepts_a_corruption_verdict() {
        // ffi::Error::new maps SQLite's primary result codes.
        let not_a_db = anyhow::Error::new(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_NOTADB),
            None,
        ));
        assert!(
            init_error_is_corruption(&not_a_db),
            "SQLITE_NOTADB (the garbage-bytes fixture) is corruption"
        );
        let corrupt = anyhow::Error::new(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CORRUPT),
            None,
        ));
        assert!(
            init_error_is_corruption(&corrupt),
            "SQLITE_CORRUPT is corruption"
        );

        let too_far_ahead = anyhow::Error::new(rusqlite_migration::Error::MigrationDefinition(
            rusqlite_migration::MigrationDefinitionError::DatabaseTooFarAhead,
        ));
        assert!(
            !init_error_is_corruption(&too_far_ahead),
            "a newer build's schema version is a healthy database, never quarantined"
        );
        let busy = anyhow::Error::new(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
            None,
        ));
        assert!(
            !init_error_is_corruption(&busy),
            "a busy database is transient, never quarantined"
        );
        let locked = anyhow::Error::new(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_LOCKED),
            None,
        ));
        assert!(
            !init_error_is_corruption(&locked),
            "a locked database is transient, never quarantined"
        );
        assert!(
            !init_error_is_corruption(&anyhow::Error::msg("something else entirely")),
            "an unclassified error fails closed toward keeping the file"
        );
    }

    /// Fleet pass 3 (the KB-179 quarantine scope), end to end against a
    /// REAL database: a healthy file whose user_version exceeds this
    /// binary's MIGRATIONS list (written by a future build, then this one
    /// run against it) fails init with the version-class error, the
    /// classifier reads it as not-corruption, and the file must stay at
    /// the live path - the pre-fix code quarantined exactly this fixture
    /// and booted with empty history.
    #[test]
    fn newer_version_db_fails_init_but_is_never_quarantined() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("history.db");
        init_database_at(&db_path).expect("seed a healthy database");

        // Write one row so the fixture proves DATA would be lost.
        let conn = Connection::open(&db_path).expect("open seeded database");
        conn.execute(
            "INSERT INTO transcription_history (file_name, timestamp, saved, title, transcription_text)
             VALUES ('seed.wav', 0, 0, 'seed', 'the user history that must survive')",
            [],
        )
        .expect("seed row");
        conn.pragma_update(None, "user_version", MIGRATIONS.len() as i32 + 1)
            .expect("bump past this binary's migrations");
        drop(conn);

        let err = init_database_at(&db_path).expect_err("a too-far-ahead version must fail init");
        assert!(
            !init_error_is_corruption(&err),
            "the version-class failure must read as not-corruption (error was: {err})"
        );
        assert!(
            db_path.exists(),
            "the healthy database stays at the live path"
        );
        assert!(
            !dir.path().join("history.db.corrupt").exists(),
            "no quarantine file may appear"
        );
        let conn = Connection::open(&db_path).expect("reopen the kept database");
        let text: String = conn
            .query_row(
                "SELECT transcription_text FROM transcription_history",
                [],
                |row| row.get(0),
            )
            .expect("the kept database is still readable");
        assert_eq!(text, "the user history that must survive");
    }

    /// Fleet pass 3 (the KB-179 quarantine scope), source pin: HistoryManager
    /// ::new must gate the quarantine on the corruption classifier BEFORE
    /// any quarantine_history_db call - mounting an AppHandle in a unit
    /// test is not possible (same convention as utils.rs's cancel pins).
    #[test]
    fn new_gates_the_quarantine_on_the_corruption_classifier() {
        let source = include_str!("history.rs");
        let fn_at = source
            .find("pub fn new(app_handle: &AppHandle)")
            .expect("HistoryManager::new");
        let body_end = fn_at + source[fn_at..].find("\n    }\n").expect("fn end");
        let body = &source[fn_at..body_end];
        let gate_at = body.find("if !init_error_is_corruption(&first_error)").expect(
            "the quarantine decision must branch on the corruption classifier",
        );
        let quarantine_at = body
            .find("quarantine_history_db(&db_path")
            .expect("the quarantine call");
        assert!(
            gate_at < quarantine_at,
            "the classifier gate must run before the file is displaced"
        );
        assert!(
            body.contains("return Err(first_error.context("),
            "a non-corruption failure must propagate instead of quarantining"
        );
    }

    /// KB-179: when history.db.corrupt already exists (an earlier corrupt
    /// database was kept), the next quarantine takes the timestamped name
    /// so BOTH files survive.
    #[test]
    fn second_quarantine_takes_the_timestamped_suffix() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("history.db");
        std::fs::write(&db_path, b"first corrupt payload").expect("first fixture");
        std::fs::write(
            dir.path().join("history.db.corrupt"),
            b"older corrupt payload",
        )
        .expect("existing quarantine");

        let quarantined = quarantine_history_db(&db_path, 1_700_000_000).expect("quarantine");
        assert_eq!(
            quarantined,
            dir.path().join("history.db.corrupt.1700000000"),
            "the timestamped suffix keeps the older quarantine intact"
        );
        assert_eq!(
            std::fs::read(dir.path().join("history.db.corrupt")).expect("older quarantine"),
            b"older corrupt payload".to_vec(),
            "the earlier corrupt file is not overwritten"
        );
        assert_eq!(
            std::fs::read(&quarantined).expect("new quarantine"),
            b"first corrupt payload".to_vec()
        );
    }

    /// KB-179: a stale rollback journal left next to the dead database is
    /// dropped by the quarantine, so it can never try to roll back into
    /// the fresh database that takes the path next.
    #[test]
    fn quarantine_drops_the_stale_rollback_journal() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("history.db");
        std::fs::write(&db_path, b"corrupt payload").expect("fixture");
        let journal = dir.path().join("history.db-journal");
        std::fs::write(&journal, b"stale journal").expect("journal fixture");

        quarantine_history_db(&db_path, 1_700_000_000).expect("quarantine");
        assert!(!journal.exists(), "the stale journal is gone");
    }

    /// KB-179 wiring pin: `new()` must attempt the quarantine-and-retry
    /// instead of failing (the caller's expect panicked the whole app at
    /// every launch on a corrupt database). The manager needs a full app
    /// handle a unit test cannot mount headless, so the wiring is pinned
    /// against this committed source (the same convention as the
    /// companion-delete pin above): the init failure falls into
    /// quarantine + fresh init, and the recovery is recorded for the
    /// frontend's persistent notice.
    #[test]
    fn init_failure_quarantines_and_retries_instead_of_failing() {
        let source = include_str!("history.rs");
        let at = source
            .find("pub fn new(app_handle: &AppHandle)")
            .expect("new()");
        let end = at
            + source[at..]
                .find("pub fn db_recovery")
                .expect("the fn after new()");
        let body = &source[at..end];
        let first_init = body
            .find("if let Err(first_error) = init_database_at(&db_path)")
            .expect("the first init's failure is handled, not propagated");
        let quarantine = body
            .find("quarantine_history_db(&db_path, Utc::now().timestamp())?")
            .expect("the unreadable database is quarantined");
        let retry = body
            .find("init_database_at(&db_path)?;")
            .expect("a fresh database initializes at the same path");
        let record = body
            .find("db_recovery = Some(HistoryDbRecovery {")
            .expect("the recovery is recorded for the persistent notice");
        assert!(
            first_init < quarantine && quarantine < retry && retry < record,
            "the recovery order is: fail, quarantine, fresh init, record"
        );
    }

    /// KB-023 + KB-240: update_transcription rewrites the five pp summary
    /// columns ATOMICALLY with the text. A toggle-on retry (the leg ran,
    /// summary present) replaces all five values; a toggle-off retry (the
    /// leg did not run, summary None) clears all five to NULL - the chip
    /// on the row disappears instead of describing a run that no longer
    /// produced the entry.
    #[test]
    fn update_transcription_rewrites_all_five_summary_columns() {
        let conn = setup_conn();
        insert_entry(&conn, 100, "raw one", Some("polished one"));
        let id = conn.last_insert_rowid();
        // Seed the row with a PREVIOUS run's summary.
        conn.execute(
            "UPDATE transcription_history SET
                post_process_provider = 'openai',
                post_process_model = 'gpt-4o-mini',
                post_process_prompt_id = 'english_casual',
                post_process_outcome = 'applied',
                post_process_latency_ms = 2500
             WHERE id = ?1",
            params![id],
        )
        .expect("seed prior summary");

        // Toggle-on retry: the leg re-ran and produced a fresh summary.
        let updated = HistoryManager::update_transcription_with_conn(
            &conn,
            id,
            "raw one again".to_string(),
            Some("polished again".to_string()),
            Some("Polish this".to_string()),
            Some("large-v3".to_string()),
            Some(crate::post_process_runs::PostProcessRunSummary {
                provider_id: "anthropic".to_string(),
                model: "claude-3-haiku".to_string(),
                prompt_id: "english_formal".to_string(),
                outcome: "applied".to_string(),
                latency_ms: 1800,
            }),
        )
        .expect("update with summary");
        assert_eq!(updated.transcription_text, "raw one again");
        assert_eq!(updated.post_process_provider.as_deref(), Some("anthropic"));
        assert_eq!(
            updated.post_process_model.as_deref(),
            Some("claude-3-haiku")
        );
        assert_eq!(
            updated.post_process_prompt_id.as_deref(),
            Some("english_formal")
        );
        assert_eq!(updated.post_process_outcome.as_deref(), Some("applied"));
        assert_eq!(updated.post_process_latency_ms, Some(1800));

        // Toggle-off retry: the leg did not run, and ALL five columns
        // clear (the chip goes away).
        let cleared = HistoryManager::update_transcription_with_conn(
            &conn,
            id,
            "raw only".to_string(),
            None,
            None,
            Some("large-v3".to_string()),
            None,
        )
        .expect("update without summary");
        assert_eq!(cleared.post_process_provider, None);
        assert_eq!(cleared.post_process_model, None);
        assert_eq!(cleared.post_process_prompt_id, None);
        assert_eq!(cleared.post_process_outcome, None);
        assert_eq!(cleared.post_process_latency_ms, None);
    }

    /// KB-025: the count-based cleanup selects exactly the unsaved rows
    /// beyond the newest `limit` (newest first); saved rows never count
    /// against the limit and are never selected.
    #[test]
    fn count_cleanup_selects_unsaved_rows_beyond_the_limit() {
        let conn = setup_conn();
        for ts in 100..106 {
            insert_entry(&conn, ts, &format!("entry {ts}"), None);
        }
        // One saved row in the middle: immune to cleanup.
        conn.execute(
            "UPDATE transcription_history SET saved = 1 WHERE timestamp = 103",
            [],
        )
        .expect("save one row");

        let selected = HistoryManager::unsaved_entries_beyond_limit_with_conn(&conn, 3)
            .expect("select beyond limit");
        let selected_ids: Vec<i64> = selected.iter().map(|(id, _)| *id).collect();
        // Unsaved newest-first: 105, 104, 102 (103 is saved, immune),
        // 101, 100. Keeping the newest 3 unsaved (105, 104, 102) selects
        // 101 and 100.
        assert_eq!(
            selected_ids.len(),
            2,
            "only the unsaved rows past the limit are selected"
        );
        let file_names: Vec<&str> = selected.iter().map(|(_, f)| f.as_str()).collect();
        assert_eq!(
            file_names,
            vec!["voxbar-101.wav", "voxbar-100.wav"],
            "the OLDEST unsaved rows are the ones selected"
        );

        // At or below the limit: nothing is selected.
        assert!(
            HistoryManager::unsaved_entries_beyond_limit_with_conn(&conn, 5)
                .expect("select at limit")
                .is_empty()
        );
    }

    /// KB-025: the time-based cleanup selects only UNSAVED entries older
    /// than the cutoff; saved entries of any age and unsaved entries on
    /// the fresh side of the cutoff stay.
    #[test]
    fn time_cleanup_selects_only_old_unsaved_rows() {
        let conn = setup_conn();
        let now = 1_700_000_000;
        insert_entry(&conn, now - 100, "recent unsaved", None);
        insert_entry(&conn, now - 10_000, "old unsaved", None);
        insert_entry(&conn, now - 10_000, "old saved", None);
        conn.execute(
            "UPDATE transcription_history SET saved = 1 WHERE transcription_text = 'old saved'",
            [],
        )
        .expect("save the old row");

        let selected = HistoryManager::unsaved_entries_older_than_with_conn(&conn, now - 1_000)
            .expect("select older than cutoff");
        assert_eq!(selected.len(), 1, "only the old unsaved row is selected");
        assert_eq!(selected[0].1, format!("voxbar-{}.wav", now - 10_000));
    }

    /// KB-025 wiring pin: the delete path the cleanup loops share emits a
    /// per-id Deleted event AFTER each row leaves the table (so the open
    /// History tab's listener can drop the ghost row). Needs a full
    /// manager + app handle a conn-level test cannot mount, so pinned
    /// against this committed source like the companion-delete pin above.
    #[test]
    fn cleanup_delete_path_emits_per_id_deleted_events() {
        let source = include_str!("history.rs");
        let at = source
            .find("fn delete_entries_and_files")
            .expect("delete_entries_and_files");
        let end = at
            + source[at..]
                .find("fn cleanup_by_count")
                .expect("the fn after delete_entries_and_files");
        let body = &source[at..end];
        let row_delete = body
            .find("DELETE FROM transcription_history WHERE id = ?1")
            .expect("the row delete");
        let emit = body
            .find("HistoryUpdatePayload::Deleted { id: *id }")
            .expect("the per-id Deleted event");
        assert!(
            row_delete < emit,
            "the event fires after the row is actually gone"
        );
    }
}
