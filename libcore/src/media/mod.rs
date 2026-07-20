//! Still-image pipeline: RGBA (from the platform decoder) → AVIF, plus a
//! gaussian-blurred thumbnail. libcore owns encode/blur so it's one impl for
//! every platform; the platform owns decode (HEIC/HDR/EXIF) + video/PDF.
//!
//! Real bodies (`compress_image` / `blur_thumb`) land in Tasks 4-5.
