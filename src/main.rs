use crate::util::config::AppConfig;

mod util;
mod quic;

#[tokio::main]
async fn main() {
    let config = AppConfig::load();

    
}
