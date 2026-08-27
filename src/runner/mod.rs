//! 状态机服务层(阶段 6)
//!
//! 把"生成预览 / 确认 / 拒绝"三个动作封装成可由 Web 或 CLI 调用的函数。
//! 不持有运行时/调度(那是阶段 7 的 systemd oneshot+timer 干的)。
//!
//! 不变量(决策 #14 / #15):
//! - preview PDF 写入 `<output>/preview/...`,final PDF 写入 `<output>/final/...`
//! - confirm 时把 preview PDF **原子** rename 到 final;若 final 已存在,不覆盖,先失败
//! - 状态机更新用条件 `WHERE status = 'preview'`,受影响行数必须 == 1
//! - 失败路径只改 status='failed',不删 preview PDF,以便复盘
//!
//! 注:本阶段不实装 LNMS 拉数据 + 95th 计算 + 金额合计,只做"已有数据 → 渲染 → 状态机"
//! 的最小可用骨架;LNMS 拉数据与定价逻辑由后续阶段补(决策 #16 留口)。

use crate::error::{Error, Result};
use crate::render::{fill_template, replace_media_in_xlsx, xlsx_to_pdf, InvoiceData, PortLine};
use crate::store::{Customer, InvoiceStatus, Port, Store};
use std::path::PathBuf;

/// 渲染 + 落盘 + 状态机更新需要的全部上下文
#[derive(Clone)]
pub struct InvoiceService {
    store: Store,
    /// 模板根目录
    template_root: PathBuf,
    /// 输出根目录
    output_root: PathBuf,
    /// soffice 临时 UserInstallation
    soffice_profile: PathBuf,
}

