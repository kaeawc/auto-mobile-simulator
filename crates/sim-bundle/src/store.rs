//! The content-addressed chunk store: `chunks/<aa>/<blake3>`, each chunk one zstd frame of the
//! data whose BLAKE3 names it. Identical data is stored once, and a reader checks every chunk
//! against its name.

use std::path::{Path, PathBuf};

use crate::{Error, Result};

/// The BLAKE3 of `data`, as the manifest writes it.
pub fn hash(data: &[u8]) -> String {
    blake3::hash(data).to_hex().to_string()
}

/// The BLAKE3 of the file at `path`, read in 1 MiB pieces (a 4.4 GB system image: ~3 s).
pub fn hash_file(path: &Path) -> Result<String> {
    let f = std::fs::File::open(path).map_err(|e| Error::Io(path.to_path_buf(), e))?;
    let mut h = blake3::Hasher::new();
    h.update_reader(std::io::BufReader::with_capacity(1 << 20, f)).map_err(|e| Error::Io(path.to_path_buf(), e))?;
    Ok(h.finalize().to_hex().to_string())
}

pub fn compress(data: &[u8], level: i32) -> Result<Vec<u8>> {
    zstd::bulk::compress(data, level).map_err(Error::Write)
}

/// Decompresses chunk `hash` (at most `max` bytes) and checks it is what its name says.
pub fn decompress_verified(hash_hex: &str, compressed: &[u8], max: u64) -> Result<Vec<u8>> {
    let data = zstd::bulk::decompress(compressed, max as usize).map_err(|_| Error::ChunkHash(hash_hex.to_owned()))?;
    if hash(&data) != hash_hex {
        return Err(Error::ChunkHash(hash_hex.to_owned()));
    }
    Ok(data)
}

/// The name a chunk has in a bundle: `chunks/<first two hex digits>/<hash>`.
pub fn chunk_name(hash: &str) -> String {
    format!("chunks/{}/{hash}", &hash[..2])
}

/// The hash a bundle entry named like [`chunk_name`] holds, if it is named so.
pub fn parse_chunk_name(name: &str) -> Option<&str> {
    let rest = name.strip_prefix("chunks/")?;
    let (dir, hash) = rest.split_once('/')?;
    (crate::manifest::is_hash(hash) && dir == &hash[..2]).then_some(hash)
}

/// A bundle directory's chunks.
pub struct ChunkStore {
    root: PathBuf,
}

impl ChunkStore {
    /// The store of the bundle directory `root` (its `chunks/`).
    pub fn new(root: &Path) -> ChunkStore {
        ChunkStore { root: root.to_path_buf() }
    }

    pub fn path(&self, hash: &str) -> PathBuf {
        self.root.join(chunk_name(hash))
    }

    pub fn has(&self, hash: &str) -> bool {
        self.path(hash).is_file()
    }

    /// Stores `compressed` as chunk `hash` unless the store has it; whether it was new. Written
    /// beside and renamed, so a chunk file is whole or absent.
    pub fn put(&self, hash: &str, compressed: &[u8]) -> Result<bool> {
        let path = self.path(hash);
        if path.is_file() {
            return Ok(false);
        }
        let dir = path.parent().unwrap_or(&self.root);
        std::fs::create_dir_all(dir).map_err(|e| Error::Io(dir.to_path_buf(), e))?;
        let tmp = dir.join(format!(".{hash}.partial"));
        std::fs::write(&tmp, compressed).map_err(|e| Error::Io(tmp.clone(), e))?;
        std::fs::rename(&tmp, &path).map_err(|e| Error::Io(path.clone(), e))?;
        Ok(true)
    }

    /// Chunk `hash`, decompressed (at most `max` bytes) and checked against its name.
    pub fn get(&self, hash: &str, max: u64) -> Result<Vec<u8>> {
        let path = self.path(hash);
        let compressed = std::fs::read(&path).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => Error::Missing { first: hash.to_owned(), count: 1 },
            _ => Error::Io(path.clone(), e),
        })?;
        decompress_verified(hash, &compressed, max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_chunks_are_stored_once_and_checked_on_read() {
        let root = std::env::temp_dir().join(format!("sim-bundle-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let store = ChunkStore::new(&root);
        let data = vec![42u8; 100_000];
        let h = hash(&data);
        let z = compress(&data, 3).expect("compress");
        assert!(z.len() < 1000, "a run compresses");
        assert!(store.put(&h, &z).expect("put"));
        assert!(!store.put(&h, &z).expect("put again"), "already stored");
        assert_eq!(std::fs::read_dir(root.join("chunks")).expect("chunks").count(), 1);
        assert_eq!(store.path(&h), root.join(format!("chunks/{}/{h}", &h[..2])));
        assert_eq!(parse_chunk_name(&chunk_name(&h)), Some(h.as_str()));
        assert_eq!(parse_chunk_name(&format!("chunks/zz/{h}")), None, "the directory is the hash's prefix");
        assert_eq!(parse_chunk_name("chunks/../x"), None);
        assert_eq!(store.get(&h, 1 << 20).expect("get"), data);
        assert!(store.get(&h, 10).is_err(), "longer than allowed");
        // A chunk whose content isn't what its name says.
        let other = compress(b"other", 3).expect("compress");
        std::fs::write(store.path(&h), other).expect("overwrite");
        assert!(matches!(store.get(&h, 1 << 20), Err(Error::ChunkHash(_))));
        assert!(matches!(store.get(&hash(b"absent"), 1 << 20), Err(Error::Missing { .. })));
        let _ = std::fs::remove_dir_all(root);
    }
}
