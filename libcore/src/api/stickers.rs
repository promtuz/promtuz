//! Platform API for sticker packs, messages and cached images.

use common::proto::pack::Packer;
use common::proto::sticker::StickerRef;

use crate::api::messaging::on_runtime;
use crate::api::messaging::to_conv16;
use crate::api::messaging::to_did16;
use crate::data::identity::Identity;
use crate::data::stickers as db;
use crate::data::stickers::StickerRow;
use crate::platform::CoreError;
use crate::stickers::SourceImage;

/// A sticker reference and its display dimensions. The token grants access
/// to the pack; share it only as part of an intentional sticker send.
#[derive(uniffi::Record, Clone, Debug)]
pub struct StickerRecord {
    pub pack: Vec<u8>,
    pub id: Vec<u8>,
    pub token: Vec<u8>,
    pub store: u16,
    pub width: u32,
    pub height: u32,
}

/// A kept pack with its roster, in picker order.
#[derive(uniffi::Record)]
pub struct StickerPackRecord {
    pub pack: Vec<u8>,
    pub name: String,
    /// Only the creator can append stickers.
    pub mine: bool,
    pub stickers: Vec<StickerRecord>,
}

/// The pack behind a received sticker, before deciding to keep it.
#[derive(uniffi::Record)]
pub struct StickerPackPreview {
    pub pack: Vec<u8>,
    pub name: String,
    pub installed: bool,
    pub mine: bool,
    pub stickers: Vec<StickerRecord>,
}

