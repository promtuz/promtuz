//! The wire [`Body`] against its stored rows, and which kinds a revision may swap.

use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use common::proto::mls_wire::AppPayload;
use common::proto::mls_wire::Body;
use common::proto::pack::Packer;

use crate::data::message::Message;

/// Returns the stored row with the text to surface, a caption for media.
pub(crate) fn save_inbound_body(
    conversation: &[u8; 16], sender: &[u8; 32], did: &[u8; 16], timestamp: u64,
    reply_to: Option<[u8; 16]>, body: Body,
) -> Result<Option<(Message, String)>> {
    let (content, media) = split_body(body);
    Ok(match media {
        None => Message::save_incoming(*conversation, *sender, did, &content, timestamp, reply_to)?
            .map(|m| (m, content)),
        Some(r) => crate::data::media::save_incoming_with_media(
            conversation,
            sender,
            did,
            &content,
            timestamp,
            reply_to,
            &r,
        )?
        .map(|m| (m, content)),
    })
}

/// The text to surface and the media side row, if any. A sticker stores only its reference; the
/// picture is fetched by it and cached under the pack.
fn split_body(body: Body) -> (String, Option<crate::data::media::MediaRow>) {
    use crate::data::media::KIND_ATTACHMENT;
    use crate::data::media::KIND_IMAGE;
    use crate::data::media::KIND_STICKER;
    use crate::data::media::KIND_VOICE;
    use crate::data::media::MediaRow;

    match body {
        Body::Text(content) => (content, None),
        Body::Image { caption, group_id, mime, width, height, data } => (
            caption,
            Some(MediaRow {
                kind: KIND_IMAGE,
                group_id: group_id.map(|g| g.to_vec()),
                mime,
                name: String::new(),
                size: data.len() as u64,
                width,
                height,
                blob: Some(data),
                thumb: None,
                file_id: None,
                duration_ms: 0,
                sticker: None,
            }),
        ),
        Body::Attachment { caption, group_id, mime, name, size, thumb, file_id } => (
            caption,
            Some(MediaRow {
                kind: KIND_ATTACHMENT,
                group_id: group_id.map(|g| g.to_vec()),
                mime,
                name,
                size,
                width: 0,
                height: 0,
                blob: None,
                thumb: (!thumb.is_empty()).then_some(thumb),
                file_id: Some(file_id.to_vec()),
                duration_ms: 0,
                sticker: None,
            }),
        ),
        Body::Voice { mime, duration_ms, waveform, data } => (
            String::new(),
            Some(MediaRow {
                kind: KIND_VOICE,
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
            }),
        ),
        Body::Sticker { pack, id, token, store, width, height } => {
            use common::proto::pack::Packer;
            let r = common::proto::sticker::StickerRef { pack, id, token, store };
            (
                String::new(),
                Some(MediaRow {
                    kind: KIND_STICKER,
                    group_id: None,
                    mime: "image/avif".into(),
                    name: String::new(),
                    size: 0,
                    width: width as u32,
                    height: height as u32,
                    duration_ms: 0,
                    blob: None,
                    thumb: None,
                    file_id: None,
                    sticker: r.ser().ok(),
                }),
            )
        },
    }
}

/// Persists the new body in place unless the matrix refuses the swap. With `own = false` a peer
/// may only revise its own messages; `None` when the target is unknown, tombstoned or not theirs.
pub(crate) fn apply_revise_body(
    conversation: &[u8; 16], target: &[u8; 16], body: Body, own: bool, author: Option<&[u8; 32]>,
) -> Result<Option<(crate::db::messages::MessageRow, String)>> {
    let current = BodyKind::stored(crate::data::media::get(conversation, target)?.map(|m| m.kind));
    let incoming = BodyKind::of(&body);
    if !current.revisable_to(incoming) {
        bail!("revision {current:?} -> {incoming:?} is not permitted");
    }
    let (content, media) = split_body(body);
    Ok(crate::data::media::apply_revise(
        conversation,
        target,
        &content,
        media.as_ref(),
        own,
        author,
    )?
    .map(|row| (row, content)))
}

