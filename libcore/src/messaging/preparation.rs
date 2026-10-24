//! One message from preparation through completion, including reordered v12 fallback posts.

use anyhow::{Result, ensure};
use common::proto::media::{CompletedMedia, MediaPresentation, MediaUpdate, PreparingMedia};
use common::proto::mls_wire::Body;
use rusqlite::{Connection, OptionalExtension};

use crate::data::{
    media::{self, MediaRow},
    message::Message,
};
use crate::db::messages::MessageRow;
use crate::events::{Emittable, messaging::MessageEv};
use crate::state::core;

struct Stored {
    message: Message,
    first: bool,
}

pub(super) fn receive(
    conversation: [u8; 16], author: [u8; 32], timestamp: u64, update: MediaUpdate,
) -> Result<()> {
    let auto = match &update {
        MediaUpdate::Complete(CompletedMedia {
            body: Body::Attachment { size, file_id, .. },
            ..
        }) => Some((*size, *file_id)),
        _ => None,
    };
    let saved = match update {
        MediaUpdate::Preparing(pending) => prepare(&conversation, &author, timestamp, pending)?,
        MediaUpdate::Complete(completed) => complete(&conversation, &author, timestamp, completed)?,
    };
    if let Some(Stored { message, first }) = saved {
        let row = message.inner;
        if first {
            MessageEv::Received {
                id: row.id,
                conversation,
                sender: author,
                content: row.content,
                timestamp: row.timestamp,
            }
            .emit();
        } else {
            MessageEv::Edited { id: row.id, conversation, content: row.content }.emit();
        }
        crate::data::receipts::schedule();
        if let Some((size, file_id)) = auto
            && crate::transfer::should_auto_download(&author, size, false)
        {
            core().spawn(async move {
                let _ = crate::transfer::download(file_id).await;
            });
        }
    }
    Ok(())
}

fn validate_presentation(presentation: MediaPresentation) -> Result<()> {
    ensure!(
        presentation.width > 0
            && presentation.height > 0
            && presentation.width <= 16_384
            && presentation.height <= 16_384
            && u64::from(presentation.width) * u64::from(presentation.height) <= 32_000_000,
        "invalid media presentation"
    );
    Ok(())
}

fn current(
    db: &Connection, conversation: &[u8; 16], target: &[u8; 16], author: &[u8; 32],
) -> Result<Option<PreparingMedia>> {
    let bytes: Option<Vec<u8>> = db
        .query_row(
            "SELECT p.details FROM message_media_preparations p JOIN messages m
         ON m.conversation_id = p.conversation_id AND m.dispatch_id = p.dispatch_id
         WHERE p.conversation_id = ?1 AND p.dispatch_id = ?2
           AND m.sender_ipk = ?3 AND m.outgoing = 0 AND m.deleted = 0",
            (conversation.as_slice(), target.as_slice(), author.as_slice()),
            |row| row.get(0),
        )
        .optional()?;
    bytes.map(|bytes| postcard::from_bytes(&bytes).map_err(Into::into)).transpose()
}

