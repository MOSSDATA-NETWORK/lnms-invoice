//! 阶段 5 渲染测试
//!
//! - fill_template:写入 INVOICE NO / 客户 / 端口行后,umya 读出来断言
//! - replace_media_in_xlsx:注入新 PNG 后,zip 读出 media SHA 与新 bytes 一致
//! - xlsx_to_pdf:在 soffice 不可用时跳过(只 emit warning,不算失败)

use lnms_invoice::render::{fill_template, replace_media_in_xlsx, InvoiceData, PortLine};
use lnms_invoice::template::inspect;
use sha2::{Digest, Sha256};
use tempfile::tempdir;
use umya_spreadsheet::Workbook;

fn build_template(dir: &std::path::Path) -> std::path::PathBuf {
    let path = dir.join("template.xlsx");
    let mut book = Workbook::default();
    let sheet = book.new_sheet("Invoice").expect("new sheet");
    sheet.cell_mut("D2"); // 预创建,后面 fill 才不会 set 时 panic
    sheet.cell_mut("D3");
    sheet.cell_mut("D4");
    sheet.cell_mut("D5");
    sheet.cell_mut("D6");
    sheet.cell_mut("A9");
    sheet.cell_mut("B9");
    sheet.cell_mut("C9");
    sheet.cell_mut("D9");
    sheet.cell_mut("E9");
    sheet.cell_mut("F9");

    umya_spreadsheet::writer::xlsx::write(&book, &path).expect("write template");

    // 注入占位 PNG
    let png: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x62, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];
    inject_png(&path, "xl/media/image1.png", png);
    path
}

fn inject_png(xlsx_path: &std::path::Path, target: &str, png: &[u8]) {
    use std::io::{Read, Write};
    let tmp = xlsx_path.with_extension("inject.tmp.xlsx");
    {
        let src = std::fs::File::open(xlsx_path).unwrap();
        let mut zip_in = zip::ZipArchive::new(std::io::BufReader::new(src)).unwrap();
        let dst = std::fs::File::create(&tmp).unwrap();
        let mut zip_out = zip::ZipWriter::new(dst);
        let opts = zip::write::FileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for i in 0..zip_in.len() {
            let mut entry = zip_in.by_index(i).unwrap();
            let name = entry.name().to_string();
            zip_out.start_file(&name, opts).unwrap();
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf).unwrap();
            zip_out.write_all(&buf).unwrap();
        }
        zip_out.start_file(target, opts).unwrap();
        zip_out.write_all(png).unwrap();
        zip_out.finish().unwrap();
    }
    std::fs::rename(&tmp, xlsx_path).unwrap();
}

fn sha256(b: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(b);
    h.finalize()
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect()
}

#[test]
fn test_fill_template_writes_invoice_and_ports() {
    let dir = tempdir().unwrap();
    let template = build_template(dir.path());
    let output = dir.path().join("filled.xlsx");

    let data = InvoiceData {
        invoice_no: "INV-2026-08-0001".into(),
        customer_name: "湖南XX网络".into(),
        period_label: "2026-08".into(),
        ports: vec![
            PortLine {
                label: "华为BGP 3段".into(),
                mbps_95th: Some(850),
                machine_rent: false,
                machine_hosting: true,
            },
            PortLine {
                label: "联通BGP 1段".into(),
                mbps_95th: Some(420),
                machine_rent: true,
                machine_hosting: false,
            },
        ],
        total_yuan: 12700.00, // ¥12,700.00
        currency: "CNY".into(),
    };

    fill_template(&template, &output, "Invoice", &data).expect("fill");

    // 读回断言
    let audit = inspect(&output, "filled").expect("inspect");
    let lookup = |cell: &str| -> Option<String> {
        audit
            .cell_map
            .iter()
            .find(|c| c.cell == cell)
            .and_then(|c| c.value.clone())
    };

    assert_eq!(lookup("D2").as_deref(), Some("INV-2026-08-0001"));
    assert_eq!(lookup("D3").as_deref(), Some("湖南XX网络"));
    assert_eq!(lookup("D4").as_deref(), Some("2026-08"));
    assert_eq!(lookup("D5").as_deref(), Some("12700"));
    assert_eq!(lookup("D6").as_deref(), Some("CNY"));
    assert_eq!(lookup("A9").as_deref(), Some("华为BGP 3段"));
    assert_eq!(lookup("B9").as_deref(), Some("850"));
    assert_eq!(lookup("A10").as_deref(), Some("联通BGP 1段"));
    assert_eq!(lookup("B10").as_deref(), Some("420"));
}

#[test]
fn test_replace_media_in_xlsx_swaps_png_bytes() {
    let dir = tempdir().unwrap();
    let xlsx = build_template(dir.path());

    let new_png: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x02, 0x08, 0x06, 0x00, 0x00, 0x00, 0xF4,
        0x78, 0xD4, 0xFA, // CRC for 2x2 IHDR
        0x00, 0x00, 0x00, 0x16, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x62, 0xFC, 0xCF, 0xC0, 0xF0,
        0x9F, 0x81, 0x81, 0x21, 0x00, 0x04, 0x00, 0x01, 0x5C, 0xCD, 0xFF, 0x69, 0x00, 0x00, 0x00,
        0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];
    let new_sha = sha256(new_png);

    replace_media_in_xlsx(&xlsx, "image1.png", new_png).expect("replace");

    // 读回 zip,断言 media bytes 是新 png
    let f = std::fs::File::open(&xlsx).unwrap();
    let mut zip = zip::ZipArchive::new(std::io::BufReader::new(f)).unwrap();
    let mut entry = zip.by_name("xl/media/image1.png").unwrap();
    let mut buf = Vec::new();
    std::io::Read::read_to_end(&mut entry, &mut buf).unwrap();
    assert_eq!(sha256(&buf), new_sha);
}

#[test]
fn test_replace_media_adds_when_missing() {
    let dir = tempdir().unwrap();
    let xlsx = dir.path().join("no_media.xlsx");
    let mut book = Workbook::default();
    book.new_sheet("S").expect("new sheet");
    umya_spreadsheet::writer::xlsx::write(&book, &xlsx).expect("write");

    let png = b"placeholder-bytes";
    replace_media_in_xlsx(&xlsx, "new.png", png).expect("replace");

    let f = std::fs::File::open(&xlsx).unwrap();
    let mut zip = zip::ZipArchive::new(std::io::BufReader::new(f)).unwrap();
    let mut entry = zip.by_name("xl/media/new.png").unwrap();
    let mut buf = Vec::new();
    std::io::Read::read_to_end(&mut entry, &mut buf).unwrap();
    assert_eq!(buf, png);
}
