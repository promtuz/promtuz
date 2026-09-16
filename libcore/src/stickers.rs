//! Encrypted sticker downloads, pack installation and resumable publishing.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use chacha20poly1305::KeyInit;
use chacha20poly1305::XChaCha20Poly1305;
use chacha20poly1305::XNonce;
use chacha20poly1305::aead::Aead;
use chacha20poly1305::aead::Payload;
use common::node::capability::NodeCapabilities;
use common::proto::client_res::ClientRequest;
use common::proto::client_res::ClientResponse;
use common::proto::pack::Packer;
use common::proto::pack::Unpacker;
use common::proto::push::GatewayRequest;
use common::proto::sticker::BLOB_MAX_BYTES;
use common::proto::sticker::MANIFEST_MAX_BYTES;
use common::proto::sticker::MAX_PACKS_PER_CREATOR;
use common::proto::sticker::Manifest;
use common::proto::sticker::ManifestEnvelope;
use common::proto::sticker::ManifestSticker;
use common::proto::sticker::PACK_MAX_STICKERS;
use common::proto::sticker::PACK_NAME_MAX;
use common::proto::sticker::STICKER_EDGE;
use common::proto::sticker::STICKER_MAX_BYTES;
use common::proto::sticker::StickerRef;
use common::proto::sticker::StoreReject;
use common::proto::sticker::StoreRequest;
use common::proto::sticker::StoreResponse;
use common::proto::sticker::blob_key;
use common::proto::sticker::blob_path;
use common::proto::sticker::blob_put_signing_input;
use common::proto::sticker::manifest_path;
use common::proto::sticker::manifest_signing_input;
use common::types::bytes::Bytes;
use once_cell::sync::Lazy;

use crate::platform::Refused;
use parking_lot::Mutex;
use ravif::Encoder;
use ravif::Img;
use rgb::FromSlice;

use crate::ENDPOINT;
use crate::RESOLVER_SEEDS;
use crate::data::identity::Identity;
use crate::data::identity::IdentitySigner;
use crate::data::stickers as db;
use crate::data::stickers::PackRow;
use crate::data::stickers::StickerRow;
use crate::quic::dialer::connect_to_any_seed;

const CACHE_DIR: &str = "stickers";
const STICKER_AAD: &[u8] = b"promtuz-sticker-v1";
const MANIFEST_AAD: &[u8] = b"promtuz-sticker-manifest-v1";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// An installed pack's manifest is re-read for appends at most this often.
const REFRESH_EVERY: Duration = Duration::from_secs(10 * 60);

static HTTP: Lazy<reqwest::Client> = Lazy::new(|| {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .build()
        .expect("reqwest client")
});

// Concurrent reads of the same sticker share one download.
type Gates = HashMap<([u8; 16], [u8; 32]), Weak<tokio::sync::Mutex<()>>>;
static INFLIGHT: Lazy<Mutex<Gates>> = Lazy::new(|| Mutex::new(HashMap::new()));

static REFRESHED: Lazy<Mutex<HashMap<[u8; 16], Instant>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

/// A decoded picture the platform hands over for a new sticker.
pub struct SourceImage {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// A pack as the picker or the add-pack sheet sees it.
pub struct PackView {
    pub pack: PackRow,
    pub stickers: Vec<StickerRow>,
}

static CACHE_ROOT: Lazy<PathBuf> = Lazy::new(|| PathBuf::from(crate::db::files_dir(CACHE_DIR)));

const CACHE_MAX_BYTES: u64 = 128 * 1024 * 1024;
static CACHE_GENERATION: AtomicU64 = AtomicU64::new(0);
static CACHE_WRITES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
static DOWNLOADS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);

static PUBLISHING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub fn cache_path(pack: &[u8; 16], id: &[u8; 32]) -> PathBuf {
    CACHE_ROOT.join(hex::encode(pack)).join(hex::encode(id))
}

// ── crypto ──────────────────────────────────────────────────────────────

fn aad_sticker(id: &[u8; 32]) -> Vec<u8> {
    [STICKER_AAD, id.as_slice()].concat()
}

fn aad_manifest(pack: &[u8; 16]) -> Vec<u8> {
    [MANIFEST_AAD, pack.as_slice()].concat()
}

/// `nonce ‖ ciphertext` under the pack token.
fn seal(token: &[u8; 32], aad: &[u8], plain: &[u8]) -> Vec<u8> {
    let nonce = common::crypto::get_nonce::<24>();
    let ct = XChaCha20Poly1305::new(token.into())
        .encrypt(XNonce::from_slice(&nonce), Payload { msg: plain, aad })
        .expect("xchacha encrypt");
    [nonce.as_slice(), &ct].concat()
}

