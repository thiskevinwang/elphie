use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;
use uuid::Uuid;

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Tokens {
    pub access: String,
    pub refresh: String,
    pub id: String,
    pub expires_at: u64,
    pub scope: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Account {
    pub subject: String,
    pub email: Option<String>,
    pub tokens: Option<Tokens>,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Accounts {
    pub host_id: String,
    pub active: Option<String>,
    pub accounts: BTreeMap<String, Account>,
}

pub(crate) struct Store {
    dir: PathBuf,
    _lock: File,
    pub data: Accounts,
}

pub(crate) fn config_dir() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("ELPHIE_CONFIG_DIR") {
        let dir = PathBuf::from(path);
        ensure!(dir.is_absolute(), "ELPHIE_CONFIG_DIR must be absolute");
        return Ok(dir);
    }
    Ok(dirs::home_dir()
        .context("Cannot locate the user's home directory")?
        .join(".elphie"))
}

impl Store {
    pub async fn lock(dir: &Path) -> Result<Self> {
        let dir = dir.to_owned();
        tokio::task::spawn_blocking(move || Self::open(dir)).await?
    }

    fn open(dir: PathBuf) -> Result<Self> {
        let mut directory = fs::DirBuilder::new();
        directory.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            directory.mode(0o700);
        }
        directory.create(&dir)?;
        let metadata = fs::symlink_metadata(&dir)?;
        ensure!(metadata.is_dir(), "Invalid Elphie configuration directory");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            ensure!(
                metadata.permissions().mode() & 0o077 == 0,
                "Elphie configuration directory must have mode 0700"
            );
        }
        let mut options = OpenOptions::new();
        options.create(true).truncate(false).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let lock = options.open(dir.join(".signin.lock"))?;
        lock.lock()?;
        let data = match fs::read(dir.join("chatgpt.json")) {
            Ok(bytes) => {
                serde_json::from_slice(&bytes).context("Cannot read saved ChatGPT registrations")?
            }
            Err(err) if err.kind() == ErrorKind::NotFound => Accounts {
                host_id: format!("urn:uuid:{}", Uuid::new_v4()),
                active: None,
                accounts: BTreeMap::new(),
            },
            Err(err) => return Err(err.into()),
        };
        Ok(Self {
            dir,
            _lock: lock,
            data,
        })
    }

    pub fn save(&self) -> Result<()> {
        let mut temp = NamedTempFile::new_in(&self.dir)?;
        serde_json::to_writer(&mut temp, &self.data)?;
        temp.flush()?;
        temp.as_file().sync_all()?;
        temp.persist(self.dir.join("chatgpt.json"))?;
        #[cfg(unix)]
        File::open(&self.dir)?.sync_all()?;
        Ok(())
    }
}
