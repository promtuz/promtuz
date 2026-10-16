use anyhow::{Result, ensure};

use super::{MediaRequest, NormalizedCrop, image};

#[derive(Clone, Copy)]
pub struct AvatarCrop {
    pub center_x: f64,
    pub center_y: f64,
    pub zoom: f64,
}

impl AvatarCrop {
    fn normalize(self) -> Result<NormalizedCrop> {
        ensure!(
            self.center_x.is_finite()
                && self.center_y.is_finite()
                && self.zoom.is_finite()
                && (0.0..=1.0).contains(&self.center_x)
                && (0.0..=1.0).contains(&self.center_y)
                && self.zoom >= 1.0,
            "The picture crop is invalid"
        );
        NormalizedCrop {
            center_x: self.center_x as f32,
            center_y: self.center_y as f32,
            zoom: self.zoom as f32,
        }
        .validate()
        .map_err(anyhow::Error::msg)
    }
}

pub fn prepare_avatar_image(
    bytes: &[u8], crop: Option<AvatarCrop>,
) -> Result<image::PreparedImage> {
    let crop = crop.map(AvatarCrop::normalize).transpose()?.unwrap_or(NormalizedCrop {
        center_x: 0.5,
        center_y: 0.5,
        zoom: 1.0,
    });
    image::prepare_image(bytes, MediaRequest::Avatar { crop })
}

pub fn validate_avatar_avif(bytes: &[u8]) -> Result<image::PreparedImage> {
    ensure!(
        image::inspect(bytes)? == image::ImageKind::Avif,
        "Profile pictures must be prepared as AVIF before saving"
    );
    prepare_avatar_image(bytes, None)
}