/// A pre-v12 content payload as a [`Body`] and quote target; `None` for anything but content.
pub(crate) fn legacy_body(p: AppPayload) -> Option<(Option<[u8; 16]>, Body)> {
    Some(match p {
        AppPayload::Text(content) => (None, Body::Text(content)),
        AppPayload::Reply { reply_to, content } => (Some(reply_to), Body::Text(content)),
        AppPayload::Image { caption, group_id, mime, width, height, data } => {
            (None, Body::Image { caption, group_id, mime, width, height, data })
        },
        AppPayload::Attachment { caption, group_id, mime, name, size, thumb, file_id } => {
            (None, Body::Attachment { caption, group_id, mime, name, size, thumb, file_id })
        },
        _ => return None,
    })
}

/// Storage keeps a media side-row kind rather than a [`Body`], so the revision rule is expressed
/// over this discriminant.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum BodyKind {
    Text,
    Image,
    Attachment,
    Sticker,
    Voice,
}

impl BodyKind {
    pub(crate) fn of(body: &Body) -> Self {
        match body {
            Body::Text(..) => Self::Text,
            Body::Image { .. } => Self::Image,
            Body::Attachment { .. } => Self::Attachment,
            Body::Sticker { .. } => Self::Sticker,
            Body::Voice { .. } => Self::Voice,
        }
    }

    pub(crate) fn stored(media_kind: Option<u8>) -> Self {
        match media_kind {
            Some(crate::data::media::KIND_IMAGE) => Self::Image,
            Some(crate::data::media::KIND_ATTACHMENT) => Self::Attachment,
            Some(crate::data::media::KIND_VOICE) => Self::Voice,
            Some(crate::data::media::KIND_STICKER) => Self::Sticker,
            _ => Self::Text,
        }
    }

    /// Text and image ride in the frame, so they interchange. Revising into or out of an
    /// attachment strands a transfer; stickers and voice notes revise only to their own kind.
    pub(crate) fn revisable_to(self, to: Self) -> bool {
        matches!(
            (self, to),
            (Self::Text | Self::Image, Self::Text | Self::Image)
                | (Self::Attachment, Self::Attachment)
                | (Self::Sticker, Self::Sticker)
                | (Self::Voice, Self::Voice)
        )
    }
}

/// The wire [`AppPayload::Post`] for a pending row, rebuilt from its stored body so a retry keeps
/// its media.
pub(crate) fn rebuild_pending_payload(conversation: &[u8; 16], msg: &Message) -> Result<Vec<u8>> {
    let reply_to = msg.inner.reply_to.as_deref().and_then(|r| r.try_into().ok());
    let body = stored_body(conversation, msg)?;
    AppPayload::Post { reply_to, body }.ser().map_err(|e| anyhow!("encode AppPayload: {e}"))
}

pub(super) fn stored_body(conversation: &[u8; 16], msg: &Message) -> Result<Body> {
    let did = msg.inner.dispatch_id.as_slice().try_into()
        .map_err(|_| anyhow!("invalid dispatch id"))?;
    let media = crate::data::media::get(conversation, &did)?;
    join_body(msg.inner.content.clone(), media)
}