fn prepare(
    conversation: &[u8; 16], author: &[u8; 32], timestamp: u64, pending: PreparingMedia,
) -> Result<Option<Stored>> {
    validate_presentation(pending.presentation)?;
    ensure!(
        matches!(pending.mime.as_str(), "video/mp4" | "image/avif"),
        "invalid prepared media type"
    );
    ensure!(
        pending.name.len() <= 1024
            && pending.caption.len() <= 64 * 1024
            && pending.thumb.len() <= 32 * 1024,
        "media preparation metadata exceeds limits"
    );
    let mut db = core().db.messages().lock();
    let tx = db.transaction()?;
    let previous = current(&tx, conversation, &pending.target, author)?;
    let first = previous.is_none();
    let message = if let Some(previous) = previous {
        if pending.attempt.generation < previous.attempt.generation {
            return Ok(None);
        }
        if pending.attempt.generation == previous.attempt.generation {
            ensure!(pending == previous, "conflicting media attempt");
            return Ok(None);
        }
        ensure!(
            pending.mime == previous.mime
                && pending.reply_to == previous.reply_to
                && pending.group_id == previous.group_id,
            "media preparation changed its message"
        );
        tx.execute(
            "UPDATE messages SET content = ?3 WHERE conversation_id = ?1 AND dispatch_id = ?2",
            (conversation.as_slice(), pending.target.as_slice(), &pending.caption),
        )?;
        Message {
            inner: tx.query_row(
                "SELECT * FROM messages WHERE conversation_id = ?1 AND dispatch_id = ?2",
                (conversation.as_slice(), pending.target.as_slice()),
                MessageRow::from_row,
            )?,
        }
    } else {
        let Some(message) = Message::save_incoming_tx(
            &tx,
            *conversation,
            *author,
            &pending.target,
            &pending.caption,
            timestamp,
            pending.reply_to,
        )?
        else {
            return Ok(None);
        };
        message
    };
    media::save_tx(
        &tx,
        conversation,
        &pending.target,
        &MediaRow {
            kind: media::KIND_ATTACHMENT,
            group_id: pending.group_id.map(|id| id.to_vec()),
            mime: pending.mime.clone(),
            name: pending.name.clone(),
            size: 0,
            width: pending.presentation.width,
            height: pending.presentation.height,
            duration_ms: pending.presentation.duration_ms,
            blob: None,
            thumb: (!pending.thumb.is_empty()).then(|| pending.thumb.clone()),
            file_id: None,
            sticker: None,
            hd: None,
        },
    )?;
    tx.execute(
        "INSERT INTO message_media_preparations (conversation_id, dispatch_id, details)
                VALUES (?1, ?2, ?3)",
        (conversation.as_slice(), pending.target.as_slice(), postcard::to_allocvec(&pending)?),
    )?;
    tx.commit()?;
    Ok(Some(Stored { message, first }))
}

fn complete(
    conversation: &[u8; 16], author: &[u8; 32], timestamp: u64, completed: CompletedMedia,
) -> Result<Option<Stored>> {
    let CompletedMedia { target, attempt, reply_to, presentation, body } = completed;
    validate_presentation(presentation)?;
    let (caption, media) = super::body::split_body(body);
    let Some(mut media) = media else { anyhow::bail!("media completion has no media") };
    ensure!(
        (media.kind == media::KIND_ATTACHMENT && media.mime == "video/mp4")
            || (media.kind == media::KIND_IMAGE && media.mime == "image/avif"),
        "invalid media completion"
    );
    let mut db = core().db.messages().lock();
    let tx = db.transaction()?;
    let previous = current(&tx, conversation, &target, author)?;
    let first = previous.is_none();
    let message = if let Some(previous) = previous {
        if attempt.generation < previous.attempt.generation {
            return Ok(None);
        }
        ensure!(
            attempt.generation != previous.attempt.generation || attempt.id == previous.attempt.id,
            "conflicting media completion"
        );
        ensure!(
            media.mime == previous.mime
                && reply_to == previous.reply_to
                && media.group_id == previous.group_id.map(|id| id.to_vec()),
            "media completion changed its message"
        );
        tx.execute(
            "UPDATE messages SET content = ?3 WHERE conversation_id = ?1 AND dispatch_id = ?2",
            (conversation.as_slice(), target.as_slice(), &caption),
        )?;
        Message {
            inner: tx.query_row(
                "SELECT * FROM messages WHERE conversation_id = ?1 AND dispatch_id = ?2",
                (conversation.as_slice(), target.as_slice()),
                MessageRow::from_row,
            )?,
        }
    } else {
        let Some(message) = Message::save_incoming_tx(
            &tx,
            *conversation,
            *author,
            &target,
            &caption,
            timestamp,
            reply_to,
        )?
        else {
            // COMPAT(v12): media-completion. A finished Post may outrun its new-style completion.
            let saved =
                complete_fallback(&tx, conversation, author, &target, &media, presentation)?;
            tx.commit()?;
            return Ok(saved);
        };
        message
    };
    media.width = presentation.width;
    media.height = presentation.height;
    media.duration_ms = presentation.duration_ms;
    media::save_tx(&tx, conversation, &target, &media)?;
    tx.commit()?;
    Ok(Some(Stored { message, first }))
}

