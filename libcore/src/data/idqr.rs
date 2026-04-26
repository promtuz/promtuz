use std::io;
use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::net::Ipv6Addr;
use std::net::SocketAddr;

use crc32fast::Hasher;

const MAGIC_BYTES: &[u8] = b"PIDQ";

#[derive(Debug)]
pub struct IdentityQr {
    pub ipk: [u8; 32],
    pub addr: SocketAddr,
    pub name: String,
}

impl IdentityQr {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(96);

        // magic + version
        out.extend_from_slice(MAGIC_BYTES);
        out.push(1); // version
        out.push(0); // flags

        out.extend_from_slice(&self.ipk);

        match self.addr.ip() {
            std::net::IpAddr::V4(ip) => {
                out.push(4);
                out.extend_from_slice(&ip.octets());
            },
            std::net::IpAddr::V6(ip) => {
                out.push(6);
                out.extend_from_slice(&ip.octets());
            },
        }

        out.extend_from_slice(&self.addr.port().to_be_bytes());

        let name = self.name.as_bytes();
        assert!(name.len() <= 255);
        out.push(name.len() as u8);
        out.extend_from_slice(name);

        // checksum
        let mut hasher = Hasher::new();
        hasher.update(&out);
        out.extend_from_slice(&hasher.finalize().to_be_bytes());

        out
    }

    pub fn decode(bytes: &[u8]) -> io::Result<Self> {
        // CRC tag is the last 4 bytes; everything before must include at least
        // magic(4) + version(1) + flags(1) + ipk(32) + ip_ver(1) + ip(>=4) +
        // port(2) + name_len(1).
        const MIN: usize = 4 + 4 + 1 + 1 + 32 + 1 + 4 + 2 + 1;
        if bytes.len() < MIN {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "too short"));
        }

        let (body, crc_bytes) = bytes.split_at(bytes.len() - 4);
        let mut hasher = Hasher::new();
        hasher.update(body);
        if hasher.finalize().to_be_bytes() != crc_bytes {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "crc mismatch"));
        }

        let need = |i: usize, n: usize| -> io::Result<()> {
            if i.checked_add(n).is_none_or(|end| end > body.len()) {
                Err(io::Error::new(io::ErrorKind::InvalidData, "truncated"))
            } else {
                Ok(())
            }
        };

        let mut i = 0;

        if &body[i..i + 4] != MAGIC_BYTES {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "bad magic"));
        }
        i += 4;

        let version = body[i];
        i += 1;
        if version != 1 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "unsupported version"));
        }

        let _flags = body[i];
        i += 1;

        need(i, 32)?;
        let mut ipk = [0u8; 32];
        ipk.copy_from_slice(&body[i..i + 32]);
        i += 32;

        need(i, 1)?;
        let ip = match body[i] {
            4 => {
                i += 1;
                need(i, 4)?;
                let ip = Ipv4Addr::new(body[i], body[i + 1], body[i + 2], body[i + 3]);
                i += 4;
                IpAddr::V4(ip)
            },
            6 => {
                i += 1;
                need(i, 16)?;
                let mut oct = [0u8; 16];
                oct.copy_from_slice(&body[i..i + 16]);
                i += 16;
                IpAddr::V6(Ipv6Addr::from(oct))
            },
            _ => return Err(io::Error::new(io::ErrorKind::InvalidData, "bad ip version")),
        };

        need(i, 2)?;
        let port = u16::from_be_bytes([body[i], body[i + 1]]);
        i += 2;

        let addr = SocketAddr::new(ip, port);

        need(i, 1)?;
        let name_len = body[i] as usize;
        i += 1;

        need(i, name_len)?;
        let name = std::str::from_utf8(&body[i..i + name_len])
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad utf8"))?
            .to_owned();

        Ok(Self { ipk, addr, name })
    }
}
