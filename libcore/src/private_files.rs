//! Code-bound chunk storage. File keys are wrapped exclusively for the private chat; the
//! identity/storage key never unwraps them. Every operation revalidates its session generation.
use std::{collections::HashMap, fs::File, io::{self, Read, Seek, SeekFrom, Write}, sync::{OnceLock, atomic::Ordering}};
use anyhow::{Result, anyhow, ensure};
use chacha20poly1305::{XChaCha20Poly1305, XNonce, aead::{Aead, KeyInit, Payload}};
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension};
use zeroize::Zeroizing;
use crate::state::core;

const BLOCK: usize = 64 * 1024;
const SLOT: u64 = (24 + BLOCK + 16) as u64;
const MAGIC: &[u8; 8] = b"PZFILE01";

fn random<const N: usize>() -> [u8; N] {
    use ed25519_dalek::ed25519::signature::rand_core::{OsRng, RngCore};
    let mut bytes = [0; N]; OsRng.fill_bytes(&mut bytes); bytes
}
fn io_error(e: impl std::fmt::Display) -> io::Error { io::Error::other(e.to_string()) }

#[derive(serde::Serialize, serde::Deserialize)]
struct Details {
    key: [u8; 32],
    size: u64,
    manifest: Vec<u8>,
    expires_at: u64,
}
impl Drop for Details { fn drop(&mut self) { use zeroize::Zeroize; self.key.zeroize(); } }

struct Temporary { key: Zeroizing<[u8; 32]>, size: u64 }
fn temporary() -> &'static Mutex<HashMap<String, Temporary>> {
    static KEYS: OnceLock<Mutex<HashMap<String, Temporary>>> = OnceLock::new();
    KEYS.get_or_init(Default::default)
}

