//! One file operation per channel: list a directory, download or upload a file.

use std::time::UNIX_EPOCH;

use anyhow::Context as _;
use irmm_proto::{FileEntry, FilesReply, FilesRequest, read_frame, write_frame};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use super::{BoxRead, BoxWrite};

pub async fn serve(mut r: BoxRead, mut w: BoxWrite) -> anyhow::Result<()> {
    let req: FilesRequest = read_frame(&mut r).await?.context("no request")?;
    if let Err(e) = handle(req, &mut r, &mut w).await {
        write_frame(&mut w, &FilesReply::Error { message: format!("{e:#}") }).await?;
    }
    w.shutdown().await?;
    Ok(())
}

async fn handle(req: FilesRequest, r: &mut BoxRead, w: &mut BoxWrite) -> anyhow::Result<()> {
    match req {
        FilesRequest::List { path } => {
            let path = if path.is_empty() { default_root() } else { path };
            let mut entries = Vec::new();
            let mut dir = tokio::fs::read_dir(&path).await.with_context(|| format!("read {path}"))?;
            while let Some(e) = dir.next_entry().await? {
                let Ok(meta) = e.metadata().await else { continue };
                entries.push(FileEntry {
                    name: e.file_name().to_string_lossy().into_owned(),
                    is_dir: meta.is_dir(),
                    size: meta.len(),
                    modified: meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs()),
                });
            }
            entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then(a.name.to_lowercase().cmp(&b.name.to_lowercase())));
            write_frame(w, &FilesReply::List { entries }).await
        }
        FilesRequest::Download { path } => {
            let mut f = tokio::fs::File::open(&path).await.with_context(|| format!("open {path}"))?;
            let size = f.metadata().await?.len();
            write_frame(w, &FilesReply::Download { size }).await?;
            tokio::io::copy(&mut (&mut f).take(size), w).await?;
            w.flush().await?;
            Ok(())
        }
        FilesRequest::Upload { path, size } => {
            let tmp = format!("{path}.irmm-partial");
            let mut f = tokio::fs::File::create(&tmp).await.with_context(|| format!("create {path}"))?;
            let n = tokio::io::copy(&mut r.take(size), &mut f).await?;
            f.flush().await?;
            drop(f);
            if n != size {
                let _ = tokio::fs::remove_file(&tmp).await;
                anyhow::bail!("upload truncated: {n} of {size} bytes");
            }
            tokio::fs::rename(&tmp, &path).await?;
            write_frame(w, &FilesReply::Done).await
        }
    }
}

fn default_root() -> String {
    if cfg!(windows) { "C:\\".into() } else { "/".into() }
}