fn open(token: &[u8; 32], aad: &[u8], blob: &[u8]) -> Result<Vec<u8>> {
    if blob.len() < 24 + 16 {
        bail!("object too short to be sealed");
    }
    let (nonce, ct) = blob.split_at(24);
    XChaCha20Poly1305::new(token.into())
        .decrypt(XNonce::from_slice(nonce), Payload { msg: ct, aad })
        .map_err(|_| anyhow!("wrong token or corrupted object"))
}

// ── reads ───────────────────────────────────────────────────────────────

/// The sticker's AVIF bytes: from the cache, else fetched, decrypted, checked
/// against its id and cached.
pub async fn fetch(r: &StickerRef) -> Result<Vec<u8>> {
    let path = cache_path(&r.pack, &r.id);
    if let Ok(bytes) = tokio::fs::read(&path).await {
        return Ok(bytes);
    }
    let gate = {
        let mut gates = INFLIGHT.lock();
        gates.retain(|_, g| g.strong_count() > 0);
        let entry = gates.entry((r.pack, r.id)).or_default();
        entry.upgrade().unwrap_or_else(|| {
            let gate = Arc::new(tokio::sync::Mutex::new(()));
            *entry = Arc::downgrade(&gate);
            gate
        })
    };
    let _held = gate.lock().await;
    if let Ok(bytes) = tokio::fs::read(&path).await {
        return Ok(bytes);
    }
    let generation = CACHE_GENERATION.load(Ordering::Relaxed);
    let _permit = DOWNLOADS.acquire().await?;
    let blob = get_object(r.store, &blob_path(&r.pack, &blob_key(&r.token, &r.id)), BLOB_MAX_BYTES)
        .await?;
    let plain = open(&r.token, &aad_sticker(&r.id), &blob).context("open sticker")?;
    if blake3::hash(&plain).as_bytes() != &r.id {
        bail!("sticker bytes don't match their id");
    }
    let _write = CACHE_WRITES.lock().await;
    if CACHE_GENERATION.load(Ordering::Relaxed) == generation {
        if let Err(e) = write_atomic(&path, &plain).await {
            log::debug!("STICKERS: cache write: {e:#}");
        } else if let Err(e) =
            tokio::task::spawn_blocking(|| trim_cache(&CACHE_ROOT, CACHE_MAX_BYTES))
                .await
                .context("cache cleanup task")
                .and_then(|result| result)
        {
            log::debug!("STICKERS: cache cleanup: {e:#}");
        }
    }
    Ok(plain)
}

/// A pack's manifest, verified: the envelope signature by its creator, the
/// blob opened under `token`, and the plaintext agreeing with the envelope.
async fn fetch_manifest(pack: [u8; 16], store: u16, token: [u8; 32]) -> Result<Manifest> {
    // A manifest changes in place; bypass CDN copies during refresh and retry.
    let path = format!("{}?v={}", manifest_path(&pack), crate::utils::systime().as_nanos());
    let raw = get_object(store, &path, MANIFEST_MAX_BYTES + 256).await?;
    let env = ManifestEnvelope::deser(&raw).context("manifest envelope")?;
    if env.pack_id != pack || !env.verify() {
        bail!("manifest signature does not verify");
    }
    let plain = open(&token, &aad_manifest(&pack), &env.manifest_blob).context("open manifest")?;
    let m = Manifest::deser(&plain).context("manifest")?;
    if env.store != store
        || m.pack_id != pack
        || m.creator != env.creator.0
        || m.version != env.version
        || m.store_id != store
    {
        bail!("manifest disagrees with its envelope");
    }
    if m.name.trim().is_empty() || m.name.chars().count() > PACK_NAME_MAX {
        bail!("manifest name out of bounds");
    }
    let mut ids = std::collections::HashSet::new();
    if m.stickers.is_empty()
        || m.stickers.iter().any(|s| {
            s.width == 0
                || s.height == 0
                || s.width as u32 > STICKER_EDGE
                || s.height as u32 > STICKER_EDGE
                || !ids.insert(s.id)
        })
    {
        bail!("invalid sticker roster");
    }
    Ok(m)
}

/// Enforce the installed pack's creator and minimum version.
fn admissible(m: &Manifest, kept: Option<&PackRow>) -> Result<()> {
    if let Some(k) = kept {
        if k.creator != m.creator {
            bail!("pack creator changed");
        }
        if m.version < k.version {
            bail!("manifest is older than the one kept");
        }
    }
    Ok(())
}

