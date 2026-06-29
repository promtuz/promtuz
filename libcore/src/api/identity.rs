//! Identity exports: enrollment (and, later, QR invite pairing).

use crate::data::identity::Identity;
use crate::platform::CoreError;

/// Enroll — create the long-term identity. The client calls this from the
/// enrollment screen (shown when `should_launch_app()` is false).
#[uniffi::export]
pub fn enroll(name: String) -> Result<(), CoreError> {
    Identity::create(&name)?;
    Ok(())
}
