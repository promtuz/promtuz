use std::{net::SocketAddr, path::PathBuf};
use serde::Deserialize;
use url::Url;

use crate::quic::id::NodeId;

/// Network section of `config.toml` for both relay & resolver
#[derive(Deserialize, Debug)]
pub struct NetworkConfig {
    /// address on which quic endpoint will start
    pub address: SocketAddr,
    pub cert_path: PathBuf,
    pub key_path: PathBuf,
    /// root ca to verify outgoing/incoming quic connections
    pub root_ca_path: PathBuf,
}

#[derive(Deserialize, Debug)]
pub struct ResolverSeed {
    pub id: NodeId,
    pub url: Url,
}

#[derive(Deserialize, Debug)]
pub struct ResolverConfig {
    pub seed: Vec<ResolverSeed>,
}


// pub fn print_config_err(err: &toml::de::Error, source: &str) -> String {
//     if let Some(core::ops::Range { start, end }) = err.span() {
//         let line_idx = line.saturating_sub(1);
//         let src_line = source.lines().nth(line_idx).unwrap_or("");

//         format!(
//             "TOML Parse Error:\n  → Line {}, Column {}\n    {}\n    {}^\n  {}\n",
//             line,
//             col,
//             src_line,
//             " ".repeat(col.saturating_sub(1)),
//             err
//         )
//     } else {
//         format!("TOML Parse Error: {}", err)
//     }
// }