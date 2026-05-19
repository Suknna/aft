use rusqlite::{params, Connection};

#[derive(Debug, Clone)]
pub struct BackupRow<'a> {
    pub backup_id: &'a str,
    pub harness: &'a str,
    pub session_id: &'a str,
    pub project_key: &'a str,
    pub op_id: Option<&'a str>,
    pub order: u128,
    pub file_path: &'a str,
    pub path_hash: &'a str,
    pub backup_path: Option<&'a str>,
    pub kind: &'a str,
    pub description: &'a str,
    pub created_at: i64,
    pub is_tombstone: bool,
}

pub fn upsert_backup(conn: &Connection, row: &BackupRow<'_>) -> rusqlite::Result<()> {
    let order_blob = row.order.to_be_bytes();

    conn.execute(
        "DELETE FROM backups
         WHERE harness = ?1 AND session_id = ?2 AND path_hash = ?3 AND order_blob = ?4",
        params![row.harness, row.session_id, row.path_hash, &order_blob[..]],
    )?;

    conn.execute(
        "INSERT INTO backups (
            backup_id, harness, session_id, project_key, op_id, order_blob, file_path,
            path_hash, backup_path, kind, description, created_at, is_tombstone
         ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6, ?7,
            ?8, ?9, ?10, ?11, ?12, ?13
         )",
        params![
            row.backup_id,
            row.harness,
            row.session_id,
            row.project_key,
            row.op_id,
            &order_blob[..],
            row.file_path,
            row.path_hash,
            row.backup_path,
            row.kind,
            row.description,
            row.created_at,
            row.is_tombstone,
        ],
    )?;

    Ok(())
}
