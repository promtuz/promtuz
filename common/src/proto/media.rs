//! Authenticated media preparation updates. The enclosing MLS leaf owns the message.

use serde::{Deserialize, Serialize};

use super::mls_wire::Body;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaAttempt {
    pub generation: u32,
    pub id: [u8; 16],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaPresentation {
    pub width: u32,
    pub height: u32,
    pub duration_ms: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreparingMedia {
    pub target: [u8; 16],
    pub attempt: MediaAttempt,
    pub reply_to: Option<[u8; 16]>,
    pub caption: String,
    pub group_id: Option<[u8; 16]>,
    pub mime: String,
    pub name: String,
    pub presentation: MediaPresentation,
    pub thumb: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompletedMedia {
    pub target: [u8; 16],
    pub attempt: MediaAttempt,
    pub reply_to: Option<[u8; 16]>,
    pub presentation: MediaPresentation,
    pub body: Body,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum MediaUpdate {
    Preparing(PreparingMedia),
    Complete(CompletedMedia),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{
        mls_wire::AppPayload,
        pack::{Packer, Unpacker},
    };

    // COMPAT(v12): media-completion. Pin the bytes older clients must still decode.
    #[test]
    fn completed_attachment_keeps_its_v12_wire_format() {
        let post = AppPayload::Post {
            reply_to: None,
            body: Body::Attachment {
                caption: String::new(),
                group_id: None,
                mime: "video/mp4".into(),
                name: "video.mp4".into(),
                size: 1,
                thumb: vec![],
                file_id: [7; 32],
            },
        };
        let bytes =
            [b"\x06\x00\x02\x00\x00\x09video/mp4\x09video.mp4\x01\x00".as_slice(), &[7; 32]]
                .concat();
        assert_eq!(post.ser().unwrap(), bytes);
        assert_eq!(AppPayload::deser(&bytes).unwrap(), post);
    }
}
