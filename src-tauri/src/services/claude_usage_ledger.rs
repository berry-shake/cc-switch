//! Durable Claude message identities, independent of the 30-day detail retention.
//! Migration only recovers IDs; it never recomputes costs or changes rollups.
use std::fs::File;
use std::io::{BufRead, BufReader, Read};

use rusqlite::{Connection, OptionalExtension};

use crate::database::{lock_conn, Database};
use crate::error::AppError;

const LEGACY_BEFORE: &str = "claude_usage_legacy_before";

pub(crate) fn remember(conn: &Connection, id: &str) -> Result<(), AppError> {
    conn.execute(
        "INSERT OR IGNORE INTO session_usage_dedup(data_source, request_id, semantic_id, has_entry_id)
         VALUES ('session_log', ?1, ?1, 1)", [id],
    )?;
    Ok(())
}

pub(crate) fn contains(conn: &Connection, id: &str) -> Result<bool, AppError> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM session_usage_dedup WHERE data_source='session_log' AND request_id=?1)",
        [id], |row| row.get(0),
    )?)
}

pub(crate) fn seed_detail_ids(conn: &Connection) -> Result<(), AppError> {
    // The Claude proxy uses the same session:<message.id> key as the importer.
    conn.execute(
        "INSERT OR IGNORE INTO session_usage_dedup(data_source, request_id, semantic_id, has_entry_id)
         SELECT 'session_log', request_id, request_id, 1 FROM proxy_request_logs
         WHERE app_type IN ('claude', 'claude-desktop') AND request_id LIKE 'session:%'", [],
    )?;
    Ok(())
}

pub(crate) fn migrate_legacy(conn: &Connection) -> Result<(), AppError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS claude_usage_legacy_cursors (
        file_path TEXT PRIMARY KEY, last_line_offset INTEGER NOT NULL,
        last_byte_offset INTEGER, last_tail_fingerprint INTEGER,
        last_synced_at INTEGER NOT NULL, completed INTEGER NOT NULL DEFAULT 0
    ); CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT);",
    )?;
    // The migration helpers also support partial legacy schemas used by recovery.
    if Database::has_column(conn, "proxy_request_logs", "app_type")? {
        seed_detail_ids(conn)?;
    }
    if Database::has_column(conn, "session_log_sync", "last_synced_at")? {
        let mut stmt = conn.prepare(
        "SELECT file_path, last_line_offset, last_byte_offset, last_tail_fingerprint, last_synced_at
         FROM session_log_sync
         WHERE last_tail_fingerprint IS NOT NULL OR replace(file_path, char(92), '/') LIKE '%/projects/%'",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Option<i64>>(2)?,
                row.get::<_, Option<i64>>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })?;
        for row in rows {
            let (path, lines, offset, fingerprint, synced) = row?;
            // A user's generic /projects/ workspace may also contain Codex/Pi
            // roots; do not classify those cursors as unknown Claude history.
            if fingerprint.is_some() || is_legacy_claude_path(&path) {
                conn.execute("INSERT OR IGNORE INTO claude_usage_legacy_cursors
                    (file_path, last_line_offset, last_byte_offset, last_tail_fingerprint, last_synced_at)
                    VALUES (?1, ?2, ?3, ?4, ?5)", rusqlite::params![path, lines, offset, fingerprint, synced])?;
            }
        }
    }
    // Already-pruned details cannot be reconstructed from rollups (proxy-only
    // requests may have no transcript). Protect just that historical date range.
    if Database::has_column(conn, "usage_daily_rollups", "app_type")? {
        conn.execute(
        "INSERT OR IGNORE INTO settings(key, value)
         SELECT ?1, CAST(strftime('%s', MAX(date), '+1 day', 'utc') AS TEXT)
         FROM usage_daily_rollups WHERE app_type IN ('claude', 'claude-desktop') HAVING COUNT(*) > 0",
        [LEGACY_BEFORE],
    )?;
    }
    Ok(())
}

