use std::sync::atomic::AtomicBool;

/// Set from `[log] dht` at startup; gates the bootstrap/routing chatter.
pub static DHT_LOG: AtomicBool = AtomicBool::new(false);

#[macro_export]
macro_rules! dht_log {
    ($($arg:tt)*) => {{
        if $crate::util::dht_log::DHT_LOG.load(std::sync::atomic::Ordering::Relaxed) {
            common::info!($($arg)*);
        }
    }};
}
