use ed25519_dalek::Signature;
use ed25519_dalek::VerifyingKey;

const UPDATE_MANIFEST_PUBLIC_KEY: [u8; 32] = [
    0x29, 0x87, 0x89, 0xd7, 0x6c, 0xfe, 0x24, 0xcf, 0x88, 0xd6, 0xa1, 0x92, 0x1a, 0x32, 0xb9, 0xaa,
    0x00, 0xf2, 0x57, 0x5d, 0xb2, 0x9f, 0x21, 0x62, 0x14, 0x8d, 0xf1, 0xfc, 0x77, 0xeb, 0x07, 0xbb,
];

/// Verify detached Ed25519 signature over unchanged update manifest bytes.
#[uniffi::export]
pub fn verify_update_manifest(manifest: Vec<u8>, signature: Vec<u8>) -> bool {
    let Ok(signature) = Signature::from_slice(&signature) else {
        return false;
    };
    let Ok(public_key) = VerifyingKey::from_bytes(&UPDATE_MANIFEST_PUBLIC_KEY) else {
        return false;
    };

    public_key.verify_strict(&manifest, &signature).is_ok()
}