fn rows_of(m: &Manifest, token: [u8; 32], added_at: u64) -> (PackRow, Vec<StickerRow>) {
    let pack = PackRow {
        pack_id: m.pack_id,
        store_id: m.store_id,
        token,
        creator: m.creator,
        version: m.version,
        name: m.name.trim().to_string(),
        added_at,
    };
    let stickers = m
        .stickers
        .iter()
        .enumerate()
        .map(|(i, s)| StickerRow {
            pack_id: m.pack_id,
            sticker_id: s.id,
            position: i as u32,
            width: s.width,
            height: s.height,
        })
        .collect();
    (pack, stickers)
}

/// The pack behind a sticker, as the add-pack sheet shows it. Nothing is kept.
pub async fn preview(r: &StickerRef) -> Result<PackView> {
    let kept = db::get_pack(&r.pack);
    let m = fetch_manifest(r.pack, r.store, r.token).await?;
    admissible(&m, kept.as_ref())?;
    let added_at = kept.map(|k| k.added_at).unwrap_or_else(now);
    let (pack, stickers) = rows_of(&m, r.token, added_at);
    Ok(PackView { pack, stickers })
}

/// Keep the pack behind a sticker.
pub async fn install(r: &StickerRef) -> Result<()> {
    let view = preview(r).await?;
    db::upsert_pack(&view.pack, &view.stickers)?;
    Ok(())
}

/// Remove a pack from the picker. Cached message images remain available offline.
pub fn remove(pack: &[u8; 16]) -> Result<()> {
    db::remove_pack(pack)?;
    REFRESHED.lock().remove(pack);
    Ok(())
}

/// Re-read kept packs' manifests for appends, each at most once per
/// [`REFRESH_EVERY`]. Failures are logged; a stale roster is not an error.
pub async fn refresh_kept() {
    if let Ok(_publishing) = PUBLISHING.try_lock() {
        match db::pending_uploads() {
            Ok(uploads) => {
                for pending in uploads {
                    if let Err(e) = resume_upload(&pending).await {
                        log::debug!("STICKERS: pending upload: {e:#}");
                    }
                }
            },
            Err(e) => log::warn!("STICKERS: pending uploads: {e:#}"),
        }
    }
    let due: Vec<PackRow> = {
        let refreshed = REFRESHED.lock();
        db::list_packs()
            .into_iter()
            .filter(|p| refreshed.get(&p.pack_id).is_none_or(|t| t.elapsed() >= REFRESH_EVERY))
            .collect()
    };
    for kept in due {
        REFRESHED.lock().insert(kept.pack_id, Instant::now());
        match fetch_manifest(kept.pack_id, kept.store_id, kept.token).await {
            Ok(m) if m.version > kept.version && admissible(&m, Some(&kept)).is_ok() => {
                let (pack, stickers) = rows_of(&m, kept.token, kept.added_at);
                if let Err(e) = db::refresh_pack(&kept, &pack, &stickers) {
                    log::warn!(
                        "STICKERS: refresh of {} not kept: {e:#}",
                        hex::encode(&kept.pack_id[..4])
                    );
                }
            },
            Ok(_) => {},
            Err(e) => log::debug!(
                "STICKERS: refresh of {} failed: {e:#}",
                hex::encode(&kept.pack_id[..4])
            ),
        }
    }
}

// ── publish ─────────────────────────────────────────────────────────────

/// Create and publish a pack, resuming a matching pending upload first.
pub async fn create(name: String, images: Vec<SourceImage>) -> Result<[u8; 16]> {
    let _publishing = PUBLISHING.lock().await;
    let name = name.trim().to_string();
    if name.is_empty() || name.chars().count() > PACK_NAME_MAX {
        bail!("name must be 1..={PACK_NAME_MAX} characters");
    }
    if images.is_empty() || images.len() > PACK_MAX_STICKERS {
        bail!("a pack holds 1..={PACK_MAX_STICKERS} stickers");
    }
    let creator = Identity::get().context("no identity")?.ipk();
    if let Some(pending) = db::pending_uploads()?
        .into_iter()
        .find(|p| p.pack.creator == creator && p.pack.name == name)
    {
        resume_upload(&pending).await?;
        append_images(pending.pack.pack_id, images).await?;
        return Ok(pending.pack.pack_id);
    }
    let pack = common::crypto::get_nonce::<16>();
    publish(
        pack,
        common::crypto::get_nonce::<32>(),
        default_store().await?,
        name,
        1,
        Vec::new(),
        images,
    )
    .await?;
    Ok(pack)
}

pub async fn append(pack: [u8; 16], images: Vec<SourceImage>) -> Result<()> {
    let _publishing = PUBLISHING.lock().await;
    if let Some(pending) = db::pending_uploads()?.into_iter().find(|p| p.pack.pack_id == pack) {
        resume_upload(&pending).await?;
    }
    append_images(pack, images).await
}