enum Access {
    Plain,
    Temporary(Zeroizing<[u8; 32]>),
    Private { conversation: [u8; 16], generation: u64 },
}
/// The transfer engine reads the logical bytes; ciphertext never changes its wire manifest.
pub(crate) struct StorageFile { file: File, path: String, access: Access, position: u64, size: u64 }
impl StorageFile {
    pub(crate) fn plain(file: File) -> io::Result<Self> {
        let size = file.metadata()?.len();
        Ok(Self { file, path: String::new(), access: Access::Plain, position: 0, size })
    }
    pub(crate) fn open(path: &str, write: bool) -> Result<Self> {
        if let Some(temp) = temporary().lock().get(path) {
            return Ok(Self { file: File::options().read(true).write(write).open(path)?, path: path.into(),
                access: Access::Temporary(Zeroizing::new(*temp.key)), position: 0, size: temp.size });
        }
        let mut opened = File::options().read(true).write(write).create(write).truncate(false).open(path)?;
        let mut header=[0;8];let n=opened.read(&mut header)?;opened.seek(SeekFrom::Start(0))?;
        if n!=8 || &header!=MAGIC {return Ok(Self::plain(opened)?)}
        let row: Option<([u8; 16], Vec<u8>)> = core().db.messages().lock().query_row(
            "SELECT conversation_id,details FROM private_files WHERE path=?1", [path],
            |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
        if let Some((conversation, bytes)) = row {
            let generation = core().private_generation.load(Ordering::SeqCst);
            let plain = Zeroizing::new(crate::privacy::open_bytes(&conversation, &bytes)?);
            let details: Details = postcard::from_bytes(&plain)?;
            ensure!(generation == core().private_generation.load(Ordering::SeqCst), "Chat is locked");
            return Ok(Self { file: File::options().read(true).write(write).open(path)?, path: path.into(),
                access: Access::Private { conversation, generation }, position: 0, size: details.size });
        }
        Err(anyhow!("Encrypted file is unavailable"))
    }
    fn key(&self) -> Result<Option<Zeroizing<[u8; 32]>>> {
        match &self.access {
            Access::Plain => Ok(None),
            Access::Temporary(key) => Ok(Some(Zeroizing::new(**key))),
            Access::Private { conversation, generation } => {
                ensure!(*generation == core().private_generation.load(Ordering::SeqCst), "Chat is locked");
                let bytes: Vec<u8> = core().db.messages().lock().query_row(
                    "SELECT details FROM private_files WHERE conversation_id=?1 AND path=?2",
                    (conversation.as_slice(), &self.path), |r| r.get(0))?;
                let plain = Zeroizing::new(crate::privacy::open_bytes(conversation, &bytes)?);
                let details: Details = postcard::from_bytes(&plain)?;
                Ok(Some(Zeroizing::new(details.key)))
            },
        }
    }
    fn check(&self) -> Result<()> {
        if let Access::Private { conversation, generation } = &self.access {
            ensure!(*generation == core().private_generation.load(Ordering::SeqCst)
                && core().private_sessions.read().contains_key(conversation), "Chat is locked");
        }
        Ok(())
    }
    fn block(&mut self, index: u64, key: &[u8; 32]) -> io::Result<Zeroizing<Vec<u8>>> {
        self.file.seek(SeekFrom::Start(8 + index.checked_mul(SLOT).ok_or_else(|| io_error("file offset overflow"))?))?;
        let mut nonce = [0; 24]; self.file.read_exact(&mut nonce)?;
        let mut cipher = vec![0; BLOCK + 16]; self.file.read_exact(&mut cipher)?;
        // A sparse unwritten slot contains only zeros. It is missing data, not an authenticated block.
        if nonce.iter().all(|b| *b == 0) && cipher.iter().all(|b| *b == 0) {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "encrypted block is not written"));
        }
        let plain = XChaCha20Poly1305::new(key.into()).decrypt(XNonce::from_slice(&nonce),
            Payload { msg: &cipher, aad: &index.to_le_bytes() }).map_err(|_| io_error("file authentication failed"))?;
        Ok(Zeroizing::new(plain))
    }
    fn write_block(&mut self, index: u64, key: &[u8; 32], bytes: &[u8]) -> io::Result<()> {
        let nonce = random::<24>();
        let cipher = XChaCha20Poly1305::new(key.into()).encrypt(XNonce::from_slice(&nonce),
            Payload { msg: bytes, aad: &index.to_le_bytes() }).map_err(io_error)?;
        self.file.seek(SeekFrom::Start(8 + index.checked_mul(SLOT).ok_or_else(|| io_error("file offset overflow"))?))?;
        self.file.write_all(&nonce)?; self.file.write_all(&cipher)
    }
    pub(crate) fn sync_data(&self) -> io::Result<()> { self.check().map_err(io_error)?; self.file.sync_data() }
    pub(crate) fn set_len(&mut self, size: u64) -> io::Result<()> {
        self.check().map_err(io_error)?;
        match self.access { Access::Plain => self.file.set_len(size)?, _ => {
            ensure_io(size == self.size, "Encrypted file size changed")?;
            self.file.set_len(8 + size.div_ceil(BLOCK as u64) * SLOT)?;
        }}
        self.size = size; Ok(())
    }
    pub(crate) fn len(&self) -> u64 { self.size }
}
fn ensure_io(condition: bool, message: &str) -> io::Result<()> { if condition { Ok(()) } else { Err(io_error(message)) } }
impl Read for StorageFile {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let key = self.key().map_err(io_error)?;
        let Some(key) = key else { self.file.seek(SeekFrom::Start(self.position))?; let n = self.file.read(output)?; self.position += n as u64; return Ok(n) };
        if output.is_empty() || self.position >= self.size { return Ok(0) }
        let mut count = 0;
        let wanted = output.len().min((self.size - self.position).min(usize::MAX as u64) as usize);
        while count < wanted {
            let index = self.position / BLOCK as u64; let offset = self.position as usize % BLOCK;
            let bytes = self.block(index, &key)?; let n = (BLOCK - offset).min(wanted - count);
            output[count..count+n].copy_from_slice(&bytes[offset..offset+n]);
            count += n; self.position += n as u64;
        }
        if let Err(error) = self.check() { use zeroize::Zeroize; output[..count].zeroize(); return Err(io_error(error)); }
        Ok(count)
    }
}
impl Write for StorageFile {
    fn write(&mut self, input: &[u8]) -> io::Result<usize> {
        let key = self.key().map_err(io_error)?;
        let Some(key) = key else { self.file.seek(SeekFrom::Start(self.position))?; let n=self.file.write(input)?; self.position+=n as u64; self.size=self.size.max(self.position); return Ok(n) };
        ensure_io(self.position.checked_add(input.len() as u64).is_some_and(|end| end <= self.size), "Write exceeds encrypted file size")?;
        let mut count = 0;
        while count < input.len() {
            let index = self.position / BLOCK as u64; let offset = self.position as usize % BLOCK;
            let n = (BLOCK - offset).min(input.len() - count);
            let mut bytes = if offset == 0 && n == BLOCK { Zeroizing::new(vec![0; BLOCK]) }
                else { match self.block(index, &key) { Ok(bytes) => bytes, Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => Zeroizing::new(vec![0; BLOCK]), Err(error) => return Err(error) }};
            bytes[offset..offset+n].copy_from_slice(&input[count..count+n]);
            self.check().map_err(io_error)?; self.write_block(index, &key, &bytes)?;
            count += n; self.position += n as u64;
        }
        self.check().map_err(io_error)?; Ok(count)
    }
    fn flush(&mut self) -> io::Result<()> { self.check().map_err(io_error)?; self.file.flush() }
}
impl Seek for StorageFile {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.check().map_err(io_error)?;
        let next = match position { SeekFrom::Start(n) => i128::from(n), SeekFrom::Current(n) => i128::from(self.position)+i128::from(n), SeekFrom::End(n) => i128::from(self.size)+i128::from(n) };
        self.position = u64::try_from(next).map_err(io_error)?; Ok(self.position)
    }
}

