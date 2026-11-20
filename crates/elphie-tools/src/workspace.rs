use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use rustix::fd::OwnedFd;
use rustix::fs::{self, Dev, Dir, FileType, Mode, OFlags, Stat};
use serde_json::{Value, json};

use crate::{LIST, MAX_PATH_BYTES, READ};

const MAX_BYTES: usize = 32 * 1024;
const MAX_COMPONENTS: usize = 32;
const MAX_DIRECTORY_SCAN: usize = 1024;
const MAX_DIRECTORY_ENTRIES: usize = 256;

pub struct Workspace {
    root: OwnedFd,
    device: Dev,
}

impl Workspace {
    pub fn open(path: &Path) -> Result<Self> {
        let root = fs::open(
            path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .context("Cannot open the workspace directory")?;
        #[cfg(target_os = "linux")]
        {
            use linux_raw_sys::general::*;
            const CONFIGFS_MAGIC: u32 = 0x6265_6570;
            let fs_type = fs::fstatfs(&root)?.f_type as u32;
            ensure!(
                !matches!(
                    fs_type,
                    PROC_SUPER_MAGIC
                        | SYSFS_MAGIC
                        | CONFIGFS_MAGIC
                        | DEBUGFS_MAGIC
                        | TRACEFS_MAGIC
                        | SECURITYFS_MAGIC
                        | SELINUX_MAGIC
                        | AAFS_MAGIC
                        | SMACK_MAGIC
                        | CGROUP_SUPER_MAGIC
                        | CGROUP2_SUPER_MAGIC
                        | BPF_FS_MAGIC
                        | EFIVARFS_MAGIC
                        | PSTOREFS_MAGIC
                        | RDTGROUP_SUPER_MAGIC
                        | BINFMTFS_MAGIC
                        | DEVPTS_SUPER_MAGIC
                        | XENFS_SUPER_MAGIC
                ),
                "Kernel filesystems cannot be used as a workspace."
            );
        }
        let device = fs::fstat(&root)?.st_dev;
        let workspace = Self { root, device };
        workspace.open_path(Path::new("."), OFlags::DIRECTORY)?;
        Ok(workspace)
    }

    pub fn execute(&self, name: &str, arguments: &Value) -> Result<String, String> {
        let path = relative_path(arguments, name == LIST).map_err(|err| err.to_string())?;
        match name {
            LIST => self.list(&path),
            READ => self.read(&path),
            _ => return Err("Unknown workspace tool.".into()),
        }
        .map_err(|err| err.to_string())
    }

    fn open_path(&self, path: &Path, flags: OFlags) -> Result<OwnedFd> {
        let flags = flags | OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NONBLOCK | OFlags::NOCTTY;
        #[cfg(target_os = "linux")]
        let fd = fs::openat2(
            &self.root,
            path,
            flags,
            Mode::empty(),
            fs::ResolveFlags::BENEATH | fs::ResolveFlags::NO_SYMLINKS | fs::ResolveFlags::NO_XDEV,
        );
        #[cfg(target_os = "macos")]
        let fd = fs::openat(
            &self.root,
            path,
            flags | OFlags::NOFOLLOW_ANY,
            Mode::empty(),
        );
        let fd = fd.context("Cannot open this workspace path")?;
        ensure!(
            fs::fstat(&fd)?.st_dev == self.device,
            "Path is on another filesystem."
        );
        #[cfg(target_os = "macos")]
        {
            use std::ffi::OsStr;
            use std::os::unix::ffi::OsStrExt;
            let root = fs::getpath(&self.root)?;
            let opened = fs::getpath(&fd)?;
            let directory = Path::new(OsStr::from_bytes(root.to_bytes()));
            let path = Path::new(OsStr::from_bytes(opened.to_bytes()));
            let relative = path
                .strip_prefix(directory)
                .context("Path is outside the workspace.")?;
            ensure!(
                relative
                    .iter()
                    .all(|part| part.to_str().is_some_and(visible_name)),
                "Path is hidden or unavailable."
            );
        }
        Ok(fd)
    }

    fn list(&self, path: &Path) -> Result<String> {
        let directory = self.open_path(path, OFlags::DIRECTORY)?;
        let mut entries = BTreeMap::new();
        for (index, entry) in Dir::read_from(&directory)?.enumerate() {
            ensure!(
                index < MAX_DIRECTORY_SCAN,
                "Directory exceeds the scan limit."
            );
            let entry = entry?;
            let Ok(name) = entry.file_name().to_str() else {
                continue;
            };
            if !visible_name(name) {
                continue;
            }
            let stat = match fs::statat(&directory, name, fs::AtFlags::SYMLINK_NOFOLLOW) {
                Ok(stat) => stat,
                Err(rustix::io::Errno::NOENT) => continue,
                Err(err) => return Err(err.into()),
            };
            if stat.st_dev != self.device {
                continue;
            }
            let kind = match FileType::from_raw_mode(stat.st_mode) {
                FileType::Directory => "directory",
                FileType::RegularFile if stat.st_nlink == 1 => "file",
                _ => continue,
            };
            entries.insert(name.to_owned(), kind);
            ensure!(
                entries.len() <= MAX_DIRECTORY_ENTRIES,
                "Directory exceeds 256 visible entries."
            );
        }
        let entries: Vec<_> = entries
            .into_iter()
            .map(|(name, kind)| json!({"name":name, "kind":kind}))
            .collect();
        let output = serde_json::to_string(&entries)?;
        ensure!(
            output.len() <= MAX_BYTES,
            "Directory listing exceeds 32 KiB."
        );
        Ok(output)
    }

    fn read(&self, path: &Path) -> Result<String> {
        let name = path.file_name().context("A file path is required.")?;
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let parent = self.open_path(parent, OFlags::DIRECTORY)?;
        let before = fs::statat(&parent, name, fs::AtFlags::SYMLINK_NOFOLLOW)
            .context("Cannot inspect this file")?;
        check_file(&before, self.device)?;
        let file = File::from(self.open_path(path, OFlags::empty())?);
        let opened = fs::fstat(&file)?;
        check_file(&opened, self.device)?;
        ensure!(before.st_ino == opened.st_ino, "File changed during open.");
        let mut bytes = Vec::new();
        (&file).take(MAX_BYTES as u64 + 1).read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= MAX_BYTES, "File exceeds 32 KiB.");
        check_file(&fs::fstat(&file)?, self.device)?;
        let text = String::from_utf8(bytes).context("File must contain UTF-8 text.")?;
        ensure!(
            !text
                .chars()
                .any(|c| c.is_control() && !matches!(c, '\t' | '\n' | '\r')),
            "File contains unsupported control characters."
        );
        Ok(text)
    }
}