async fn append_images(pack: [u8; 16], images: Vec<SourceImage>) -> Result<()> {
    let kept = db::get_pack(&pack).context("pack not installed")?;
    if kept.creator != Identity::get().context("no identity")?.ipk() {
        bail!("only the creator can add to a pack");
    }
    if images.is_empty() || images.len() > PACK_MAX_STICKERS {
        bail!("invalid sticker count");
    }
    let existing = db::stickers_of(&pack);
    let version = kept.version.checked_add(1).context("pack version exhausted")?;
    publish(pack, kept.token, kept.store_id, kept.name.clone(), version, existing, images).await
}

async fn resume_upload(pending: &db::PendingUpload) -> Result<()> {
    anyhow::ensure!(
        Identity::get().is_some_and(|i| i.ipk() == pending.pack.creator),
        "upload belongs to another identity"
    );
    let mut pending = pending.clone();
    for _ in 0..3 {
        match upload(&pending.requests).await {
            Ok(()) => {
                db::finish_upload(&pending)?;
                REFRESHED.lock().insert(pending.pack.pack_id, Instant::now());
                return Ok(());
            },
            Err(e) if e.downcast_ref::<StoreReject>() == Some(&StoreReject::StaleVersion) => {
                let p = &pending.pack;
                let latest = fetch_manifest(p.pack_id, p.store_id, p.token).await?;
                admissible(&latest, Some(p))?;
                let (mut pack, mut stickers) = rows_of(&latest, p.token, p.added_at);
                for s in &pending.stickers {
                    if !stickers.iter().any(|known| known.sticker_id == s.sticker_id) {
                        let mut s = s.clone();
                        s.position = stickers.len() as u32;
                        stickers.push(s);
                    }
                }
                if stickers.len() == latest.stickers.len() {
                    pending.pack = pack;
                    pending.stickers = stickers;
                    db::finish_upload(&pending)?;
                    return Ok(());
                }
                anyhow::ensure!(stickers.len() <= PACK_MAX_STICKERS, "pack is full");
                pack.version = pack.version.checked_add(1).context("pack version exhausted")?;
                pending.requests.retain(|r| matches!(r, StoreRequest::PutBlob { .. }));
                pending.requests.push(manifest_request(&pack, &stickers)?);
                pending.pack = pack;
                pending.stickers = stickers;
                db::save_upload(&pending)?;
            },
            Err(e) => return Err(e),
        }
    }
    Err(Refused("The pack changed on another device. Try again.".into()).into())
}

fn manifest_request(pack: &PackRow, stickers: &[StickerRow]) -> Result<StoreRequest> {
    let manifest = Manifest {
        pack_id: pack.pack_id,
        store_id: pack.store_id,
        creator: pack.creator,
        version: pack.version,
        name: pack.name.clone(),
        stickers: stickers
            .iter()
            .map(|s| ManifestSticker { id: s.sticker_id, width: s.width, height: s.height })
            .collect(),
    };
    let manifest_blob = seal(&pack.token, &aad_manifest(&pack.pack_id), &manifest.ser()?);
    let (sig, creator) = IdentitySigner::sign_with_ipk(&manifest_signing_input(
        &pack.pack_id,
        pack.store_id,
        pack.version,
        &manifest_blob,
    ))?;
    anyhow::ensure!(creator == pack.creator, "identity changed during publishing");
    Ok(StoreRequest::PutManifest {
        env: ManifestEnvelope {
            pack_id: pack.pack_id,
            store: pack.store_id,
            creator: Bytes(creator),
            version: pack.version,
            manifest_blob,
            sig: Bytes(sig.to_bytes()),
        },
        keys: stickers.iter().map(|s| Bytes(blob_key(&pack.token, &s.sticker_id))).collect(),
    })
}