fn create(path: &str, key: &[u8; 32], size: u64) -> Result<StorageFile> {
    let mut file = File::options().read(true).write(true).create_new(true).open(path)?;
    file.write_all(MAGIC)?;
    Ok(StorageFile { file, path: path.into(), access: Access::Temporary(Zeroizing::new(*key)), position: 0, size })
}

/// The unsent copy has no persistent key, even when its destination chat is chosen later.
pub(crate) fn stage_reader(reader: &mut impl Read, size: u64) -> Result<(String, crate::transfer::wire::Manifest)> {
    stage_reader_in(reader,size,std::path::Path::new(&core().db.files_dir("private-files")))
}

pub(crate) fn stage_reader_in(reader:&mut impl Read,size:u64,directory:&std::path::Path)->Result<(String,crate::transfer::wire::Manifest)> {
    std::fs::create_dir_all(directory)?;
    let path=directory.join(format!("{}.stage",hex::encode(random::<24>()))).to_string_lossy().into_owned();
    let key = Zeroizing::new(random::<32>()); let mut file = create(&path, &key, size)?;
    let result = (|| {
        let mut hashes=Vec::new(); let mut remaining=size; let mut bytes=Zeroizing::new(vec![0; crate::transfer::wire::CHUNK_SIZE]);
        while remaining > 0 { let n=(remaining.min(bytes.len() as u64)) as usize; reader.read_exact(&mut bytes[..n])?; file.write_all(&bytes[..n])?; hashes.push(*blake3::hash(&bytes[..n]).as_bytes()); remaining-=n as u64; }
        let mut extra=[0]; ensure!(reader.read(&mut extra)? == 0, "Media source size changed");
        file.sync_data()?;
        Ok(crate::transfer::wire::Manifest { total_size:size, chunk_size:crate::transfer::wire::CHUNK_SIZE as u32, chunks:hashes })
    })();
    match result { Ok(manifest) => { temporary().lock().insert(path.clone(), Temporary{key:Zeroizing::new(*key),size}); Ok((path,manifest)) }, Err(error) => { let _=std::fs::remove_file(path); Err(error) }}
}

