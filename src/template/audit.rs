//! 把 TemplateAudit 写入 SQLite `template_versions` 表(阶段 3)。
//!
//! cell_map_json / drawing_anchors_json 都是 JSON 字符串;
//! 模板版本变更(SHA256 不同)→ UPDATE,记录新指纹。

use crate::error::{Error, Result};
use crate::store::Store;
use crate::template::inspect::TemplateAudit;
use chrono::Utc;

pub async fn write_template_version(store: &Store, audit: &TemplateAudit) -> Result<()> {
    let cell_map_json = serde_json::to_string(&audit.cell_map)
        .map_err(|e| Error::Template(format!("serialize cell_map: {e}")))?;
    let drawing_anchors_json = serde_json::to_string(&audit.drawings)
        .map_err(|e| Error::Template(format!("serialize drawings: {e}")))?;
    let now = Utc::now().to_rfc3339();

    sqlx::query(
        "INSERT INTO template_versions
            (template_name, template_sha256, cell_map_json, drawing_anchors_json, last_validated_at, notes)
         VALUES (?, ?, ?, ?, ?, NULL)
         ON CONFLICT(template_name) DO UPDATE SET
            template_sha256 = excluded.template_sha256,
            cell_map_json = excluded.cell_map_json,
            drawing_anchors_json = excluded.drawing_anchors_json,
            last_validated_at = excluded.last_validated_at",
    )
    .bind(&audit.template_name)
    .bind(&audit.sha256)
    .bind(cell_map_json)
    .bind(drawing_anchors_json)
    .bind(&now)
    .execute(store.pool())
    .await
    .map_err(|e| Error::Template(format!("write template_versions: {e}")))?;

    Ok(())
}