/// Encode, encrypt and upload `images` after `existing`, then publish the
/// manifest naming all of them and keep the result locally.
async fn publish(
    pack: [u8; 16], token: [u8; 32], store: u16, name: String, version: u32,
    existing: Vec<StickerRow>, images: Vec<SourceImage>,
) -> Result<()> {
    let known: Vec<[u8; 32]> = existing.iter().map(|s| s.sticker_id).collect();
    type Encoded = (StickerRow, Vec<u8>);
    // Encoding is CPU-bound and takes a moment per picture; off the reactor.
    let encoded = tokio::task::spawn_blocking(move || -> Result<Vec<Encoded>> {
        let mut out = Vec::with_capacity(images.len());
        let mut position = known.len() as u32;
        for img in images {
            let (avif, width, height) = encode_sticker(&img.rgba, img.width, img.height)?;
            let id = *blake3::hash(&avif).as_bytes();
            if known.contains(&id) || out.iter().any(|(r, _): &Encoded| r.sticker_id == id) {
                continue; // the same picture twice is one sticker
            }
            let blob = seal(&token, &aad_sticker(&id), &avif);
            out.push((StickerRow { pack_id: pack, sticker_id: id, position, width, height }, blob));
            position += 1;
        }
        Ok(out)
    })
    .await
    .context("encode task")??;
    if encoded.is_empty() {
        if existing.is_empty() {
            bail!("nothing to publish");
        }
        return Ok(());
    }
    if existing.len() + encoded.len() > PACK_MAX_STICKERS {
        return Err(Refused(format!("A pack can hold up to {PACK_MAX_STICKERS} stickers.")).into());
    }

    let creator_ipk = Identity::get().context("no identity")?.ipk();
    let mut requests = Vec::with_capacity(encoded.len() + 1);
    for (row, blob) in &encoded {
        let key = blob_key(&token, &row.sticker_id);
        let (sig, creator) =
            IdentitySigner::sign_with_ipk(&blob_put_signing_input(&pack, store, &key, blob))?;
        requests.push(StoreRequest::PutBlob {
            pack,
            store,
            key,
            creator: Bytes(creator),
            bytes: blob.clone(),
            sig: Bytes(sig.to_bytes()),
        });
    }
    let all: Vec<StickerRow> =
        existing.into_iter().chain(encoded.iter().map(|(r, _)| r.clone())).collect();
    let added_at = db::get_pack(&pack).map(|p| p.added_at).unwrap_or_else(now);
    let row = PackRow {
        pack_id: pack,
        store_id: store,
        token,
        creator: creator_ipk,
        version,
        name,
        added_at,
    };
    requests.push(manifest_request(&row, &all)?);
    let pending = db::PendingUpload { pack: row, stickers: all, requests };
    db::save_upload(&pending)?;
    resume_upload(&pending).await
}

/// Fit within [`STICKER_EDGE`], encode AVIF with alpha under
/// [`STICKER_MAX_BYTES`]. Returns `(avif, width, height)`.
fn encode_sticker(rgba: &[u8], width: u32, height: u32) -> Result<(Vec<u8>, u16, u16)> {
    if width == 0 || height == 0 || rgba.len() != width as usize * height as usize * 4 {
        bail!("bad picture buffer");
    }
    let img = image::ImageBuffer::<image::Rgba<u8>, _>::from_raw(width, height, rgba)
        .expect("len checked");
    let fit = |w: u32, h: u32, edge: u32| {
        let s = edge as f64 / w.max(h) as f64;
        if s >= 1.0 {
            (w, h)
        } else {
            (((w as f64) * s).round().max(1.0) as u32, ((h as f64) * s).round().max(1.0) as u32)
        }
    };
    let mut edge = STICKER_EDGE;
    for quality in [78.0f32, 62.0, 48.0] {
        let (w, h) = fit(width, height, edge);
        let scaled = image::imageops::resize(&img, w, h, image::imageops::FilterType::Lanczos3);
        let out = Encoder::new()
            .with_quality(quality)
            .with_alpha_quality(85.0)
            .with_speed(7)
            .encode_rgba(Img::new(scaled.as_raw().as_rgba(), w as usize, h as usize))?
            .avif_file;
        if out.len() <= STICKER_MAX_BYTES {
            return Ok((out, w as u16, h as u16));
        }
        edge = (edge * 3 / 4).max(128);
    }
    bail!("picture would not fit a sticker")
}

/// Try every discovered gateway. A push gateway may serve no sticker store,
/// or a different one; either response leaves the request safe to retry.
async fn upload(requests: &[StoreRequest]) -> Result<()> {
    let gateways = tokio::time::timeout(REQUEST_TIMEOUT, crate::push::fetch_gateways()).await??;
    let mut last_error = None;
    for gateway in gateways {
        let attempt = tokio::time::timeout(CONNECT_TIMEOUT, async {
            let endpoint = ENDPOINT.get().context("endpoint not initialized")?;
            let conn = endpoint.connect(gateway.addr, &gateway.id.to_string())?.await?;
            Ok::<_, anyhow::Error>(conn)
        })
        .await;
        let conn = match attempt {
            Ok(Ok(conn)) => conn,
            Ok(Err(e)) => {
                last_error = Some(e);
                continue;
            },
            Err(e) => {
                last_error = Some(e.into());
                continue;
            },
        };
        if !crate::push::capabilities_from_conn(&conn)
            .is_some_and(|c| c.contains(NodeCapabilities::STICKER_STORE))
        {
            conn.close(0u32.into(), b"not-a-store");
            continue;
        }
        let result = async {
            for req in requests {
                let response = tokio::time::timeout(REQUEST_TIMEOUT, async {
                    let (mut tx, mut rx) = conn.open_bi().await?;
                    tx.write_all(&GatewayRequest::Store(req.clone()).pack()?).await?;
                    tx.finish()?;
                    Ok::<_, anyhow::Error>(StoreResponse::unpack(&mut rx).await?)
                })
                .await??;
                if let StoreResponse::Rejected(why) = response {
                    let text = match why {
                        StoreReject::QuotaExceeded => {
                            format!("You can publish up to {MAX_PACKS_PER_CREATOR} packs.")
                        },
                        StoreReject::PackFull => "This pack has reached its sticker limit.".into(),
                        StoreReject::Unavailable => {
                            "The sticker store is unavailable. Try again later.".into()
                        },
                        _ => "Couldn’t publish this pack. Try again.".into(),
                    };
                    return Err(anyhow::Error::new(Refused(text)).context(why));
                }
            }
            Ok(())
        }
        .await;
        conn.close(0u32.into(), b"done");
        match result {
            Ok(()) => return Ok(()),
            Err(e) if matches!(e.downcast_ref::<StoreReject>(), Some(StoreReject::WrongStore)) => {
            },
            Err(e)
                if matches!(
                    e.downcast_ref::<StoreReject>(),
                    None | Some(StoreReject::Unavailable)
                ) =>
            {
                last_error = Some(e)
            },
            Err(e) => return Err(e),
        }
    }
    Err(last_error.unwrap_or_else(|| Refused(NO_STORE.into()).into()))
}