/// A decoded picture for a new sticker: tightly packed RGBA.
#[derive(uniffi::Record)]
pub struct StickerSource {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

pub(crate) fn record(row: &StickerRow, token: &[u8; 32], store: u16) -> StickerRecord {
    StickerRecord {
        pack: row.pack_id.to_vec(),
        id: row.sticker_id.to_vec(),
        token: token.to_vec(),
        store,
        width: row.width as u32,
        height: row.height as u32,
    }
}

pub(crate) fn to_ref(s: &StickerRecord) -> Result<StickerRef, CoreError> {
    let bad =
        |what: &str| CoreError::Internal { msg: format!("sticker {what} has the wrong length") };
    Ok(StickerRef {
        pack: s.pack.as_slice().try_into().map_err(|_| bad("pack"))?,
        id: s.id.as_slice().try_into().map_err(|_| bad("id"))?,
        token: s.token.as_slice().try_into().map_err(|_| bad("token"))?,
        store: s.store,
    })
}

fn my_ipk() -> Option<[u8; 32]> {
    Identity::get().map(|i| i.ipk())
}

fn pack_record(
    pack: &db::PackRow, stickers: &[StickerRow], me: Option<[u8; 32]>,
) -> StickerPackRecord {
    StickerPackRecord {
        pack: pack.pack_id.to_vec(),
        name: pack.name.clone(),
        mine: me == Some(pack.creator),
        stickers: stickers.iter().map(|s| record(s, &pack.token, pack.store_id)).collect(),
    }
}

/// Installed packs and their stickers, in picker order.
#[uniffi::export]
pub fn sticker_packs() -> Vec<StickerPackRecord> {
    let me = my_ipk();
    db::list_packs()
        .into_iter()
        .map(|p| pack_record(&p, &db::stickers_of(&p.pack_id), me))
        .collect()
}

/// Most recently sent first, from kept packs.
#[uniffi::export]
pub fn recent_stickers(limit: u32) -> Vec<StickerRecord> {
    db::recents(limit)
        .into_iter()
        .filter_map(|s| db::get_pack(&s.pack_id).map(|p| record(&s, &p.token, p.store_id)))
        .collect()
}

/// The sticker's AVIF bytes, fetched on first use and cached after.
#[uniffi::export]
pub async fn sticker_image(sticker: StickerRecord) -> Result<Vec<u8>, CoreError> {
    let r = to_ref(&sticker)?;
    on_runtime(async move {
        crate::stickers::fetch(&r).await.inspect_err(|e| {
            log::warn!("STICKERS: fetch {} failed: {e:#}", hex::encode(&r.id[..4]));
        })
    })
    .await
}

/// The pack a received sticker belongs to, verified and read, nothing kept.
#[uniffi::export]
pub async fn sticker_pack_preview(sticker: StickerRecord) -> Result<StickerPackPreview, CoreError> {
    let r = to_ref(&sticker)?;
    let view = on_runtime(async move { crate::stickers::preview(&r).await }).await?;
    let rec = pack_record(&view.pack, &view.stickers, my_ipk());
    Ok(StickerPackPreview {
        pack: rec.pack,
        name: rec.name,
        installed: db::get_pack(&view.pack.pack_id).is_some(),
        mine: rec.mine,
        stickers: rec.stickers,
    })
}

/// Keep the pack a sticker belongs to.
#[uniffi::export]
pub async fn install_sticker_pack(sticker: StickerRecord) -> Result<(), CoreError> {
    let r = to_ref(&sticker)?;
    on_runtime(async move { crate::stickers::install(&r).await }).await
}

#[uniffi::export]
pub fn remove_sticker_pack(pack: Vec<u8>) -> Result<(), CoreError> {
    let pack = to_did16(&pack)?;
    crate::stickers::remove(&pack).map_err(Into::into)
}

fn log_publish(e: &anyhow::Error) {
    log::warn!("STICKERS: publish failed: {e:#}");
}

/// Publish a new pack from decoded pictures. Returns the pack id once the
/// store has every object; the pack is kept locally at the same time.
#[uniffi::export]
pub async fn create_sticker_pack(
    name: String, images: Vec<StickerSource>,
) -> Result<Vec<u8>, CoreError> {
    let images = images
        .into_iter()
        .map(|i| SourceImage { rgba: i.rgba, width: i.width, height: i.height })
        .collect();
    let id =
        on_runtime(
            async move { crate::stickers::create(name, images).await.inspect_err(log_publish) },
        )
        .await?;
    Ok(id.to_vec())
}

/// Add pictures to a pack this identity created.
#[uniffi::export]
pub async fn add_to_sticker_pack(
    pack: Vec<u8>, images: Vec<StickerSource>,
) -> Result<(), CoreError> {
    let pack = to_did16(&pack)?;
    let images = images
        .into_iter()
        .map(|i| SourceImage { rgba: i.rgba, width: i.width, height: i.height })
        .collect();
    on_runtime(async move { crate::stickers::append(pack, images).await.inspect_err(log_publish) })
        .await
}

/// Save the outgoing message and schedule delivery. Status updates arrive through core events.
#[uniffi::export]
pub fn send_sticker(
    conversation_id: Vec<u8>, sticker: StickerRecord, reply_to: Option<Vec<u8>>,
) -> Result<(), CoreError> {
    let to = to_conv16(&conversation_id)?;
    let reply_to = reply_to.as_deref().map(to_did16).transpose()?;
    let r = to_ref(&sticker)?;
    let row = crate::data::media::MediaRow {
        kind: crate::data::media::KIND_STICKER,
        group_id: None,
        mime: "image/avif".into(),
        name: String::new(),
        size: 0,
        width: sticker.width.min(u16::MAX as u32),
        height: sticker.height.min(u16::MAX as u32),
        duration_ms: 0,
        blob: None,
        thumb: None,
        file_id: None,
        sticker: Some(r.ser().map_err(|e| anyhow::anyhow!("encode sticker ref: {e}"))?),
    };
    let msg = crate::data::media::save_outgoing_with_media(&to, "", reply_to, &row)?;
    let _ = db::touch_recent(&r.pack, &r.id, crate::utils::systime().as_secs());
    crate::RUNTIME.spawn(async move {
        let sent = async {
            let payload = crate::messaging::rebuild_pending_payload(&to, &msg)?;
            crate::messaging::send_prepared(to, &msg, payload).await
        };
        if let Err(e) = sent.await {
            log::warn!("STICKERS: send deferred to retry: {e}");
        }
    });
    Ok(())
}

/// Look for appends to kept packs, in the background. Cheap to call on every
/// picker open; core rate-limits per pack.
#[uniffi::export]
pub fn refresh_sticker_packs() {
    crate::RUNTIME.spawn(crate::stickers::refresh_kept());
}

#[uniffi::export]
pub fn sticker_cache_bytes() -> Result<u64, CoreError> {
    crate::stickers::cache_bytes().map_err(Into::into)
}

#[uniffi::export]
pub async fn clear_sticker_cache() -> Result<(), CoreError> {
    on_runtime(crate::stickers::clear_cache()).await
}
