use std::io;
use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::net::Ipv6Addr;
use std::net::SocketAddr;

use crc32fast::Hasher;

const MAGIC_BYTES: &[u8] = b"PIDQ";

pub struct IdentityQr {
    pub ipk: [u8; 32],
    // pub vfk: [u8; 32],
    // pub epk: [u8; 32],
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
        // out.extend_from_slice(&self.vfk);
        // out.extend_from_slice(&self.epk);

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
        if bytes.len() < 4 + 1 + 1 + 32 * 3 + 1 + 2 + 1 + 4 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "too short"));
        }

        // verify CRC
        let (body, crc_bytes) = bytes.split_at(bytes.len() - 4);
        let mut hasher = Hasher::new();
        hasher.update(body);
        if hasher.finalize().to_be_bytes() != crc_bytes {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "crc mismatch"));
        }

        let mut i = 0;

        // magic
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

        let take32 = |buf: &[u8], i: &mut usize| {
            let mut out = [0u8; 32];
            out.copy_from_slice(&buf[*i..*i + 32]);
            *i += 32;
            out
        };

        let ipk = take32(body, &mut i);
        // let vfk = take32(body, &mut i);
        // let epk = take32(body, &mut i);

        let ip = match body[i] {
            4 => {
                i += 1;
                let ip = Ipv4Addr::new(body[i], body[i + 1], body[i + 2], body[i + 3]);
                i += 4;
                IpAddr::V4(ip)
            },
            6 => {
                i += 1;
                let mut oct = [0u8; 16];
                oct.copy_from_slice(&body[i..i + 16]);
                i += 16;
                IpAddr::V6(Ipv6Addr::from(oct))
            },
            _ => return Err(io::Error::new(io::ErrorKind::InvalidData, "bad ip version")),
        };

        let port = u16::from_be_bytes([body[i], body[i + 1]]);
        i += 2;

        let addr = SocketAddr::new(ip, port);

        let name_len = body[i] as usize;
        i += 1;

        let name = std::str::from_utf8(&body[i..i + name_len])
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad utf8"))?
            .to_owned();

        Ok(Self { ipk, addr, name })
    }
}