// ── store directory + transport ─────────────────────────────────────────

const NO_STORE: &str = "Sticker packs can't be published on this network yet.";

/// Where new packs go: the first store the resolver lists.
async fn default_store() -> Result<u16> {
    let stores = fetch_directory().await?;
    stores.first().map(|s| s.id).ok_or_else(|| Refused(NO_STORE.into()).into())
}

async fn fetch_directory() -> Result<Vec<common::proto::client_res::StoreDescriptor>> {
    let seeds = RESOLVER_SEEDS.get().context("resolver seeds not set")?;
    let conn = connect_to_any_seed(seeds).await?;
    let (mut send, mut recv) = conn.open_bi().await?;
    send.write_all(&ClientRequest::GetStores().pack()?).await?;
    send.finish()?;
    let resp = ClientResponse::unpack(&mut recv).await?;
    conn.close(0u32.into(), b"done");
    match resp {
        ClientResponse::GetStores { stores } => {
            db::cache_stores(&stores);
            Ok(stores)
        },
        other => Err(anyhow!("GetStores: unexpected variant {other:?}")),
    }
}

async fn store_base_url(store: u16, refresh: bool) -> Result<String> {
    if !refresh && let Some(url) = db::cached_store(store) {
        return Ok(url);
    }
    fetch_directory()
        .await?
        .into_iter()
        .find(|s| s.id == store)
        .map(|s| s.base_url.trim_end_matches('/').to_string())
        .with_context(|| format!("store {store} is not in the directory"))
}

/// Refresh the store directory once on connection failure to follow moved stores.
/// HTTP and size errors are final.
async fn get_object(store: u16, path: &str, max: usize) -> Result<Vec<u8>> {
    let base = store_base_url(store, false).await?;
    match http_get(&format!("{base}/{path}"), max).await {
        Ok(bytes) => Ok(bytes),
        Err(Fetch::Answered(e)) => Err(e),
        Err(Fetch::Unreachable(e)) => {
            let fresh = match store_base_url(store, true).await {
                Ok(fresh) if fresh != base => fresh,
                _ => return Err(e),
            };
            http_get(&format!("{fresh}/{path}"), max).await.map_err(Fetch::into_inner)
        },
    }
}

/// Distinguishes connection failures from HTTP and content errors.
enum Fetch {
    Answered(anyhow::Error),
    Unreachable(anyhow::Error),
}

impl Fetch {
    fn into_inner(self) -> anyhow::Error {
        match self {
            Self::Answered(e) | Self::Unreachable(e) => e,
        }
    }
}