pub(crate) fn protect_tx(conn: &Connection, conversation: &[u8;16], file: &[u8;32]) -> Result<()> {
    if conn.query_row("SELECT EXISTS(SELECT 1 FROM private_files WHERE conversation_id=?1 AND file_id=?2)",
        (conversation.as_slice(), file.as_slice()), |r| r.get::<_,bool>(0))? { return Ok(()) }
    let retained = crate::transfer::store::retention_get(file);
    // A completed private copy may be the only remaining authoritative local source. Select
    // an unlocked owner explicitly; never treat its ciphertext as ordinary source bytes.
    let retained = if retained.is_some() { retained } else {
        let owners: Vec<[u8;16]> = crate::db::all(conn,
            "SELECT conversation_id FROM private_files WHERE file_id=?1 AND complete=1", [file], |r| r.get(0))?;
        let mut found = None;
        for owner in owners {
            if core().private_sessions.read().contains_key(&owner) {
                found = retention_tx(conn,&owner,file)?;
                if found.is_some() { break; }
            }
        }
        found
    };
    let partial = crate::transfer::store::partial_get(file);
    let source = retained.as_ref().map(|r|r.path.as_str()).or_else(||partial.as_ref().map(|p|p.path.as_str()));
    let Some(source) = source else { return Ok(()) }; // A remote-only offer has no local bytes yet.
    if !std::path::Path::new(source).exists() { return Ok(()) }
    let (size,manifest,expires)=match retained { Some(ref r)=>(r.size,r.manifest.clone(),r.expires_at), None=> {let p=partial.as_ref().unwrap();(p.total,p.manifest.clone().unwrap_or_default(),u64::MAX)} };
    let protected_source:Option<([u8;16],Vec<u8>)>=conn.query_row("SELECT conversation_id,details FROM private_files WHERE path=?1",[source],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    let source_key=if let Some((owner,sealed))=protected_source {
        let Ok(plain)=crate::privacy::open_bytes(&owner,&sealed) else{return Ok(())};
        let plain = Zeroizing::new(plain);
        let details:Details=postcard::from_bytes(&plain)?;Some(Zeroizing::new(details.key))
    }else{None};
    let path=format!("{}/{}.private",core().db.files_dir("private-files"),hex::encode(random::<24>()));
    let key=Zeroizing::new(random::<32>());
    let result=(|| {
        // The old file remains immutable until its new encrypted copy and key record are durable.
        let mut source = if let Some(source_key)=source_key {
            StorageFile {file:File::open(source)?,path:source.into(),access:Access::Temporary(source_key),position:0,size}
        }else if let Some(temp)=temporary().lock().get(source) {
            StorageFile { file:File::open(source)?,path:source.into(),access:Access::Temporary(Zeroizing::new(*temp.key)),position:0,size:temp.size }
        } else {
            // Existing private copies are never opened here: they are selected explicitly by chat.
            StorageFile::plain(File::open(source)?)?
        };
        let mut output=create(&path,&key,size)?;
        let expected=postcard::from_bytes::<crate::transfer::wire::Manifest>(&manifest).ok();
        let incomplete=retained.is_none() && partial.as_ref().is_some_and(|p|!p.is_complete());
        if let Some(expected) = &expected { ensure!(expected.file_id() == *file && expected.total_size == size
            && expected.chunk_size > 0 && expected.chunk_size as usize <= crate::transfer::wire::CHUNK_SIZE,
            "Invalid stored media manifest"); }
        let chunk_size = expected.as_ref().map(|m| m.chunk_size as usize).unwrap_or(crate::transfer::wire::CHUNK_SIZE);
        let mut bytes=Zeroizing::new(vec![0;chunk_size]); let mut offset=0;
        while offset<size {
            let n=(size-offset).min(bytes.len() as u64) as usize;
            let read=source.read_exact(&mut bytes[..n]);
            // Incomplete incoming files have holes; only verified ranges are reused after reopening.
            if let Err(error)=read { if error.kind()==io::ErrorKind::UnexpectedEof && partial.as_ref().is_some_and(|p|!p.is_complete()) { break } else { return Err(error.into()) } }
            if !incomplete && let Some(expected)=&expected {
                ensure!(expected.chunks.get((offset/chunk_size as u64) as usize)==Some(blake3::hash(&bytes[..n]).as_bytes()),"Media changed during private conversion");
            }
            output.write_all(&bytes[..n])?;offset+=n as u64;
        }
        output.sync_data()?;
        let details=Details{key:*key,size,manifest,expires_at:expires};
        let plain = Zeroizing::new(postcard::to_allocvec(&details)?);
        let sealed=crate::privacy::seal_bytes_tx(conn,conversation,&plain)?;
        conn.execute("INSERT INTO private_files(conversation_id,file_id,path,details,complete) VALUES(?1,?2,?3,?4,?5)",
            (conversation.as_slice(),file.as_slice(),&path,sealed,!incomplete))?;
        Ok(())
    })();
    if result.is_err() { let _=std::fs::remove_file(&path); } result
}

pub(crate) fn retention_tx(conn:&Connection,conversation:&[u8;16],file:&[u8;32])->Result<Option<crate::transfer::store::Retention>> {
    let row:Option<(String,Vec<u8>)>=conn.query_row("SELECT path,details FROM private_files WHERE conversation_id=?1 AND file_id=?2",(conversation.as_slice(),file.as_slice()),|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    let Some((path,sealed))=row else{return Ok(None)};
    let plain = Zeroizing::new(crate::privacy::open_bytes(conversation,&sealed)?);
    let details:Details=postcard::from_bytes(&plain)?;
    let chunk_size=postcard::from_bytes::<crate::transfer::wire::Manifest>(&details.manifest).map(|m|m.chunk_size).unwrap_or(crate::transfer::wire::CHUNK_SIZE as u32);
    Ok(Some(crate::transfer::store::Retention{path,size:details.size,chunk_size,manifest:details.manifest.clone(),expires_at:details.expires_at}))
}

pub(crate) fn clear_conversation_tx(conn:&Connection,conversation:&[u8;16])->Result<Vec<String>> {
    let paths=crate::db::all(conn,"SELECT path FROM private_files WHERE conversation_id=?1",[conversation],|r|r.get(0))?;
    conn.execute("DELETE FROM private_files WHERE conversation_id=?1",[conversation])?;
    conn.execute("DELETE FROM private_file_refs WHERE conversation_id=?1",[conversation])?;Ok(paths)
}

/// Called after publication and on startup to finish an interrupted conversion. Ordinary shared
/// ownership is authoritative; private references never keep an ordinary plaintext copy alive.
pub(crate) fn cleanup_plaintext_tx(conn:&Connection)->Result<()> {
    let files:Vec<([u8;16],[u8;32],String)>=crate::db::all(conn,"SELECT conversation_id,file_id,path FROM private_files",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
    for (conv,file,path) in files {
        let owned:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM private_file_refs WHERE conversation_id=?1 AND file_id=?2)",(conv.as_slice(),file.as_slice()),|r|r.get(0))?;
        if !owned {
            conn.execute("DELETE FROM private_files WHERE conversation_id=?1 AND file_id=?2",(conv.as_slice(),file.as_slice()))?;
            let _=std::fs::remove_file(&path);continue
        }
        if crate::staging::holds(core(),&file) {continue}
        let ordinary:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM message_media WHERE file_id=?1) OR EXISTS(SELECT 1 FROM message_media_renditions WHERE file_id=?1)",[file],|r|r.get(0))?;
        if !ordinary {
            let transfers=core().db.transfers().lock();
            // Stop old writers before retiring their ordinary path. Rechecking the bitmap after
            // reopening keeps every verified range, including interrupted downloads.
            crate::transfer::store::cancel_file(core(),&file);
            if let Some(partial)=crate::transfer::store::partial_get_tx(&transfers,&file) {
                if partial.path != path {
                    transfers.execute("UPDATE partials SET path=?2 WHERE file_id=?1",(file.as_slice(),&path))?;
                    if !conn.query_row("SELECT EXISTS(SELECT 1 FROM private_files WHERE path=?1)",[&partial.path],|r|r.get::<_,bool>(0))? {let _=std::fs::remove_file(partial.path);}
                }
            }
            if let Some(ret)=crate::transfer::store::retention_get_tx(&transfers,&file) {
                transfers.execute("DELETE FROM retention WHERE file_id=?1",[file])?;
                if !conn.query_row("SELECT EXISTS(SELECT 1 FROM private_files WHERE path=?1)",[&ret.path],|r|r.get::<_,bool>(0))? {temporary().lock().remove(&ret.path);let _=std::fs::remove_file(ret.path);}
            }
        }
    }
    Ok(())
}

/// Copies a chat's private files to ordinary storage before its lock is removed, and returns the
/// encrypted paths to delete once the removal commits. Until then startup cleanup drops the copies.
pub(crate) fn publish_conversation(conversation:&[u8;16])->Result<Vec<String>> {
    let rows:Vec<([u8;32],String,Vec<u8>)>=crate::db::all(&core().db.messages().lock(),
        "SELECT file_id,path,details FROM private_files WHERE conversation_id=?1",[conversation],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
    let mut paths=Vec::new();
    for (file,path,sealed) in rows {
        paths.push(path.clone());
        let transfers=core().db.transfers();
        transfers.lock().execute("DELETE FROM partials WHERE path=?1",[&path])?;
        if crate::transfer::store::retention_get(&file).is_some_and(|r|r.path!=path && !is_private_path(&r.path).unwrap_or(true)) {continue}
        let plain=Zeroizing::new(crate::privacy::open_bytes(conversation,&sealed)?);
        let details:Details=postcard::from_bytes(&plain)?;
        let chunk_size=postcard::from_bytes::<crate::transfer::wire::Manifest>(&details.manifest).map(|m|m.chunk_size).unwrap_or(crate::transfer::wire::CHUNK_SIZE as u32);
        let mut source=StorageFile::open(&path,false)?;
        let target=format!("{}/{}.src",core().db.files_dir("transfers"),hex::encode(file));
        let candidate=format!("{target}.{}",hex::encode(random::<8>()));
        let result:Result<()>=(|| {let mut out=File::options().write(true).create_new(true).open(&candidate)?;io::copy(&mut source,&mut out)?;out.sync_data()?;std::fs::rename(&candidate,&target)?;
            crate::transfer::store::retention_put_tx(&transfers.lock(),&file,&target,details.size,chunk_size,&details.manifest,details.expires_at)?;Ok(())})();
        if result.is_err(){let _=std::fs::remove_file(&candidate);}
        result?;
    }
    Ok(paths)
}

/// Commits ordinary composer bytes to ordinary storage; private commits use protect_tx instead.
pub(crate) fn publish_ordinary(file:&[u8;32])->Result<()> {
    let Some(ret)=crate::transfer::store::retention_get(file) else{return Ok(())};
    if !temporary().lock().contains_key(&ret.path) {return Ok(())}
    let mut source=StorageFile::open(&ret.path,false)?;
    let path=format!("{}/{}.src",core().db.files_dir("transfers"),hex::encode(file));
    let candidate=format!("{path}.{}",hex::encode(random::<8>()));
    let result=(|| {let mut out=File::options().write(true).create_new(true).open(&candidate)?;io::copy(&mut source,&mut out)?;out.sync_data()?;std::fs::rename(&candidate,&path)?;
        crate::transfer::store::retention_put_tx(&core().db.transfers().lock(),file,&path,ret.size,ret.chunk_size,&ret.manifest,ret.expires_at)?;Ok(())})();
    if result.is_err(){let _=std::fs::remove_file(&candidate);}else{temporary().lock().remove(&ret.path);let _=std::fs::remove_file(ret.path);}result
}

/// Selects a code-bound target before the first incoming chunk can be written.
pub(crate) fn download_path(db:&crate::db::Stores,file:&[u8;32],manifest:&crate::transfer::wire::Manifest)->Result<Option<String>> {
    if !crate::state::owns_stores(db) {return Ok(None)}
    let conn=core().db.messages().lock();
    let ordinary:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM message_media WHERE file_id=?1) OR EXISTS(SELECT 1 FROM message_media_renditions WHERE file_id=?1)",[file],|r|r.get(0))?;
    if ordinary{return Ok(None)}
    let rows=crate::data::media::private_file_rows_tx(&conn,file)?;
    let Some((conv,_,_))=rows.first() else {
        let known:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM private_files WHERE file_id=?1)",[file],|r|r.get(0))?;
        ensure!(!known,"Chat is locked"); return Ok(None)
    };
    if let Some(ret)=retention_tx(&conn,conv,file)? {
        // The manifest is authenticated by the message offer and validated by the receiver.
        ensure!(ret.size==manifest.total_size,"Private media size changed");
        return Ok(Some(ret.path))
    }
    let path=format!("{}/{}.private",core().db.files_dir("private-files"),hex::encode(random::<24>()));
    let key=Zeroizing::new(random::<32>());let output=create(&path,&key,manifest.total_size)?;output.sync_data()?;
    let details=Details{key:*key,size:manifest.total_size,manifest:postcard::to_allocvec(manifest)?,expires_at:u64::MAX};
    let result=(|| {
        let plain = Zeroizing::new(postcard::to_allocvec(&details)?);
        let sealed=crate::privacy::seal_bytes_tx(&conn,conv,&plain)?;
        conn.execute("INSERT INTO private_files(conversation_id,file_id,path,details) VALUES(?1,?2,?3,?4)",(conv.as_slice(),file.as_slice(),&path,sealed))?;
        Ok(Some(path.clone()))
    })();
    if result.is_err(){let _=std::fs::remove_file(path);}result
}