/// The inverse of [`split_body`].
fn join_body(content: String, media: Option<crate::data::media::MediaRow>) -> Result<Body> {
    let body = match media {
        Some(m) if m.kind == crate::data::media::KIND_IMAGE => {
            // An empty blob is a placeholder still encoding: bail so a retry leaves the row pending
            // for `finish_image`.
            let data = match m.blob {
                Some(b) if !b.is_empty() => b,
                _ => bail!("media not ready"),
            };
            Body::Image {
                caption: content,
                group_id: m.group_id.as_deref().and_then(|g| g.try_into().ok()),
                mime: m.mime,
                width: m.width,
                height: m.height,
                data,
            }
        },
        Some(m) if m.kind == crate::data::media::KIND_VOICE => Body::Voice {
            mime:        m.mime,
            duration_ms: m.duration_ms,
            waveform:    m.thumb.unwrap_or_default(),
            data:        m.blob.unwrap_or_default(),
        },
        Some(m) if m.kind == crate::data::media::KIND_STICKER => {
            use common::proto::pack::Unpacker;
            let r = m
                .sticker
                .as_deref()
                .and_then(|b| common::proto::sticker::StickerRef::deser(b).ok())
                .ok_or_else(|| anyhow!("sticker row carries no reference"))?;
            Body::Sticker {
                pack:   r.pack,
                id:     r.id,
                token:  r.token,
                store:  r.store,
                width:  m.width.min(u16::MAX as u32) as u16,
                height: m.height.min(u16::MAX as u32) as u16,
            }
        },
        Some(m) if m.kind == crate::data::media::KIND_ATTACHMENT => {
            // A null file id is a placeholder still hashing.
            let file_id = match m.file_id.as_deref().and_then(|f| f.try_into().ok()) {
                Some(f) => f,
                None => bail!("media not ready"),
            };
            Body::Attachment {
                caption:  content,
                group_id: m.group_id.as_deref().and_then(|g| g.try_into().ok()),
                mime:     m.mime,
                name:     m.name,
                size:     m.size,
                thumb:    m.thumb.unwrap_or_default(),
                file_id,
            }
        },
        _ => Body::Text(content),
    };
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Text and image ride in the frame, so they swap; an attachment, a sticker and a voice note
    /// revise only to their own kind.
    #[test]
    fn text_and_images_swap_but_other_kinds_revise_only_to_themselves() {
        use BodyKind::*;
        let kinds = [Text, Image, Attachment, Sticker, Voice];
        // Columns are the kind revised to, in the same order.
        let allowed = [
            [1, 1, 0, 0, 0], // Text
            [1, 1, 0, 0, 0], // Image
            [0, 0, 1, 0, 0], // Attachment
            [0, 0, 0, 1, 0], // Sticker
            [0, 0, 0, 0, 1], // Voice
        ];
        for (from, row) in kinds.into_iter().zip(allowed) {
            for (to, want) in kinds.into_iter().zip(row) {
                assert_eq!(from.revisable_to(to), want == 1, "{from:?} -> {to:?}");
            }
        }
    }

    /// A pending row rebuilds the body it was saved from, so a resend never drops the picture,
    /// file or voice note behind its caption. A row still being prepared has nothing to send.
    #[test]
    fn a_stored_body_resends_as_saved_and_a_placeholder_waits() {
        let image = Body::Image {
            caption:  "look at this".into(),
            group_id: Some([1; 16]),
            mime:     "image/avif".into(),
            width:    4,
            height:   3,
            data:     vec![7, 8, 9],
        };
        let attachment = Body::Attachment {
            caption:  String::new(),
            group_id: None,
            mime:     "text/plain".into(),
            name:     "notes.txt".into(),
            size:     12,
            thumb:    vec![5],
            file_id:  [3; 32],
        };
        let bodies = [
            Body::Text("hi".into()),
            image.clone(),
            attachment.clone(),
            Body::Voice {
                mime:        "audio/ogg".into(),
                duration_ms: 4200,
                waveform:    vec![1, 9, 200],
                data:        vec![0x4f, 0x67],
            },
            Body::Sticker {
                pack:   [4; 16],
                id:     [5; 32],
                token:  [6; 32],
                store:  2,
                width:  512,
                height: 384,
            },
        ];
        for body in bodies {
            let (content, media) = split_body(body.clone());
            assert_eq!(BodyKind::stored(media.as_ref().map(|m| m.kind)), BodyKind::of(&body));
            assert_eq!(join_body(content, media).unwrap(), body);
        }

        let (caption, mut encoding) = split_body(image);
        encoding.as_mut().unwrap().blob = None;
        assert!(join_body(caption, encoding).is_err(), "an image still encoding");
        let (caption, mut hashing) = split_body(attachment);
        hashing.as_mut().unwrap().file_id = None;
        assert!(join_body(caption, hashing).is_err(), "a file still hashing");
    }
}
