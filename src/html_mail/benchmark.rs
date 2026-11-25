//! Offline, deterministic benchmark. No mailbox access or network downloads.
use std::{io::Cursor, time::Instant};

use anyhow::{Context, Result, ensure};
use serde_json::json;

use super::*;

pub async fn run(args: &[String]) -> Result<()> {
    ensure!(
        args.len() <= 2 && args.get(1).is_none_or(|mode| mode == "cached"),
        "Usage: --bench-render [rounds] [cached]"
    );
    let rounds: usize = args.first().map_or(Ok(8), |arg| arg.parse())?;
    let cached = args.get(1).is_some();
    let directory = tempfile::tempdir()?;
    let cache_path = directory.path().join("render.sqlite");
    ensure!((1..=100).contains(&rounds), "Use 1 to 100 rounds");
    let mut png = Cursor::new(Vec::new());
    image::RgbaImage::from_fn(600, 280, |x, y| {
        image::Rgba([(x % 251) as u8, (y % 241) as u8, ((x + y) % 239) as u8, 255])
    })
    .write_to(&mut png, image::ImageFormat::Png)?;
    let cases = [
        ("notification", 1000, 1, 3),
        ("newsletter", 1000, 1, 45),
        ("retina", 800, 2, 15),
    ];
    for round in 0..rounds {
        for (name, width, density, rows) in cases {
            let content_round = if cached { 0 } else { round };
            let content = (0..rows).map(|index| format!(
                "<tr><td style=\"padding:14px;border-bottom:1px solid #ccc\"><a href=\"https://example.invalid/{index}\">Message {index}</a><p>Cached email content with normal wrapping and tables.</p></td></tr>"
            )).collect::<String>();
            // Change content each round so this measures actual rasterization,
            // rather than looking up an already rendered document.
            let raw = format!(
                "Content-Type: text/html; charset=utf-8\r\n\r\n<body style=\"margin:0;background:#eee\"><table align=\"center\" width=\"600\" style=\"background:#fff\"><tr><td><img src=\"https://example.invalid/banner.png\"><h1>Round {content_round}</h1></td></tr>{content}</table></body>"
            );
            if cached && round == 0 {
                render_cached(
                    from_mime(raw.as_bytes(), |_| Some(png.get_ref().clone())).unwrap(),
                    width,
                    density,
                    &cache_path,
                )
                .await?;
            }
            let start = Instant::now();
            let document = from_mime(raw.as_bytes(), |_| Some(png.get_ref().clone()))
                .context("Invalid benchmark MIME")?;
            let page = if cached {
                render_cached(document, width, density, &cache_path).await?
            } else {
                render(document, width, density).await?
            };
            ensure!(page.links.len() == rows, "Link geometry changed");
            println!(
                "{}",
                json!({"case":name,"round":round,"cached":cached,"ms":start.elapsed().as_secs_f64()*1000.,"width":page.width,"height":page.height,"density":page.density,"tiles":page.tiles.len(),"animated_images":page.animated.len(),"missing_images":page.missing.len()})
            );
        }
    }
    if let Ok(seconds) = std::env::var("TANDEM_BENCH_IDLE") {
        let seconds: u64 = seconds.parse()?;
        ensure!(seconds <= 60, "Idle measurement is limited to 60 seconds");
        eprintln!("Renderer idle measurement begins");
        tokio::time::sleep(Duration::from_secs(seconds)).await;
    }
    Ok(())
}
