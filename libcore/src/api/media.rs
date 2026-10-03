//! Media FFI: images, attachments, voice notes and transfer state.

use common::types::bytes::fixed;

use crate::platform::CoreError;
use crate::state::core;

/// Eligibility for recipient uploads only. Unknown/lost/metered networks
/// must report false; this never restricts the original sender's transfers.
#[uniffi::export]
pub fn set_attachment_sharing_network(unmetered_wifi: bool) {
    crate::transfer::sharing::set_network(unmetered_wifi);
}

/// Process-local debug events with fixed categories only, never peer or file identifiers.
#[derive(uniffi::Record)]
pub struct TransferDiagnosticEvent {
    pub at_ms: u64,
    pub category: String,
}

#[derive(uniffi::Record)]
pub struct TransferDiagnostics {
    pub events: Vec<TransferDiagnosticEvent>,
    /// Local egress: QUIC payload bytes handed to the UDP socket or TCP queue, with overhead,
    /// retransmissions and route-setup duplicates, without IP or relay envelopes.
    pub direct_datagram_bytes_sent: u64,
    pub relay_datagram_bytes_sent: u64,
    /// Datagrams shed when a TCP attachment route's send queue is full.
    /// QUIC retransmits them without blocking other peer connections.
    pub tcp_queue_datagrams_dropped: u64,
    /// File chunk payload accepted by the send stream, including re-sends.
    pub content_bytes_sent: u64,
    /// Chunk payload verified by this receiver, including re-verification.
    pub verified_content_bytes_received: u64,
}

#[uniffi::export]
pub fn get_transfer_diagnostics() -> TransferDiagnostics {
    let snapshot = crate::p2p::diagnostics::snapshot();
    TransferDiagnostics {
        events: snapshot.events.into_iter().map(|(at_ms, event)| TransferDiagnosticEvent {
            at_ms, category: format!("{event:?}"),
        }).collect(),
        direct_datagram_bytes_sent: snapshot.direct_sent,
        relay_datagram_bytes_sent: snapshot.relay_sent,
        tcp_queue_datagrams_dropped: snapshot.tcp_queue_drops,
        content_bytes_sent: snapshot.content_sent,
        verified_content_bytes_received: snapshot.verified_received,
    }
}

#[derive(uniffi::Record)]
pub struct MediaRecord {
    pub dispatch_id: Vec<u8>,
    pub kind: u8,
    pub group_id: Option<Vec<u8>>,
    pub mime: String,
    pub name: String,
    pub size: u64,
    pub width: u32,
    pub height: u32,
    /// Voice only.
    pub duration_ms: u32,
    pub blob: Option<Vec<u8>>,
    /// Blurred preview for an attachment; the loudness waveform for a voice note.
    pub thumb: Option<Vec<u8>>,
    pub file_id: Option<Vec<u8>>,
    pub transfer_state: u8,
    pub transfer_have: u32,
    pub transfer_total: u32,
    pub local_path: Option<String>,
    /// Sticker only: what to hand `sticker_image` for its bytes.
    pub sticker: Option<crate::api::stickers::StickerRecord>,
}

/// Encodes AVIF within the 256 KB inline budget. The `Result` reports only synchronous errors;
/// the send outcome arrives via `on_message`.
#[uniffi::export]
pub fn send_image(
    conversation_id: Vec<u8>, rgba: Vec<u8>, width: u32, height: u32, caption: String,
    group_id: Option<Vec<u8>>,
) -> Result<(), CoreError> {
    let to = fixed::<16>(&conversation_id, "conversation id")?;
    let gid = group_id.as_deref().map(|b| fixed::<16>(b, "dispatch_id")).transpose()?;
    // Placeholder row first, so the bubble shows while the encode below blocks this call.
    let msg = crate::messaging::build_image_message(to, width, height, &caption, gid)?;
    let did: [u8; 16] = msg
        .inner
        .dispatch_id
        .as_deref()
        .and_then(|d| d.try_into().ok())
        .expect("save_outgoing mints a dispatch_id");
    // Synchronous so an over-budget photo's error reaches the caller, who then sends an attachment.
    let (avif, w, h) = match crate::media::compress_image(&rgba, width, height, 256 * 1024) {
        Ok(v) => v,
        Err(e) => {
            let _ = crate::data::media::discard_outgoing(&to, &did);
            return Err(e.into());
        },
    };
    core().spawn(async move {
        if let Err(e) = crate::messaging::finish_image(to, did, avif, w, h).await {
            log::warn!("MEDIA: send_image failed: {e}");
        }
    });
    Ok(())
}

