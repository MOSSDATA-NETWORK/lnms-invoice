//! OOXML 字节级 PNG 替换(决策 #9)
//!
//! 不动 `xl/drawings/drawing*.xml`,直接改写 `xl/media/<file>` 的字节。
//! 这样 xlsx 内部对 drawing 的引用(media_path、rId)保持不变。

use crate::error::{Error, Result};
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;

/// 把 xlsx 内 `xl/media/<file>` 替换为新字节(其他 entry 原样保留)。
pub fn replace_media_in_xlsx(
    xlsx_path: &Path,
    media_filename: &str,
    new_bytes: &[u8],
) -> Result<()> {
    let media_full = format!("xl/media/{media_filename}");

    let tmp = xlsx_path.with_extension("replace.tmp.xlsx");

    {
        let src = File::open(xlsx_path)?;
        let mut zip_in = zip::ZipArchive::new(BufReader::new(src))
            .map_err(|e| Error::Template(format!("open xlsx as zip: {e}")))?;
        let dst = File::create(&tmp)?;
        let mut zip_out = zip::ZipWriter::new(BufWriter::new(dst));

        let opts = zip::write::FileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);

        let mut replaced = false;
        for i in 0..zip_in.len() {
            let mut entry = zip_in
                .by_index(i)
                .map_err(|e| Error::Template(format!("read zip entry {i}: {e}")))?;
            let name = entry.name().to_string();

            if name == media_full {
                zip_out
                    .start_file(&name, opts)
                    .map_err(|e| Error::Template(format!("start {name}: {e}")))?;
                zip_out
                    .write_all(new_bytes)
                    .map_err(|e| Error::Template(format!("write {name}: {e}")))?;
                replaced = true;
            } else {
                zip_out
                    .start_file(&name, opts)
                    .map_err(|e| Error::Template(format!("start {name}: {e}")))?;
                let mut buf = Vec::with_capacity(entry.size() as usize);
                entry
                    .read_to_end(&mut buf)
                    .map_err(|e| Error::Template(format!("read {name}: {e}")))?;
                zip_out
                    .write_all(&buf)
                    .map_err(|e| Error::Template(format!("copy {name}: {e}")))?;
            }
        }

        if !replaced {
            // entry 不存在则补一个
            zip_out
                .start_file(&media_full, opts)
                .map_err(|e| Error::Template(format!("start new {media_full}: {e}")))?;
            zip_out
                .write_all(new_bytes)
                .map_err(|e| Error::Template(format!("write new {media_full}: {e}")))?;
        }

        zip_out
            .finish()
            .map_err(|e| Error::Template(format!("finish zip: {e}")))?;
    }

    std::fs::rename(&tmp, xlsx_path)?;
    Ok(())
}