fn check_file(stat: &Stat, device: Dev) -> Result<()> {
    ensure!(
        stat.st_dev == device
            && FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile
            && stat.st_nlink == 1,
        "Only regular workspace files with one link can be read."
    );
    ensure!(
        (0..=MAX_BYTES as i64).contains(&stat.st_size),
        "File exceeds 32 KiB."
    );
    Ok(())
}

fn relative_path(arguments: &Value, directory: bool) -> Result<PathBuf> {
    let object = arguments
        .as_object()
        .context("Expected an object with one path.")?;
    ensure!(object.len() == 1, "Expected exactly one path.");
    let path = object
        .get("path")
        .and_then(Value::as_str)
        .context("Expected a path string.")?;
    ensure!(
        !path.is_empty() && path.len() <= MAX_PATH_BYTES && !path.starts_with('/'),
        "Expected a relative path of up to 1,024 bytes."
    );
    let path = if directory {
        path.trim_end_matches('/')
    } else {
        path
    };
    let mut result = PathBuf::new();
    for (index, name) in path.split('/').enumerate() {
        ensure!(index < MAX_COMPONENTS, "Path exceeds 32 components.");
        if name == "." {
            continue;
        }
        ensure!(
            visible_name(name),
            "Expected a visible relative workspace path."
        );
        result.push(name);
    }
    if result.as_os_str().is_empty() {
        result.push(".");
    }
    Ok(result)
}

fn visible_name(name: &str) -> bool {
    let drive_letter = matches!(name.as_bytes(), [b'a'..=b'z' | b'A'..=b'Z', b':']);
    !name.is_empty()
        && !name.starts_with('.')
        && !name.contains('\\')
        && !drive_letter
        && !name.chars().any(unsafe_display_character)
}

fn unsafe_display_character(c: char) -> bool {
    c.is_control()
        || matches!(c, '\u{061c}' | '\u{200b}'..='\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}')
}