pub(crate) fn available_retention(file:&[u8;32])->Result<Option<crate::transfer::store::Retention>> {
    let conn=core().db.messages().lock();
    for (conv,_,_) in crate::data::media::private_file_rows_tx(&conn,file)? {
        protect_tx(&conn,&conv,file)?;
        if complete_tx(&conn,&conv,file)? {
            if let Some(ret)=retention_tx(&conn,&conv,file)? {return Ok(Some(ret))}
        }
    }
    Ok(None)
}

pub(crate) fn is_private_path(path:&str)->Result<bool> {
    Ok(core().db.messages().lock().query_row("SELECT EXISTS(SELECT 1 FROM private_files WHERE path=?1)",[path],|r|r.get(0))?)
}


pub(crate) fn remove_temporary(path:&str) {
    temporary().lock().remove(path);
    let _=std::fs::remove_file(path);
}

/// Bounded image/frame scratch is encrypted with a process-only key. It is never recoverable
/// from the ordinary storage key or an identity backup after the process exits.
pub(crate) struct Scratch { path:String, file:StorageFile }
impl Scratch {
    pub(crate) fn new(size:u64)->Result<Self> {
        let directory=std::env::temp_dir();
        let path=directory.join(format!("pz-{}.scratch",hex::encode(random::<24>()))).to_string_lossy().into_owned();
        let key=Zeroizing::new(random::<32>());let file=create(&path,&key,size)?;
        temporary().lock().insert(path.clone(),Temporary{key:Zeroizing::new(*key),size});
        Ok(Self{path,file})
    }
    pub(crate) fn as_file_mut(&mut self)->&mut StorageFile {&mut self.file}
}
impl Drop for Scratch {fn drop(&mut self){remove_temporary(&self.path)}}