async fn http_get(url: &str, max: usize) -> Result<Vec<u8>, Fetch> {
    // Debug, not Display: reqwest's message is "error sending request" and
    // the reason (refused, no route, DNS) is only in the source chain.
    let mut resp = HTTP
        .get(url)
        .header(reqwest::header::CACHE_CONTROL, "no-cache")
        .send()
        .await
        .map_err(|e| Fetch::Unreachable(anyhow!("GET {url}: {e:?}")))?;
    if !resp.status().is_success() {
        return Err(Fetch::Answered(anyhow!("GET {url}: {}", resp.status())));
    }
    if resp.content_length().is_some_and(|n| n as usize > max) {
        return Err(Fetch::Answered(anyhow!("object over {max} bytes")));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| Fetch::Unreachable(e.into()))? {
        if chunk.len() > max.saturating_sub(bytes.len()) {
            return Err(Fetch::Answered(anyhow!("object over {max} bytes")));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

async fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("cache path has no parent")?;
    tokio::fs::create_dir_all(parent).await?;
    let tmp = path.with_extension("part");
    tokio::fs::write(&tmp, bytes).await?;
    tokio::fs::rename(&tmp, path).await?;
    Ok(())
}

/// Cached images are replaceable; pack tokens and upload drafts live in the database.
pub fn cache_bytes() -> Result<u64> {
    Ok(cache_files(&CACHE_ROOT)?.iter().map(|(_, m)| m.len()).sum())
}

pub async fn clear_cache() -> Result<()> {
    let _write = CACHE_WRITES.lock().await;
    CACHE_GENERATION.fetch_add(1, Ordering::Relaxed);
    tokio::task::spawn_blocking(|| trim_cache(&CACHE_ROOT, 0)).await??;
    Ok(())
}

fn cache_files(root: &std::path::Path) -> Result<Vec<(PathBuf, std::fs::Metadata)>> {
    let mut files = Vec::new();
    for pack in std::fs::read_dir(root)? {
        let pack = pack?;
        if !pack.file_type()?.is_dir() {
            continue;
        }
        for entry in std::fs::read_dir(pack.path())? {
            let entry = entry?;
            let metadata = match std::fs::symlink_metadata(entry.path()) {
                Ok(metadata) => metadata,
                // Cache eviction can run while Storage calculates usage.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            };
            if metadata.is_file() {
                files.push((entry.path(), metadata));
            }
        }
    }
    Ok(files)
}

fn trim_cache(root: &std::path::Path, limit: u64) -> Result<()> {
    let mut files = cache_files(root)?;
    let mut bytes: u64 = files.iter().map(|(_, m)| m.len()).sum();
    files.sort_by_key(|(_, m)| m.modified().ok());
    for (path, meta) in files {
        if bytes <= limit {
            break;
        }
        std::fs::remove_file(path)?;
        bytes = bytes.saturating_sub(meta.len());
    }
    Ok(())
}

fn now() -> u64 {
    crate::utils::systime().as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sealed_objects_open_only_under_their_token_and_aad() {
        let token = [7u8; 32];
        let id = [9u8; 32];
        let blob = seal(&token, &aad_sticker(&id), b"avif bytes");
        assert_eq!(open(&token, &aad_sticker(&id), &blob).unwrap(), b"avif bytes");
        assert!(open(&[8u8; 32], &aad_sticker(&id), &blob).is_err());
        assert!(open(&token, &aad_sticker(&[1u8; 32]), &blob).is_err());
        assert!(open(&token, &aad_manifest(&[1u8; 16]), &blob).is_err());
    }

    #[test]
    fn manifest_admission_pins_creator_and_version() {
        let m = Manifest {
            pack_id: [1; 16],
            store_id: 1,
            creator: [4; 32],
            version: 3,
            name: "cats".into(),
            stickers: vec![],
        };
        let kept = PackRow {
            pack_id: [1; 16],
            store_id: 1,
            token: [0; 32],
            creator: [4; 32],
            version: 2,
            name: "cats".into(),
            added_at: 0,
        };
        assert!(admissible(&m, None).is_ok());
        assert!(admissible(&m, Some(&kept)).is_ok());
        assert!(admissible(&Manifest { creator: [5; 32], ..m.clone() }, Some(&kept)).is_err());
        assert!(admissible(&Manifest { version: 1, ..m }, Some(&kept)).is_err());
    }

    /// Serve fixture objects over loopback HTTP; returns the base URL.
    async fn serve(dir: PathBuf) -> String {
        use tokio::io::AsyncReadExt;
        use tokio::io::AsyncWriteExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { break };
                let dir = dir.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    let head = String::from_utf8_lossy(&buf[..n]).to_string();
                    let path = head
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or("/")
                        .trim_start_matches('/')
                        .to_string();
                    let resp = match std::fs::read(dir.join(path.split('?').next().unwrap())) {
                        Ok(body) => {
                            let mut r = format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                body.len()
                            )
                            .into_bytes();
                            r.extend(body);
                            r
                        },
                        Err(_) => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
                    };
                    let _ = sock.write_all(&resp).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        format!("http://127.0.0.1:{port}")
    }

    #[tokio::test]
    async fn fetches_and_installs_from_a_store_laid_out_like_the_gateway() {
        let dir = std::env::temp_dir().join(format!("promtuz-stickers-e2e-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        unsafe { std::env::set_var("PROMTUZ_DATA_DIR", &dir) };

        let creator = ed25519_dalek::SigningKey::from_bytes(&[3u8; 32]);
        let (pack, token, store) = ([0xA1u8; 16], [0xB2u8; 32], 1u16);
        let avif = b"not really avif, but bytes".to_vec();
        let id = *blake3::hash(&avif).as_bytes();

        // What `publish` sends and the gateway stores, minus the transport.
        let objects = dir.join("objects");
        let blob = seal(&token, &aad_sticker(&id), &avif);
        let blob_file = objects.join(blob_path(&pack, &blob_key(&token, &id)));
        std::fs::create_dir_all(blob_file.parent().unwrap()).unwrap();
        std::fs::write(&blob_file, blob).unwrap();
        let manifest = Manifest {
            pack_id: pack,
            store_id: store,
            creator: creator.verifying_key().to_bytes(),
            version: 1,
            name: "cats".into(),
            stickers: vec![ManifestSticker { id, width: 512, height: 384 }],
        };
        let env = ManifestEnvelope::signed(
            &creator,
            pack,
            store,
            1,
            seal(&token, &aad_manifest(&pack), &manifest.ser().unwrap()),
        );
        std::fs::write(objects.join(manifest_path(&pack)), env.ser().unwrap()).unwrap();

        let base = serve(objects).await;
        db::cache_stores(&[common::proto::client_res::StoreDescriptor {
            id: store,
            base_url: base,
        }]);

        let r = StickerRef { pack, id, token, store };
        assert_eq!(fetch(&r).await.unwrap(), avif);
        assert!(cache_path(&pack, &id).exists(), "fetched bytes are cached");
        assert_eq!(fetch(&r).await.unwrap(), avif, "and served from the cache");

        // The cache is by content id: a verified picture serves whatever token
        // asks for it. An unseen id under a wrong token has nowhere to look.
        let wrong = StickerRef { id: [5u8; 32], token: [9u8; 32], ..r };
        assert!(fetch(&wrong).await.is_err(), "a wrong token finds nothing");

        let view = preview(&r).await.unwrap();
        assert_eq!(view.pack.name, "cats");
        assert_eq!(view.stickers.len(), 1);
        assert!(db::get_pack(&pack).is_none(), "a preview keeps nothing");

        install(&r).await.unwrap();
        let kept = db::get_pack(&pack).unwrap();
        assert_eq!((kept.version, kept.creator), (1, creator.verifying_key().to_bytes()));
        assert_eq!(db::stickers_of(&pack)[0].sticker_id, id);

        // Upload state retains the exact signed bytes until the final local commit.
        let pending = db::PendingUpload {
            pack: kept.clone(),
            stickers: db::stickers_of(&pack),
            requests: vec![StoreRequest::PutManifest {
                env: env.clone(),
                keys: vec![Bytes(blob_key(&token, &id))],
            }],
        };
        db::save_upload(&pending).unwrap();
        let restored =
            db::pending_uploads().unwrap().into_iter().find(|p| p.pack.pack_id == pack).unwrap();
        assert_eq!(restored.requests.ser().unwrap(), pending.requests.ser().unwrap());
        db::finish_upload(&restored).unwrap();
        assert!(db::pending_uploads().unwrap().is_empty());

        remove(&pack).unwrap();
        assert!(db::get_pack(&pack).is_none());
        assert!(cache_path(&pack, &id).exists(), "removing a pack keeps cached messages available");
        assert_eq!(cache_bytes().unwrap(), avif.len() as u64);
        clear_cache().await.unwrap();
        assert_eq!(cache_bytes().unwrap(), 0);
        assert!(!cache_path(&pack, &id).exists());
    }

    #[test]
    fn encode_fits_the_edge_and_the_cap() {
        let (w, h) = (800u32, 600u32);
        let mut rgba = vec![0u8; (w * h * 4) as usize];
        for (i, px) in rgba.chunks_mut(4).enumerate() {
            px[0] = (i % 251) as u8;
            px[1] = (i % 241) as u8;
            px[2] = (i % 239) as u8;
            px[3] = if i % 7 == 0 { 0 } else { 255 };
        }
        let (avif, ow, oh) = encode_sticker(&rgba, w, h).unwrap();
        assert_eq!((ow, oh), (512, 384));
        assert!(avif.len() <= STICKER_MAX_BYTES);
        assert!(avif.len() > 8 && &avif[4..8] == b"ftyp");
        assert!(encode_sticker(&[], 0, 0).is_err());
    }

    #[test]
    fn refusal_preserves_store_error_and_display_message() {
        let e = anyhow::Error::new(Refused("full".into())).context(StoreReject::PackFull);
        assert_eq!(e.downcast_ref::<StoreReject>(), Some(&StoreReject::PackFull));
        assert!(
            matches!(crate::platform::CoreError::from(e), crate::platform::CoreError::Refused { msg } if msg == "full")
        );
    }
}
