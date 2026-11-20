use std::{
    collections::HashMap,
    io::Cursor,
    net::{IpAddr, Ipv4Addr},
    path::{Path, PathBuf},
    sync::{Arc, LazyLock, Weak},
    time::Duration,
};

use anyhow::{Result, ensure};
use reqwest::{
    Client, Url,
    dns::{Addrs, Name, Resolve, Resolving},
    redirect::Policy,
};
use tokio::sync::{Mutex, Semaphore};

use crate::{message_body::IMAGE_LIMIT, store::Store};

type ImageKey = (PathBuf, Url);
static TRANSFERS: LazyLock<Mutex<HashMap<ImageKey, Weak<Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static DOWNLOADS: Semaphore = Semaphore::const_new(6);
static CLIENT: LazyLock<Result<Client, reqwest::Error>> = LazyLock::new(|| {
    Client::builder()
        .connect_timeout(Duration::from_secs(4))
        .timeout(Duration::from_secs(8))
        .referer(false)
        .no_proxy()
        .dns_resolver(Arc::new(PublicDns))
        .redirect(Policy::custom(|redirect| {
            if redirect.previous().len() >= 5 || !public_url(redirect.url()) {
                redirect.error("Image redirect is unavailable")
            } else {
                redirect.follow()
            }
        }))
        .build()
});

pub fn public_url(url: &Url) -> bool {
    if !matches!(url.scheme(), "https" | "http")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
    {
        return false;
    }
    let Some(host) = url.host_str() else {
        return false;
    };
    let host = host.trim_end_matches('.');
    if let Ok(ip) = host.trim_matches(['[', ']']).parse() {
        return public_ip(ip);
    }
    host.contains('.')
        && ![".localhost", ".local", ".internal", ".home.arpa"]
            .iter()
            .any(|suffix| host.ends_with(suffix))
}

fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => public_v4(ip),
        IpAddr::V6(ip) => {
            if let Some(ip) = ip.to_ipv4_mapped() {
                return public_v4(ip);
            }
            let [first, second, ..] = ip.segments();
            (0x2000..0x4000).contains(&first)
                && !(first == 0x2001 && (second < 0x0200 || second == 0x0db8))
                && first != 0x2002
                && !(first == 0x3fff && second < 0x1000)
        }
    }
}

fn public_v4(ip: Ipv4Addr) -> bool {
    let reserved = matches!(
        ip.octets(),
        [0 | 224..=255, ..] | [100, 64..=127, ..] | [192, 0, 0, _] | [198, 18 | 19, ..]
    );
    !(reserved
        || ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_documentation())
}

struct PublicDns;

impl Resolve for PublicDns {
    fn resolve(&self, name: Name) -> Resolving {
        Box::pin(async move {
            // Hand the checked addresses to the connector itself so the
            // connection cannot perform a different DNS lookup afterward.
            let addresses: Vec<_> = tokio::net::lookup_host((name.as_str(), 0))
                .await?
                .filter(|addr| public_ip(addr.ip()))
                .collect();
            if addresses.is_empty() {
                return Err(std::io::Error::other("Image host has no public address").into());
            }
            Ok(Box::new(addresses.into_iter()) as Addrs)
        })
    }
}

pub async fn load(path: &Path, url: &Url) -> Result<Arc<[u8]>> {
    ensure!(public_url(url), "Image address is unavailable");
    let transfer = {
        let mut active = TRANSFERS.lock().await;
        active.retain(|_, lock| lock.strong_count() != 0);
        let key = (path.to_owned(), url.clone());
        if let Some(lock) = active.get(&key).and_then(Weak::upgrade) {
            lock
        } else {
            let lock = Arc::new(Mutex::new(()));
            active.insert(key, Arc::downgrade(&lock));
            lock
        }
    };
    let _transfer = transfer.lock().await;
    if let Some(bytes) = cached(path, url).await? {
        return Ok(bytes);
    }
    let _slot = DOWNLOADS.acquire().await?;
    let bytes = fetch(url).await?;
    let (path, url) = (path.to_owned(), url.clone());
    tokio::task::spawn_blocking(move || {
        let reader = image::ImageReader::new(Cursor::new(&bytes)).with_guessed_format()?;
        let (w, h) = reader.into_dimensions()?;
        ensure!(
            w > 0
                && h > 0
                && w <= 8192
                && h <= 8192
                && u64::from(w) * u64::from(h) <= 16 * 1024 * 1024,
            "Inline image dimensions are unsupported"
        );
        Store::open_cache_writer(&path)?.save_remote_image(url.as_str(), &bytes)?;
        Ok(Arc::from(bytes))
    })
    .await?
}

async fn cached(path: &Path, url: &Url) -> Result<Option<Arc<[u8]>>> {
    let (path, url) = (path.to_owned(), url.clone());
    tokio::task::spawn_blocking(move || {
        Ok(Store::open_read_only(&path)?
            .remote_image(url.as_str())?
            .map(Arc::from))
    })
    .await?
}

async fn fetch(url: &Url) -> Result<Vec<u8>> {
    let client = CLIENT
        .as_ref()
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let mut response = client
        .get(url.clone())
        .send()
        .await
        .map_err(reqwest::Error::without_url)?;
    ensure!(
        response.status().is_success(),
        "Image server returned {}",
        response.status()
    );
    ensure!(
        response
            .content_length()
            .is_none_or(|len| len <= IMAGE_LIMIT as u64),
        "Inline image is too large"
    );
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(reqwest::Error::without_url)?
    {
        ensure!(
            bytes.len() + chunk.len() <= IMAGE_LIMIT,
            "Inline image is too large"
        );
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn automatic_downloads_accept_public_origins_and_reject_private_destinations() {
        for url in [
            "https://assets.kustomer.com/Icons/social-media/facebook-dark.png",
            "https://cdn.example.org/image?id=42",
            "http://cdn.example.org/image",
            "https://1.1.1.1/image",
            "https://[2606:4700:4700::1111]/image",
        ] {
            assert!(public_url(&Url::parse(url).unwrap()), "{url}");
        }
        for url in [
            "http://127.1/image",
            "http://0.0.0.0/image",
            "http://192.168.1.2/image",
            "http://169.254.169.254/image",
            "http://100.64.0.1/image",
            "http://[::1]/image",
            "http://[::ffff:192.168.1.2]/image",
            "https://printer.local/image",
            "https://hostname/image",
            "https://cdn.example.org:8443/image",
            "https://local:password@cdn.example.org/image",
        ] {
            assert!(!public_url(&Url::parse(url).unwrap()), "{url}");
        }
    }
}
