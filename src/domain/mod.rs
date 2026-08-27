//! 业务模型。
//!
//! 阶段 1 骨架:占位。阶段 2-6 实装:
//! - `Customer` / `Port` / `Rate` / `Bill` / `InvoiceDraft` / `Invoice`
//! - 业务校验(币种、时区、日期序列号转换)
//! - 模板哈希 + 单元格映射(决策 #20,数据血缘)

/// 客户(精简版;完整字段阶段 2 填充)
#[derive(Debug, Clone)]
pub struct Customer {
    pub id: i64,
    pub name: String,
    pub currency: String,
    pub timezone: String,
}