/// The bytes are pulled device-to-device by `file_id`. `source_path` becomes core's and is unlinked
/// once no message shows it, so hand over a private copy, never the user's original.
#[uniffi::export]
pub fn send_attachment(
    conversation_id: Vec<u8>, source_path: String, name: String, mime: String,
    thumb_rgba: Option<Vec<u8>>, thumb_w: u32, thumb_h: u32, caption: String,
    group_id: Option<Vec<u8>>,
) -> Result<(), CoreError> {
    let to = fixed::<16>(&conversation_id, "conversation id")?;
    let gid = group_id.as_deref().map(|b| fixed::<16>(b, "dispatch_id")).transpose()?;
    let size = std::fs::metadata(&source_path)
        .map_err(|e| anyhow::anyhow!("stat {source_path}: {e}"))?
        .len();
    // Blur is light next to the hash below, so it stays synchronous and the placeholder carries it.
    let thumb = thumb_rgba.map(|r| crate::media::attachment_thumb(&mime, &r, thumb_w, thumb_h)).transpose()?;
    // Placeholder row first, so the bubble shows while `prepare_send` hashes the file off-thread.
    let msg = crate::messaging::build_attachment_message(to, size, &name, &mime, thumb, &caption, gid)?;
    let did: [u8; 16] = msg
        .inner
        .dispatch_id
        .as_deref()
        .and_then(|d| d.try_into().ok())
        .expect("save_outgoing mints a dispatch_id");
    core().spawn(async move {
        // Held from before the copy is retained until the row names it, so a cleanup of the same
        // content meanwhile keeps the fresh copy.
        let mut hold = None;
        let held = |f| hold = Some(crate::staging::Hold::new(f));
        // Only a prepare failure discards the placeholder, since the offer never existed. A later
        // send failure leaves the row pending for retry.
        let file_id = match crate::transfer::prepare_send(&source_path, 7 * 24 * 3600, held) {
            Ok((file_id, _size)) => file_id,
            Err(e) => {
                log::warn!("MEDIA: send_attachment prepare failed: {e}");
                let _ = crate::data::media::discard_outgoing(&to, &did);
                return;
            },
        };
        if let Err(e) = crate::messaging::finish_attachment(to, did, file_id).await {
            log::warn!("MEDIA: send_attachment send deferred to retry: {e}");
        }
        drop(hold);
    });
    Ok(())
}

/// Inline like an image and capped the same way. The row is on screen before this returns.
#[uniffi::export]
pub fn send_voice(
    conversation_id: Vec<u8>, data: Vec<u8>, mime: String, duration_ms: u32, waveform: Vec<u8>,
    reply_to: Option<Vec<u8>>,
) -> Result<(), CoreError> {
    let to = fixed::<16>(&conversation_id, "conversation id")?;
    let reply_to = reply_to.as_deref().map(|b| fixed::<16>(b, "dispatch_id")).transpose()?;
    if data.is_empty() || data.len() > VOICE_MAX_BYTES {
        return Err(anyhow::anyhow!("voice note must be 1..={VOICE_MAX_BYTES} bytes").into());
    }
    let row = crate::data::media::MediaRow {
        kind: crate::data::media::KIND_VOICE,
        group_id: None,
        mime,
        name: String::new(),
        size: data.len() as u64,
        width: 0,
        height: 0,
        duration_ms,
        blob: Some(data),
        thumb: (!waveform.is_empty()).then_some(waveform),
        file_id: None,
        sticker: None,
    };
    let msg = crate::data::media::save_outgoing_with_media(&to, "", reply_to, &row)?;
    core().spawn(async move {
        let sent = async {
            let payload = crate::messaging::body::rebuild_pending_payload(&to, &msg)?;
            crate::messaging::send_prepared(to, &msg, payload).await
        };
        if let Err(e) = sent.await {
            log::warn!("MEDIA: send_voice deferred to retry: {e}");
        }
    });
    Ok(())
}

/// Same ceiling as an inline image: the frame is what holds it.
pub const VOICE_MAX_BYTES: usize = 256 * 1024;

/// Dials or reverse-wakes the sender; progress surfaces through `get_media`'s `transfer_state`.
#[uniffi::export]
pub fn download_attachment(file_id: Vec<u8>) -> Result<(), CoreError> {
    let fid = fixed::<32>(&file_id, "file_id")?;
    core().spawn(async move {
        if let Err(e) = crate::transfer::download(fid).await {
            log::warn!("MEDIA: download_attachment failed: {e}");
        }
    });
    Ok(())
}

/// Transfer progress counts chunks. `local_path` appears only once the download is complete; the
/// `.part` file holds unverified bytes no platform should open.
#[uniffi::export]
pub fn get_media(conversation_id: Vec<u8>, limit: u32) -> Result<Vec<MediaRecord>, CoreError> {
    let conv = fixed::<16>(&conversation_id, "conversation id")?;
    let rows = crate::data::media::for_conversation(&conv, limit)?;
    Ok(rows.into_iter().map(|(did, r)| media_record(did, r)).collect())
}