fn is_legacy_claude_path(path: &str) -> bool {
    let path = path.replace('\\', "/");
    let Some((_, relative)) = path.rsplit_once("/projects/") else {
        return false;
    };
    if !relative.ends_with(".jsonl") {
        return false;
    }
    let parts: Vec<_> = relative.split('/').collect();
    matches!(
        parts.as_slice(),
        [_, _] | [_, _, "subagents", _] | [_, _, "subagents", "workflows", _, _]
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn legacy_cursor_detection_does_not_capture_other_apps_in_projects_workspace() {
        for path in [
            "/home/.claude/projects/-work/id.jsonl",
            "C:\\config\\projects\\-work\\session\\subagents\\agent.jsonl",
            "/work/projects/-p/id/subagents/workflows/wf_1/agent.jsonl",
        ] {
            assert!(super::is_legacy_claude_path(path), "{path}");
        }
        for path in [
            "/opt/projects/codex/sessions/2026/09/20/rollout.jsonl",
            "/opt/projects/pi/sessions/-work/session.jsonl",
            "/config/projects/p/session.json",
        ] {
            assert!(!super::is_legacy_claude_path(path), "{path}");
        }
    }
}

/// The guard also covers missing/rewritten legacy sources until their identities
/// can be recovered. Unknown historical requests are deferred, not charged twice.
pub(crate) fn protected_before(conn: &Connection) -> Result<Option<i64>, AppError> {
    let rollup: Option<String> = conn
        .query_row(
            "SELECT value FROM settings WHERE key=?1",
            [LEGACY_BEFORE],
            |r| r.get(0),
        )
        .optional()?;
    let pending: Option<i64> = conn.query_row(
        "SELECT MAX(last_synced_at) FROM claude_usage_legacy_cursors WHERE completed=0",
        [],
        |r| r.get(0),
    )?;
    Ok(rollup
        .and_then(|value| value.parse().ok())
        .into_iter()
        .chain(pending)
        .max())
}

pub(crate) fn bootstrap(db: &Database) -> Result<Vec<String>, AppError> {
    let pending = {
        let conn = lock_conn!(db.conn);
        let mut stmt = conn.prepare("SELECT file_path, last_line_offset, last_byte_offset, last_tail_fingerprint FROM claude_usage_legacy_cursors WHERE completed=0")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Option<i64>>(2)?,
                row.get::<_, Option<i64>>(3)?,
            ))
        })?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    let mut errors = Vec::new();
    for (path, lines, offset, fingerprint) in pending {
        match read_prefix_ids(&path, lines, offset, fingerprint) {
            Ok(ids) => {
                let conn = lock_conn!(db.conn);
                let tx = conn.unchecked_transaction()?;
                for id in ids { remember(&tx, &id)?; }
                tx.execute("UPDATE claude_usage_legacy_cursors SET completed=1 WHERE file_path=?1", [&path])?;
                tx.commit()?;
            }
            Err(error) => errors.push(format!("{path}: 旧用量去重账本尚未补齐（{error}），无法确认的历史请求暂停导入；可确认的新请求仍会导入")),
        }
    }
    Ok(errors)
}

fn read_prefix_ids(
    path: &str,
    lines: i64,
    offset: Option<i64>,
    fingerprint: Option<i64>,
) -> Result<Vec<String>, String> {
    if offset == Some(0) || (offset.is_none() && lines == 0) {
        return Ok(Vec::new());
    }
    let offset = offset
        .filter(|n| *n >= 0)
        .ok_or("旧行号游标没有可验证的字节指纹")?;
    let fingerprint = fingerprint.ok_or("旧游标没有可验证的字节指纹")?;
    let mut file = File::open(path).map_err(|e| e.to_string())?;
    if file.metadata().map_err(|e| e.to_string())?.len() < offset as u64 {
        return Err("文件已截断".into());
    }
    let tail =
        super::session_usage::read_tail_before(&mut file, offset).map_err(|e| e.to_string())?;
    if super::session_usage::claude_tail_fingerprint(&tail) != fingerprint {
        return Err("文件已改写".into());
    }
    use std::io::{Seek, SeekFrom};
    file.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
    let mut ids = std::collections::HashSet::new();
    let mut reader = BufReader::new(file.by_ref().take(offset as u64));
    let mut bytes = Vec::new();
    let mut read_bytes = 0;
    loop {
        bytes.clear();
        let read = reader
            .read_until(b'\n', &mut bytes)
            .map_err(|e| e.to_string())?;
        if read == 0 {
            break;
        }
        read_bytes += read as i64;
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            continue;
        };
        if value["type"] != "assistant" {
            continue;
        }
        let message = &value["message"];
        if ![
            "input_tokens",
            "output_tokens",
            "cache_read_input_tokens",
            "cache_creation_input_tokens",
        ]
        .iter()
        .any(|key| message["usage"][key].as_u64().unwrap_or(0) > 0)
        {
            continue;
        }
        if let Some(id) = message["id"].as_str() {
            ids.insert(format!("session:{id}"));
        }
    }
    // Detect truncation/rewrites while the prefix was being read.
    drop(reader);
    let tail =
        super::session_usage::read_tail_before(&mut file, offset).map_err(|e| e.to_string())?;
    if read_bytes != offset || super::session_usage::claude_tail_fingerprint(&tail) != fingerprint {
        return Err("读取期间文件发生变化".into());
    }
    Ok(ids.into_iter().collect())
}