pub(crate) fn complete_tx(conn:&Connection,conv:&[u8;16],file:&[u8;32])->Result<bool> {
    Ok(conn.query_row("SELECT complete FROM private_files WHERE conversation_id=?1 AND file_id=?2",(conv.as_slice(),file.as_slice()),|r|r.get(0)).optional()?.unwrap_or(false))
}
pub(crate) fn mark_complete(path:&str)->Result<()> {
    let conn=core().db.messages().lock();
    conn.execute("UPDATE private_files SET complete=1 WHERE path=?1",[path])?;
    let file: Option<[u8;32]> = conn.query_row("SELECT file_id FROM private_files WHERE path=?1",[path],|r|r.get(0)).optional()?;
    if let Some(file) = file {
        let owners: Vec<[u8;16]> = crate::db::all(&conn,"SELECT DISTINCT conversation_id FROM private_file_refs WHERE file_id=?1",[file],|r|r.get(0))?;
        for owner in owners { protect_tx(&conn,&owner,&file)?; }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encrypted_random_access_handles_sparse_slots_without_accepting_corruption() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("encrypted").to_string_lossy().into_owned();
        let mut file = create(&path, &[19;32], 3 * BLOCK as u64 + 7).unwrap();
        file.seek(SeekFrom::Start(2 * BLOCK as u64 + 3)).unwrap();
        file.write_all(b"tail").unwrap();
        file.seek(SeekFrom::Start(BLOCK as u64 + 5)).unwrap();
        file.write_all(b"middle").unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        assert_eq!(file.read(&mut [0;1]).unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
        file.write_all(b"head").unwrap();
        for (offset, expected) in [(0, b"head".as_slice()), (BLOCK as u64 + 5, b"middle"), (2 * BLOCK as u64 + 3, b"tail")] {
            file.seek(SeekFrom::Start(offset)).unwrap();
            let mut visible = vec![0;expected.len()]; file.read_exact(&mut visible).unwrap(); assert_eq!(visible,expected);
        }
        let disk = std::fs::read(&path).unwrap();
        assert!(!disk.windows(6).any(|bytes| bytes == b"middle"));
        let mut corrupt = File::options().write(true).open(&path).unwrap();
        corrupt.seek(SeekFrom::Start(8 + SLOT + 24)).unwrap(); corrupt.write_all(&[disk[(8 + SLOT + 24) as usize] ^ 1]).unwrap();
        file.seek(SeekFrom::Start(BLOCK as u64 + 5)).unwrap();
        assert!(file.read(&mut [0;6]).is_err());
        assert!(file.write(b"replace").is_err(), "partial overwrites must not silently replace an unauthenticated block");
    }

    #[test]
    fn encrypted_staging_validates_length_and_never_leaves_a_failed_candidate() {
        let directory = tempfile::tempdir().unwrap();
        assert!(stage_reader_in(&mut b"short".as_slice(), 6, directory.path()).is_err());
        assert!(stage_reader_in(&mut b"too long".as_slice(), 7, directory.path()).is_err());
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
        let bytes = vec![91; BLOCK * 2 + 17];
        let (path, manifest) = stage_reader_in(&mut bytes.as_slice(), bytes.len() as u64, directory.path()).unwrap();
        let mut decoded = Vec::new(); StorageFile::open(&path,false).unwrap().read_to_end(&mut decoded).unwrap();
        assert_eq!(decoded,bytes); assert_eq!(manifest.total_size,bytes.len() as u64);
        assert_ne!(std::fs::read(&path).unwrap(),bytes);
        remove_temporary(&path);
        assert!(!temporary().lock().contains_key(&path)); assert!(!std::path::Path::new(&path).exists());
    }
}