impl InvoiceService {
    pub fn new(
        store: Store,
        template_root: PathBuf,
        output_root: PathBuf,
        soffice_profile: PathBuf,
    ) -> Self {
        Self {
            store,
            template_root,
            output_root,
            soffice_profile,
        }
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    /// 模板根目录路径(阶段 8f:模板上传 handler 用)
    pub fn template_root(&self) -> &std::path::Path {
        &self.template_root
    }

    /// 生成单个客户某月的预览账单(由调用方预先算好 95th 与总额)。
    ///
    /// 流程:
    /// 1. upsert_invoice_generating(状态 = generating)
    /// 2. 写 PNG(可选,调用方给原始序列)
    /// 3. fill_template 写 xlsx
    /// 4. replace_media_in_xlsx 替换 PNG
    /// 5. xlsx_to_pdf → preview PDF
    /// 6. update_invoice_preview(状态 = preview)
    /// 失败:status='failed',并把错误冒泡给调用方
    #[allow(clippy::too_many_arguments)]
    pub async fn generate_preview(
        &self,
        customer_id: i64,
        year: i64,
        month: i64,
        invoice_no: &str,
        template_name: &str,
        ports: Vec<PortLine>,
        total_cents: i64,
        chart_png: Option<Vec<u8>>,
    ) -> Result<i64> {
        let customer = self
            .store
            .find_customer_by_id(customer_id)
            .await?
            .ok_or_else(|| Error::NotFound(format!("customer {customer_id}")))?;

        // 1. upsert generating
        let snapshot = serde_json::json!({
            "customer_id": customer_id,
            "customer": customer.internal_key,
            "ports": ports.iter().map(|p| serde_json::json!({
                "label": p.label,
                "mbps_95th": p.mbps_95th,
                "machine_rent": p.machine_rent,
                "machine_hosting": p.machine_hosting,
            })).collect::<Vec<_>>(),
            "total_cents": total_cents,
            "year": year, "month": month,
        });
        let snapshot_json = serde_json::to_string(&snapshot)
            .map_err(|e| Error::Internal(format!("snapshot json: {e}")))?;
        let template_version = format!("file:{template_name}");
        let invoice_id = self
            .store
            .upsert_invoice_generating(
                customer_id,
                year,
                month,
                invoice_no,
                &template_version,
                &snapshot_json,
                &customer.currency,
            )
            .await?;

        let result = self
            .render_to_preview(&customer, &ports, template_name, invoice_no, chart_png.as_deref())
            .await;

        match result {
            Ok(preview_pdf) => {
                self.store
                    .update_invoice_preview(invoice_id, total_cents, preview_pdf.to_string_lossy().as_ref())
                    .await?;
                self.store
                    .record_action(invoice_id, "preview_generated", None, None)
                    .await?;
                Ok(invoice_id)
            }
            Err(e) => {
                // best-effort:把状态置为 failed
                let _ = self.store.update_invoice_failed(invoice_id).await;
                let _ = self
                    .store
                    .record_action(invoice_id, "failed", None, Some(&e.to_string()))
                    .await;
                Err(e)
            }
        }
    }

    async fn render_to_preview(
        &self,
        customer: &Customer,
        ports: &[PortLine],
        template_name: &str,
        invoice_no: &str,
        chart_png: Option<&[u8]>,
    ) -> Result<PathBuf> {
        // 0. 准备目录
        let template_path = self.template_root.join(template_name);
        let work_dir = self.output_root.join("work").join(invoice_no);
        std::fs::create_dir_all(&work_dir)?;

        // 1. 写 xlsx
        let xlsx_out = work_dir.join("filled.xlsx");
        let data = InvoiceData {
            invoice_no: invoice_no.into(),
            customer_name: customer.name.clone(),
            period_label: format!("{}", customer.id), // 占位:阶段 7 用 (year, month)
            ports: ports.to_vec(),
            total_cents: ports
                .iter()
                .filter_map(|p| p.mbps_95th) // 暂以 mbps 之和作为占位合计
                .sum::<i64>()
                * 100,
            currency: customer.currency.clone(),
        };
        fill_template(&template_path, &xlsx_out, "Invoice", &data)?;

        // 2. 替换 PNG
        if let Some(bytes) = chart_png {
            replace_media_in_xlsx(&xlsx_out, "image1.png", bytes)?;
        }

        // 3. soffice → PDF
        let preview_dir = self.output_root.join("preview");
        std::fs::create_dir_all(&preview_dir)?;
        let pdf_path = xlsx_to_pdf(&xlsx_out, &preview_dir, &self.soffice_profile)?;

        Ok(pdf_path)
    }

    /// 把 preview 提升为 final(原子)。
    /// 失败模式:final 已存在 → 拒绝,不动状态。
    pub async fn confirm(&self, invoice_id: i64, actor_user_id: i64) -> Result<PathBuf> {
        let inv = self
            .store
            .find_invoice(invoice_id)
            .await?
            .ok_or_else(|| Error::NotFound(format!("invoice {invoice_id}")))?;
        if inv.status != InvoiceStatus::Preview {
            return Err(Error::InvalidTransition(format!(
                "confirm from {} not allowed",
                inv.status.as_str()
            )));
        }
        let preview = inv
            .pdf_path_preview
            .as_ref()
            .ok_or_else(|| Error::InvalidTransition("preview PDF missing".into()))?;
        let preview_path = PathBuf::from(preview);
        if !preview_path.exists() {
            return Err(Error::NotFound(format!("preview pdf {}", preview)));
        }

        let final_dir = self.output_root.join("final");
        std::fs::create_dir_all(&final_dir)?;
        let stem = preview_path
            .file_stem()
            .ok_or_else(|| Error::Internal("bad preview filename".into()))?;
        let final_path = final_dir.join(format!("{}.pdf", stem.to_string_lossy()));
        if final_path.exists() {
            return Err(Error::AlreadyExists(format!(
                "final pdf {}",
                final_path.display()
            )));
        }

        // 同盘 atomic rename,跨盘 fallback to copy+remove
        match std::fs::rename(&preview_path, &final_path) {
            Ok(()) => {}
            Err(_) => {
                std::fs::copy(&preview_path, &final_path)?;
                std::fs::remove_file(&preview_path)?;
            }
        }

        self.store
            .update_invoice_confirmed(invoice_id, final_path.to_string_lossy().as_ref(), actor_user_id)
            .await?;
        self.store
            .record_action(invoice_id, "confirmed", Some(actor_user_id), None)
            .await?;
        Ok(final_path)
    }

    /// 拒绝某个 preview。
    pub async fn reject(
        &self,
        invoice_id: i64,
        actor_user_id: i64,
        reason: &str,
    ) -> Result<()> {
        let inv = self
            .store
            .find_invoice(invoice_id)
            .await?
            .ok_or_else(|| Error::NotFound(format!("invoice {invoice_id}")))?;
        if inv.status != InvoiceStatus::Preview {
            return Err(Error::InvalidTransition(format!(
                "reject from {} not allowed",
                inv.status.as_str()
            )));
        }
        self.store
            .update_invoice_rejected(invoice_id, reason)
            .await?;
        self.store
            .record_action(invoice_id, "rejected", Some(actor_user_id), Some(reason))
            .await?;
        Ok(())
    }

    /// 重生成:从当前已存在的发票读出 ports 与 chart 数据,再次跑完整流程。
    /// 仅对 preview/rejected/failed 状态允许。
    pub async fn regenerate(&self, invoice_id: i64) -> Result<i64> {
        let inv = self
            .store
            .find_invoice(invoice_id)
            .await?
            .ok_or_else(|| Error::NotFound(format!("invoice {invoice_id}")))?;
        if !matches!(
            inv.status,
            InvoiceStatus::Preview | InvoiceStatus::Rejected | InvoiceStatus::Failed
        ) {
            return Err(Error::InvalidTransition(format!(
                "regenerate from {} not allowed",
                inv.status.as_str()
            )));
        }
        self.store
            .record_action(invoice_id, "preview_regenerated", None, None)
            .await?;
        let template_name = inv
            .template_version
            .strip_prefix("file:")
            .unwrap_or("模板.xlsx")
            .to_string();

        // 从 snapshot 反推 ports(简化:total_cents 由调用方在阶段 8 接入)
        let ports = Vec::<PortLine>::new();
        let total_cents = inv.total_cents.unwrap_or(0);
        self.generate_preview(
            inv.customer_id,
            inv.period_year,
            inv.period_month,
            &inv.invoice_no,
            &template_name,
            ports,
            total_cents,
            None,
        )
        .await
    }
}

// 抑制 Port 引用警告(为阶段 8 留口子)
#[allow(dead_code)]
fn _phantom_port(_p: &Port) {}