fn complete_fallback(
    db: &Connection, conversation: &[u8; 16], author: &[u8; 32], target: &[u8; 16],
    media: &MediaRow, presentation: MediaPresentation,
) -> Result<Option<Stored>> {
    let row = db
        .query_row(
            "SELECT * FROM messages WHERE conversation_id = ?1 AND dispatch_id = ?2
         AND sender_ipk = ?3 AND outgoing = 0 AND deleted = 0",
            (conversation.as_slice(), target.as_slice(), author.as_slice()),
            MessageRow::from_row,
        )
        .optional()?;
    let Some(row) = row else { return Ok(None) };
    let existing = db
        .query_row(
            "SELECT * FROM message_media WHERE conversation_id = ?1 AND dispatch_id = ?2",
            (conversation.as_slice(), target.as_slice()),
            MediaRow::from_row,
        )
        .optional()?;
    let Some(existing) = existing else { return Ok(None) };
    if existing.kind != media.kind
        || existing.mime != media.mime
        || existing.size != media.size
        || existing.file_id != media.file_id
        || existing.blob != media.blob
        || (existing.width, existing.height, existing.duration_ms)
            == (presentation.width, presentation.height, presentation.duration_ms)
    {
        return Ok(None);
    }
    db.execute(
        "UPDATE message_media SET width = ?3, height = ?4, duration_ms = ?5
                WHERE conversation_id = ?1 AND dispatch_id = ?2",
        (
            conversation.as_slice(),
            target.as_slice(),
            presentation.width,
            presentation.height,
            presentation.duration_ms,
        ),
    )?;
    Ok(Some(Stored { message: Message { inner: row }, first: false }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{ScopedCore, mls::with_failing_trigger};
    use common::proto::media::MediaAttempt;

    const CONVERSATION: [u8; 16] = [1; 16];
    const AUTHOR: [u8; 32] = [2; 32];

    fn pending(generation: u32) -> PreparingMedia {
        PreparingMedia {
            target: [3; 16],
            attempt: MediaAttempt { generation, id: [generation as u8; 16] },
            reply_to: None,
            caption: "clip".into(),
            group_id: None,
            mime: "video/mp4".into(),
            name: "video.mp4".into(),
            presentation: MediaPresentation { width: 480, height: 854, duration_ms: 2_000 },
            thumb: vec![],
        }
    }

    fn completed(generation: u32) -> CompletedMedia {
        let pending = pending(generation);
        CompletedMedia {
            target: pending.target,
            attempt: pending.attempt,
            reply_to: pending.reply_to,
            presentation: pending.presentation,
            body: Body::Attachment {
                caption: pending.caption,
                group_id: None,
                mime: pending.mime,
                name: pending.name,
                size: 12,
                thumb: vec![],
                file_id: [4; 32],
            },
        }
    }

    fn fallback() -> Result<Option<(Message, String)>> {
        let completed = completed(0);
        super::super::body::save_inbound_body(
            &CONVERSATION,
            &AUTHOR,
            &completed.target,
            300,
            None,
            completed.body,
        )
    }

    fn counts() -> (u32, u32) {
        core().db.messages().lock().query_row(
            "SELECT (SELECT count(*) FROM messages), (SELECT count(*) FROM message_media_preparations)",
            [], |row| Ok((row.get(0)?, row.get(1)?)),
        ).unwrap()
    }

    #[tokio::test]
    async fn every_preparation_completion_fallback_order_keeps_one_finished_message() {
        for order in [[0, 1, 2], [0, 2, 1], [1, 0, 2], [1, 2, 0], [2, 0, 1], [2, 1, 0]] {
            let _scope = ScopedCore::new();
            let mut id = None;
            for operation in order {
                let saved = match operation {
                    0 => prepare(&CONVERSATION, &AUTHOR, 100, pending(0)).unwrap(),
                    1 => complete(&CONVERSATION, &AUTHOR, 200, completed(0)).unwrap(),
                    _ => fallback().unwrap().map(|(message, _)| Stored { message, first: true }),
                };
                if let Some(saved) = saved {
                    if let Some(id) = &id {
                        assert_eq!(&saved.message.inner.id.to_string(), id)
                    }
                    id = Some(saved.message.inner.id.to_string());
                }
            }
            assert_eq!(counts(), (1, 0), "order {order:?}");
            let media = media::get(&CONVERSATION, &pending(0).target).unwrap().unwrap();
            assert_eq!(media.file_id, Some(vec![4; 32]));
            assert_eq!((media.width, media.height, media.duration_ms), (480, 854, 2_000));
            assert!(complete(&CONVERSATION, &AUTHOR, 400, completed(0)).unwrap().is_none());
            assert!(fallback().unwrap().is_none());
        }
    }

    #[tokio::test]
    async fn another_author_or_stale_attempt_cannot_complete_a_preparing_message() {
        let _scope = ScopedCore::new();
        let original = prepare(&CONVERSATION, &AUTHOR, 100, pending(0)).unwrap().unwrap();
        let retry = prepare(&CONVERSATION, &AUTHOR, 101, pending(1)).unwrap().unwrap();
        assert!(!retry.first);
        assert_eq!(original.message.inner.id.to_string(), retry.message.inner.id.to_string());
        assert!(prepare(&CONVERSATION, &AUTHOR, 102, pending(0)).unwrap().is_none());
        assert!(complete(&CONVERSATION, &[9; 32], 200, completed(1)).unwrap().is_none());
        assert!(complete(&CONVERSATION, &AUTHOR, 200, completed(0)).unwrap().is_none());
        let mut conflicting = completed(1);
        conflicting.attempt.id = [9; 16];
        assert!(complete(&CONVERSATION, &AUTHOR, 200, conflicting).is_err());
        assert_eq!(counts(), (1, 1));
        assert!(fallback().unwrap().is_none());
        let done = complete(&CONVERSATION, &AUTHOR, 200, completed(1)).unwrap().unwrap();
        assert!(!done.first);
        assert_eq!(done.message.inner.id.to_string(), original.message.inner.id.to_string());
        assert!(!done.message.inner.edited);
        assert_eq!(done.message.inner.timestamp, 100);
    }

    #[tokio::test]
    async fn delete_before_or_during_preparation_cannot_be_undone_by_completion() {
        for before in [false, true] {
            let _scope = ScopedCore::new();
            if !before {
                prepare(&CONVERSATION, &AUTHOR, 100, pending(0)).unwrap();
            }
            Message::receive_delete(&CONVERSATION, &pending(0).target, &AUTHOR).unwrap();
            assert!(prepare(&CONVERSATION, &AUTHOR, 100, pending(0)).unwrap().is_none());
            assert!(complete(&CONVERSATION, &AUTHOR, 200, completed(0)).unwrap().is_none());
            assert!(fallback().unwrap().is_none());
            assert!(media::get(&CONVERSATION, &pending(0).target).unwrap().is_none());
            assert_eq!(counts().1, 0);
        }
    }

    #[tokio::test]
    async fn preparation_and_completion_roll_back_together_on_disk_failure() {
        let scope = ScopedCore::new();
        let result = with_failing_trigger(
            scope.core.db.messages(),
            "INSERT ON message_media_preparations",
            || prepare(&CONVERSATION, &AUTHOR, 100, pending(0)),
        );
        assert!(result.is_err());
        assert_eq!(counts(), (0, 0));
        prepare(&CONVERSATION, &AUTHOR, 100, pending(0)).unwrap();
        let result =
            with_failing_trigger(scope.core.db.messages(), "INSERT ON message_media", || {
                complete(&CONVERSATION, &AUTHOR, 200, completed(0))
            });
        assert!(result.is_err());
        assert_eq!(counts(), (1, 1));
        assert!(media::get(&CONVERSATION, &pending(0).target).unwrap().unwrap().file_id.is_none());
        complete(&CONVERSATION, &AUTHOR, 200, completed(0)).unwrap();
        assert_eq!(counts(), (1, 0));
    }

    #[tokio::test]
    async fn completion_matches_the_same_preparation_after_database_reopening() {
        let scope = ScopedCore::new();
        let files = tempfile::tempdir().unwrap();
        let path = files.path().join("messages.db");
        let reopen = || {
            let mut db = Connection::open(&path).unwrap();
            crate::db::messages::migrate(&mut db);
            *scope.core.db.messages().lock() = db;
        };
        reopen();
        let saved = prepare(&CONVERSATION, &AUTHOR, 100, pending(0)).unwrap().unwrap();
        reopen();
        let done = complete(&CONVERSATION, &AUTHOR, 200, completed(0)).unwrap().unwrap();
        assert_eq!(done.message.inner.id.to_string(), saved.message.inner.id.to_string());
        assert!(!done.first);
        assert_eq!(counts(), (1, 0));
        let violations: u32 = scope
            .core
            .db
            .messages()
            .lock()
            .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| row.get(0))
            .unwrap();
        assert_eq!(violations, 0);
    }
}
