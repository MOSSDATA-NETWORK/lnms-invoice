//! soffice → PDF(阶段 5)
//!
//! 用 `--headless --convert-to pdf` 调用 LibreOffice;独立 UserInstallation 目录
//! 避免与用户正在运行的 soffice 冲突。
//!
//! 阶段 7 的部署脚本会装 LibreOffice + 字体(Noto CJK) + 创建 soffice 用户;
//! 本模块只在 soffice 可用时工作。

use crate::error::{Error, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// xlsx → pdf。返回 pdf 路径(`output_dir/<input_stem>.pdf`)。
pub fn xlsx_to_pdf(xlsx_path: &Path, output_dir: &Path, user_profile_dir: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(output_dir)?;
    std::fs::create_dir_all(user_profile_dir)?;

    let soffice = which_soffice().ok_or_else(|| {
        Error::Template(
            "soffice not found in PATH; install LibreOffice (apt: libreoffice-core)"
                .to_string(),
        )
    })?;

    let status = Command::new(&soffice)
        .arg("--headless")
        .arg("--norestore")
        .arg("--nologo")
        .arg("--nofirststartwizard")
        .arg(format!(
            "-env:UserInstallation=file://{}",
            user_profile_dir.display()
        ))
        .arg("--convert-to")
        .arg("pdf")
        .arg("--outdir")
        .arg(output_dir)
        .arg(xlsx_path)
        .status()
        .map_err(|e| Error::Template(format!("spawn soffice: {e}")))?;

    if !status.success() {
        return Err(Error::Template(format!(
            "soffice exit {status} for {}",
            xlsx_path.display()
        )));
    }

    let stem = xlsx_path.file_stem().ok_or_else(|| {
        Error::Template(format!("bad xlsx filename: {}", xlsx_path.display()))
    })?;
    let pdf = output_dir.join(format!("{}.pdf", stem.to_string_lossy()));
    if !pdf.exists() {
        return Err(Error::Template(format!(
            "soffice reported success but PDF not found: {}",
            pdf.display()
        )));
    }
    Ok(pdf)
}

fn which_soffice() -> Option<PathBuf> {
    let candidates = [
        "/usr/bin/soffice",
        "/usr/lib/libreoffice/program/soffice",
        "/opt/homebrew/bin/soffice",
    ];
    for c in candidates {
        let p = PathBuf::from(c);
        if p.exists() {
            return Some(p);
        }
    }
    // 退化:从 PATH 找
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let p = dir.join("soffice");
            if p.exists() {
                return Some(p);
            }
        }
    }
    None
}
