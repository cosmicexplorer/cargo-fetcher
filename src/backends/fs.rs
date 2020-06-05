use crate::Krate;

use anyhow::Error;
use async_std::{fs, io};
use bytes::Bytes;
use serde;
use serde_json;
use sha2::Sha256;

use std::path::PathBuf;

// TODO: This should be Copy too! We should be able to use a fixed-size byte array instead of a
// heap-allocated string.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
struct Digest {
    hex: String,
}

impl Digest {
    fn from_bytes(bytes: &[u8]) -> Self {
        let mut hasher = Sha256::default();
        hasher.input(bytes);
        let hex = hasher.hex_digest();
        Self { hex }
    }

    fn to_hex(&self) -> String {
        self.hex.clone()
    }
}

struct FilesystemDB {
    root: PathBuf,
}

impl FilesystemDB {
    async fn lookup(&self, key: Digest) -> Result<Option<Bytes>, Error> {
        let hex = key.to_hex();
        let entry_path = self.root.join(hex);
        match fs::read(&entry_path).await {
            Ok(bytes) => Ok(Some(Bytes::from(bytes))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }
    async fn insert(&self, value: Bytes) -> Result<Digest, Error> {
        let key = Digest::from_bytes(&value);
        let hex = key.to_hex();
        let entry_path = self.root.join(hex);
        fs::write(&entry_path, &value).await?;
        Ok(key)
    }
}

pub struct FSBackend {
    root: PathBuf,
    krate_lookup_db: PathBuf,
}

impl FSBackend {
    pub async fn new(loc: crate::FilesystemLocation<'_>) -> Result<Self, Error> {
        let crate::FilesystemLocation { path } = loc;
        fs::create_dir_all(path).await?;
        Ok(Self {
            root: path.to_path_buf(),
        })
    }
}

#[async_trait::async_trait]
impl crate::Backend for FSBackend {
    async fn fetch(&self, krate: &Krate) -> Result<Bytes, Error> {}
}
