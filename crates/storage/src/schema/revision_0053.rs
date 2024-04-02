use std::str::FromStr;

use anyhow::Context;
use pathfinder_common::StarknetVersion;
use rusqlite::params;

use crate::params::RowExt;

pub(crate) fn migrate(tx: &rusqlite::Transaction<'_>) -> anyhow::Result<()> {
    tx.execute("ALTER TABLE block_headers ADD COLUMN version INTEGER", [])
        .context("Adding version column to block_headers")?;

    let mut stmt = tx.prepare("SELECT id, version FROM starknet_versions")?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let id: i64 = row.get(0)?;
        // Older starknet versions were stored as null, map those to empty string.
        let s = row.get_optional_str(1)?.unwrap_or_default().to_string();
        let version = StarknetVersion::from_str(&s).expect("invalid Starknet version");
        let version = version.as_u32();
        tx.execute(
            "UPDATE block_headers SET version = ? WHERE version_id = ?",
            params![version, id],
        )
        .context("Updating block_headers with version_id_new")?;
    }

    tx.execute("ALTER TABLE block_headers DROP COLUMN version_id", [])
        .context("Dropping version_id column from block_headers")?;
    tx.execute("DROP TABLE starknet_versions", [])
        .context("Dropping starknet_versions table")?;

    Ok(())
}
