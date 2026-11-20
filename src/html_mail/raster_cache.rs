use std::{fs::OpenOptions, os::unix::fs::OpenOptionsExt, path::Path};

use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};

use super::*;

const CACHE_BYTES: i64 = 128 * 1024 * 1024;
const MAX_ENTRIES: i64 = 256;

pub async fn render_cached(
    document: Document,
    width: u32,
    density: u32,
    path: &Path,
) -> Result<Page> {
    // Hash the sanitized, fully inlined document, including downloaded image
    // bytes. Failed/missing images therefore cannot poison the completed view.
    let key = key(&document, width, density);
    let (read_path, read_key) = (path.to_owned(), key.clone());
    let snapshot = tokio::task::spawn_blocking(move || load(&read_path, &read_key)).await;
    if let Ok(Ok(Some(snapshot))) = snapshot {
        // A corrupt cache is disposable. Preserve the input for a fresh render.
        let for_decode = Document {
            html: String::new(),
            animated: document.animated.clone(),
            missing: document.missing.clone(),
        };
        if let Ok(mut page) = snapshot.decode(for_decode).await {
            page.snapshot = None;
            return Ok(page);
        }
    }
    let mut page = render(document, width, density).await?;
    if let Some(snapshot) = page.snapshot.take() {
        let path = path.to_owned();
        // Cache failure must never prevent opening a message.
        let _ = tokio::task::spawn_blocking(move || save(&path, &key, snapshot)).await;
    }
    Ok(page)
}

fn key(document: &Document, width: u32, density: u32) -> Vec<u8> {
    let mut hash = Sha256::new();
    hash.update(b"tandem-raster-v1");
    hash.update(width.to_le_bytes());
    hash.update(density.to_le_bytes());
    hash.update(document.html.as_bytes());
    hash.finalize().to_vec()
}

fn open(path: &Path) -> Result<Connection> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)?;
    let db = Connection::open(path)?;
    db.busy_timeout(Duration::from_millis(50))?;
    // This database contains only regenerable raster output. Keep mail.sqlite
    // and its sync/compose transactions completely separate.
    db.execute_batch("PRAGMA journal_mode=MEMORY; PRAGMA synchronous=OFF;
        CREATE TABLE IF NOT EXISTS pages (key BLOB PRIMARY KEY, meta TEXT NOT NULL, png BLOB NOT NULL, used INTEGER NOT NULL);")?;
    Ok(db)
}

fn load(path: &Path, key: &[u8]) -> Result<Option<Snapshot>> {
    let db = open(path)?;
    let found: Option<(String, Vec<u8>)> = db
        .query_row(
            "SELECT meta,png FROM pages WHERE key=?1 AND length(png)<=?2",
            params![key, CACHE_BYTES / 4],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if found.is_some() {
        let _ = db.execute(
            "UPDATE pages SET used=?1 WHERE key=?2",
            params![crate::store::now(), key],
        );
    }
    found
        .map(|(meta, png)| {
            Ok(Snapshot {
                size: serde_json::from_str(&meta)?,
                png,
            })
        })
        .transpose()
}

fn save(path: &Path, key: &[u8], snapshot: Snapshot) -> Result<()> {
    if snapshot.png.len() as i64 > CACHE_BYTES / 4 {
        return Ok(());
    }
    let mut db = open(path)?;
    let tx = db.transaction()?;
    tx.execute(
        "INSERT OR REPLACE INTO pages VALUES (?1,?2,?3,?4)",
        params![
            key,
            serde_json::to_string(&snapshot.size)?,
            snapshot.png,
            crate::store::now()
        ],
    )?;
    // SQLite reuses freed pages, bounding the file near the budget plus one entry.
    tx.execute(
        "DELETE FROM pages WHERE key IN (
        SELECT key FROM (SELECT key, row_number() OVER (ORDER BY used DESC,rowid DESC) AS n,
        sum(length(png)+length(meta)) OVER (ORDER BY used DESC,rowid DESC) AS bytes FROM pages)
        WHERE n>?1 OR bytes>?2)",
        params![MAX_ENTRIES, CACHE_BYTES],
    )?;
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_images_and_viewport_invalidate_the_cache() {
        let raw = b"Content-Type: text/html\r\n\r\n<img src=\"https://example.org/banner.png\">";
        let first = from_mime(raw, |_| None).unwrap();
        assert_ne!(key(&first, 800, 1), key(&first, 808, 1));
        assert_ne!(key(&first, 800, 1), key(&first, 800, 2));
        let mut png = std::io::Cursor::new(Vec::new());
        image::RgbaImage::new(8, 8)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        let loaded = from_mime(raw, |_| Some(png.get_ref().clone())).unwrap();
        assert_ne!(key(&first, 800, 1), key(&loaded, 800, 1));
    }

    #[tokio::test]
    #[ignore = "requires the local Brave browser"]
    async fn cached_raster_preserves_pixels_and_recovers_from_corruption() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("render.sqlite");
        let raw = b"Content-Type: text/html\r\n\r\n<body style=\"background:#512487\"><a href=\"https://example.org\">Test</a></body>";
        let first = render_cached(from_mime(raw, |_| None).unwrap(), 800, 2, &path).await?;
        // Prove the second render can complete without acquiring the browser.
        let permit = RENDERER.acquire().await?;
        let second = tokio::time::timeout(
            Duration::from_secs(2),
            render_cached(from_mime(raw, |_| None).unwrap(), 800, 2, &path),
        )
        .await??;
        drop(permit);
        assert_eq!(first.tiles, second.tiles);
        assert_eq!(first.links[0].url, second.links[0].url);
        open(&path)?.execute("UPDATE pages SET png=x'0000'", [])?;
        let recovered = render_cached(from_mime(raw, |_| None).unwrap(), 800, 2, &path).await?;
        assert_eq!(first.tiles, recovered.tiles);
        Ok(())
    }
}