#[uniffi::export]
pub fn get_message_media(conversation_id: Vec<u8>, dispatch_id: Vec<u8>) -> Result<Option<MediaRecord>, CoreError> {
    let conv = fixed::<16>(&conversation_id, "conversation id")?;
    let did = fixed::<16>(&dispatch_id, "dispatch_id")?;
    Ok(crate::data::media::get(&conv, &did)?.map(|r| media_record(did, r)))
}

fn media_record(did: [u8; 16], r: crate::data::media::MediaRow) -> MediaRecord {
    use crate::transfer::store;
    let fid = r.file_id.as_deref().and_then(|f| <&[u8; 32]>::try_from(f).ok());
    let (transfer_state, transfer_have, transfer_total, local_path) = match fid.and_then(store::partial_get) {
        Some(p) => {
            // A DONE row can outlive its bytes; report it PENDING so a tap re-pulls the file.
            let complete = p.is_complete();
            let state =
                if p.state == store::DONE && !complete { store::PENDING } else { p.state };
            (
                state,
                store::verified_count(&p),
                p.total.div_ceil(p.chunk_size.max(1) as u64) as u32,
                complete.then(|| p.path.clone()),
            )
        },
        // No receiver partial: it may be our own sent attachment, kept in `retention` by file id.
        None => match fid.and_then(store::retention_get) {
            Some(ret) => {
                let chunks = ret.size.div_ceil(ret.chunk_size.max(1) as u64) as u32;
                let present = std::fs::metadata(&ret.path).is_ok();
                (store::DONE, chunks, chunks, present.then_some(ret.path))
            },
            None => (store::PENDING, 0, 0, None),
        },
    };
    let sticker = r.sticker.as_deref().and_then(|b| {
        use common::proto::pack::Unpacker;
        let s = common::proto::sticker::StickerRef::deser(b).ok()?;
        Some(crate::api::stickers::StickerRecord {
            pack:   s.pack.to_vec(),
            id:     s.id.to_vec(),
            token:  s.token.to_vec(),
            store:  s.store,
            width:  r.width,
            height: r.height,
        })
    });
    MediaRecord {
        dispatch_id: did.to_vec(),
        kind: r.kind,
        group_id: r.group_id,
        mime: r.mime,
        name: r.name,
        size: r.size,
        width: r.width,
        height: r.height,
        duration_ms: r.duration_ms,
        blob: r.blob,
        thumb: r.thumb,
        file_id: r.file_id,
        transfer_state,
        transfer_have,
        transfer_total,
        local_path,
        sticker,
    }
}

/// Lightweight browsing index. Read bytes only for a visible preview or opened item.
#[derive(uniffi::Record)]
pub struct SharedMediaItem {
    pub sender_name: String,
    pub dispatch_id: Vec<u8>,
    pub kind: u8,
    pub name: String,
    pub mime: String,
    pub size: u64,
    pub timestamp: u64,
}

#[uniffi::export]
pub fn shared_media(conversation_id: Vec<u8>) -> Result<Vec<SharedMediaItem>, CoreError> {
    let conv = fixed::<16>(&conversation_id, "conversation id")?;
    let result = (|| -> anyhow::Result<_> {
        let rows = {
            let db = core().db.messages().lock();
            let mut query = db.prepare("SELECT mm.dispatch_id, mm.kind, mm.name, mm.mime, mm.size, m.timestamp, m.sender_ipk, m.outgoing
                FROM message_media mm JOIN messages m ON m.conversation_id=mm.conversation_id AND m.dispatch_id=mm.dispatch_id
                WHERE m.conversation_id=?1 AND m.deleted=0 ORDER BY m.id DESC")?;
            query.query_map([conv.as_slice()], |r| Ok((SharedMediaItem {
                sender_name: String::new(), dispatch_id: r.get(0)?, kind: r.get(1)?, name: r.get(2)?, mime: r.get(3)?, size: r.get(4)?, timestamp: r.get(5)?
            }, r.get::<_, Option<[u8; 32]>>(6)?, r.get::<_, bool>(7)?)))?.collect::<rusqlite::Result<Vec<_>>>()?
        };
        // Name resolution owns its own locks. Never call it under the messages lock.
        Ok(rows.into_iter().map(|(mut row, sender, outgoing)| {
            row.sender_name = if outgoing { "You".into() } else { sender.map(|who| crate::data::peer_name::resolve(&who)).unwrap_or_default() };
            row
        }).collect::<Vec<_>>())
    })();
    result.map_err(Into::into)
}
