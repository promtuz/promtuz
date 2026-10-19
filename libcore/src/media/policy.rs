#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageProfile {
    pub max_edge: u32,
    pub max_bytes: u64,
    pub emergency_edge: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AnimationProfile {
    pub image: ImageProfile,
    pub max_fps: u32,
    pub max_frames: u32,
    pub max_duration_ms: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VideoProfile {
    pub max_width: u32,
    pub max_height: u32,
    pub max_fps: u32,
    pub video_bitrate: u32,
    pub max_bytes: u64,
    pub emergency_width: u32,
    pub emergency_height: u32,
    pub emergency_fps: u32,
    pub emergency_video_bitrate: u32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NormalizedCrop {
    pub center_x: f32,
    pub center_y: f32,
    pub zoom: f32,
}

impl NormalizedCrop {
    pub fn validate(self) -> Result<Self, &'static str> {
        if !self.center_x.is_finite()
            || !self.center_y.is_finite()
            || !self.zoom.is_finite()
            || !(0.0..=1.0).contains(&self.center_x)
            || !(0.0..=1.0).contains(&self.center_y)
            || self.zoom < 1.0
        {
            return Err("invalid crop");
        }
        Ok(self)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimeRange {
    pub start_ms: u64,
    pub end_ms: u64,
}

impl TimeRange {
    pub fn validate(self) -> Result<Self, &'static str> {
        if self.start_ms >= self.end_ms {
            return Err("invalid time range");
        }
        Ok(self)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MediaRequest {
    ChatImage { hd_enabled: bool },
    ChatVideo { trim: Option<TimeRange> },
    Avatar { crop: NormalizedCrop },
    Sticker { trim: Option<TimeRange> },
}

impl MediaRequest {
    pub fn validate(self) -> Result<Self, &'static str> {
        match self {
            Self::ChatVideo { trim: Some(trim) } | Self::Sticker { trim: Some(trim) } => {
                trim.validate()?;
            },
            Self::Avatar { crop } => {
                crop.validate()?;
            },
            _ => {},
        }
        Ok(self)
    }

    pub fn image_profile(self) -> Option<ImageProfile> {
        match self {
            Self::ChatImage { .. } => Some(CHAT_IMAGE),
            Self::Avatar { .. } => Some(AVATAR),
            Self::ChatVideo { .. } | Self::Sticker { .. } => None,
        }
    }

    pub fn animation_profile(self) -> Option<AnimationProfile> {
        match self {
            Self::ChatImage { .. } => Some(CHAT_ANIMATION),
            Self::Sticker { .. } => Some(STICKER),
            Self::ChatVideo { .. } | Self::Avatar { .. } => None,
        }
    }

    pub fn video_profile(self) -> Option<VideoProfile> {
        match self {
            Self::ChatVideo { .. } | Self::Sticker { .. } => Some(VIDEO),
            Self::ChatImage { .. } | Self::Avatar { .. } => None,
        }
    }

    pub fn hd_profile(self) -> Option<ImageProfile> {
        match self {
            Self::ChatImage { hd_enabled: true } if HD_IMAGE_ENABLED => Some(HD_IMAGE),
            _ => None,
        }
    }

    pub fn poster_profile(self) -> Option<ImageProfile> {
        self.video_profile().map(|_| POSTER)
    }

    pub fn resource_limits() -> ResourceLimits {
        ResourceLimits {
            source_bytes: MAX_SOURCE_BYTES,
            source_pixels: MAX_SOURCE_PIXELS,
            animation_frames: MAX_ANIMATION_FRAMES,
            animation_duration_ms: MAX_ANIMATION_DURATION_MS,
            job_bytes: MAX_JOB_BYTES,
            job_memory: MAX_JOB_MEMORY,
            frame_store_memory: FRAMESTORE_MEMORY,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResourceLimits {
    pub source_bytes: u64,
    pub source_pixels: u64,
    pub animation_frames: u32,
    pub animation_duration_ms: u64,
    pub job_bytes: u64,
    pub job_memory: u64,
    pub frame_store_memory: u64,
}

pub const AVATAR: ImageProfile =
    ImageProfile { max_edge: 256, max_bytes: 64 * 1024, emergency_edge: 96 };

pub const CHAT_IMAGE: ImageProfile =
    ImageProfile { max_edge: 1_600, max_bytes: 256 * 1024, emergency_edge: 320 };

pub const HD_IMAGE: ImageProfile =
    ImageProfile { max_edge: 2_560, max_bytes: 2 * 1024 * 1024, emergency_edge: 640 };

/// Enable only with a deliberate delivery and UI rollout; the wire/storage shape is already live.
pub const HD_IMAGE_ENABLED: bool = false;

pub const CHAT_ANIMATION: AnimationProfile = AnimationProfile {
    image: ImageProfile { max_edge: 512, max_bytes: 256 * 1024, emergency_edge: 128 },
    max_fps: 15,
    max_frames: 150,
    max_duration_ms: Some(10_000),
};

pub const STICKER: AnimationProfile = AnimationProfile {
    image: ImageProfile { max_edge: 512, max_bytes: 256 * 1024, emergency_edge: 128 },
    max_fps: 12,
    max_frames: u32::MAX,
    max_duration_ms: None,
};

pub const VIDEO: VideoProfile = VideoProfile {
    max_width: 1_280,
    max_height: 720,
    max_fps: 30,
    video_bitrate: 2_000_000,
    max_bytes: 64 * 1024 * 1024,
    emergency_width: 640,
    emergency_height: 360,
    emergency_fps: 15,
    emergency_video_bitrate: 400_000,
};

pub const POSTER: ImageProfile =
    ImageProfile { max_edge: 320, max_bytes: 32 * 1024, emergency_edge: 160 };

pub const MAX_SOURCE_BYTES: u64 = 100 * 1024 * 1024;
pub const MAX_SOURCE_PIXELS: u64 = 32_000_000;
pub const MAX_ANIMATION_FRAMES: u32 = 600;
pub const MAX_ANIMATION_DURATION_MS: u64 = 5 * 60 * 1_000;
pub const MAX_JOB_BYTES: u64 = 512 * 1024 * 1024;
pub const MAX_JOB_MEMORY: u64 = 192 * 1024 * 1024;
pub const FRAMESTORE_MEMORY: u64 = 32 * 1024 * 1024;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_is_the_product_contract() {
        assert_eq!(
            AVATAR,
            ImageProfile { max_edge: 256, max_bytes: 64 * 1024, emergency_edge: 96 }
        );
        assert_eq!(
            CHAT_IMAGE,
            ImageProfile { max_edge: 1_600, max_bytes: 256 * 1024, emergency_edge: 320 }
        );
        assert_eq!(CHAT_ANIMATION.max_fps, 15);
        assert_eq!(STICKER.max_fps, 12);
        assert_eq!(VIDEO.max_bytes, 64 * 1024 * 1024);
        assert!(!HD_IMAGE_ENABLED);
        assert_eq!(MediaRequest::ChatImage { hd_enabled: true }.image_profile(), Some(CHAT_IMAGE));
        assert!(MediaRequest::ChatImage { hd_enabled: true }.hd_profile().is_none());
        assert_eq!(MediaRequest::Sticker { trim: None }.animation_profile(), Some(STICKER));
        assert_eq!(MediaRequest::ChatVideo { trim: None }.poster_profile(), Some(POSTER));
        assert_eq!(MediaRequest::resource_limits().frame_store_memory, 32 * 1024 * 1024);
    }

    #[test]
    fn request_rejects_impossible_editor_values() {
        assert!(
            MediaRequest::Avatar {
                crop: NormalizedCrop { center_x: 1.1, center_y: 0.5, zoom: 1.0 }
            }
            .validate()
            .is_err()
        );
        assert!(
            MediaRequest::ChatVideo { trim: Some(TimeRange { start_ms: 4_000, end_ms: 4_000 }) }
                .validate()
                .is_err()
        );
        assert!(
            MediaRequest::Sticker { trim: Some(TimeRange { start_ms: 0, end_ms: 1 }) }
                .validate()
                .is_ok()
        );
    }
}
