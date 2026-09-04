use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::env;
use std::ffi::OsString;
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const APP_DIR: &str = "context7-account-broker";
static TEMP_FILE_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct AccountRecord {
    pub(crate) name: String,
    #[serde(rename = "apiKey")]
    pub(crate) api_key: String,
}

#[derive(Deserialize, Serialize)]
struct AccountsFile {
    version: u8,
    accounts: Vec<AccountRecord>,
}

pub(crate) fn accounts_path() -> io::Result<PathBuf> {
    accounts_path_from(&|name| env::var_os(name))
}

pub(crate) fn cache_path() -> io::Result<PathBuf> {
    Ok(
        home_path(&|name| env::var_os(name), "XDG_CACHE_HOME", ".cache")?
            .join(APP_DIR)
            .join(env!("CARGO_PKG_VERSION")),
    )
}

pub(crate) fn runtime_path() -> io::Result<PathBuf> {
    Ok(
        home_path(&|name| env::var_os(name), "XDG_CONFIG_HOME", ".config")?
            .join(APP_DIR)
            .join(env!("CARGO_PKG_VERSION")),
    )
}

pub(crate) fn load_accounts(path: &Path) -> io::Result<Vec<AccountRecord>> {
    load_configured_accounts(path)
}

pub(crate) fn add_account(path: &Path, name: &str, api_key: &str) -> io::Result<()> {
    let mut accounts = load_configured_accounts(path)?;
    accounts.push(AccountRecord {
        name: name.to_owned(),
        api_key: api_key.to_owned(),
    });
    write_accounts(path, &accounts)
}

pub(crate) fn remove_account(path: &Path, name: &str) -> io::Result<()> {
    let mut accounts = load_configured_accounts(path)?;
    let before = accounts.len();
    accounts.retain(|account| account.name != name);
    if accounts.len() == before {
        return Err(io::Error::other(format!("account {name:?} not found")));
    }
    write_accounts(path, &accounts)
}

pub(crate) fn ensure_private_directory(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let mut builder = DirBuilder::new();
            if let Err(error) = builder.recursive(true).mode(0o700).create(path)
                && error.kind() != io::ErrorKind::AlreadyExists
            {
                return Err(error);
            }
        }
        Err(error) => return Err(error),
        Ok(_) => {}
    }
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(io::Error::other(format!(
            "{} must be a directory, not a symlink",
            path.display()
        )));
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(io::Error::other(format!(
            "{} permissions must be owner-only",
            path.display()
        )));
    }
    Ok(())
}

pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = ensure_parent(path)?;
    let (temporary, mut file) = private_temp(path, parent)?;
    let result = (|| {
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        fs::File::open(parent)?.sync_all()
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn private_temp(path: &Path, parent: &Path) -> io::Result<(PathBuf, fs::File)> {
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::other("file path has no name"))?
        .to_string_lossy();
    loop {
        let id = TEMP_FILE_ID.fetch_add(1, Ordering::Relaxed);
        let temporary = parent.join(format!(".{name}.{}.{}.tmp", std::process::id(), id));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
        {
            Ok(file) => return Ok((temporary, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
}

fn accounts_path_from(get: &impl Fn(&str) -> Option<OsString>) -> io::Result<PathBuf> {
    Ok(home_path(get, "XDG_CONFIG_HOME", ".config")?
        .join(APP_DIR)
        .join("accounts.json"))
}

fn load_configured_accounts(path: &Path) -> io::Result<Vec<AccountRecord>> {
    ensure_parent(path)?;
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
        Ok(_) => ensure_private_file(path, "accounts file")?,
    }
    let file: AccountsFile = serde_json::from_slice(&fs::read(path)?).map_err(io::Error::other)?;
    if file.version != 1 {
        return Err(io::Error::other("accounts.json version must be 1"));
    }
    validate_accounts(&file.accounts)?;
    Ok(file.accounts)
}

fn write_accounts(path: &Path, accounts: &[AccountRecord]) -> io::Result<()> {
    validate_accounts(accounts)?;
    write_private(
        path,
        &serde_json::to_vec_pretty(&AccountsFile {
            version: 1,
            accounts: accounts.to_vec(),
        })
        .map_err(io::Error::other)?,
    )
}

fn validate_accounts(accounts: &[AccountRecord]) -> io::Result<()> {
    let mut names = HashSet::new();
    let mut keys = HashSet::new();
    for account in accounts {
        let name = account.name.as_bytes();
        if name.is_empty()
            || name.len() > 64
            || !name[0].is_ascii_alphanumeric()
            || !name
                .iter()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        {
            return Err(io::Error::other(format!(
                "invalid account name {:?}",
                account.name
            )));
        }
        if !account.api_key.starts_with("ctx7sk") {
            return Err(io::Error::other(format!(
                "account {:?} API key must start with ctx7sk",
                account.name
            )));
        }
        if !names.insert(&account.name) {
            return Err(io::Error::other(format!(
                "duplicate account name {:?}",
                account.name
            )));
        }
        if !keys.insert(&account.api_key) {
            return Err(io::Error::other("duplicate Context7 API key"));
        }
    }
    Ok(())
}

fn ensure_parent(path: &Path) -> io::Result<&Path> {
    let parent = path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    ensure_private_directory(parent)?;
    Ok(parent)
}

fn ensure_private_file(path: &Path, label: &str) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(io::Error::other(format!("{label} must be a regular file")));
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(io::Error::other(format!(
            "{label} permissions must be owner-only"
        )));
    }
    Ok(())
}

fn home_path(
    get: &impl Fn(&str) -> Option<OsString>,
    xdg_name: &str,
    home_suffix: &str,
) -> io::Result<PathBuf> {
    if let Some(path) = env_path(get, xdg_name)? {
        return Ok(path);
    }
    Ok(env_path(get, "HOME")?
        .ok_or_else(|| io::Error::other("HOME is not set"))?
        .join(home_suffix))
}

fn env_path(get: &impl Fn(&str) -> Option<OsString>, name: &str) -> io::Result<Option<PathBuf>> {
    let Some(value) = get(name).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let path = PathBuf::from(value);
    if !path.is_absolute() {
        return Err(io::Error::other(format!("{name} must be an absolute path")));
    }
    Ok(Some(path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn values(home: &Path) -> HashMap<String, OsString> {
        HashMap::from([("HOME".to_owned(), home.as_os_str().to_owned())])
    }

    #[test]
    fn rejects_invalid_accounts_and_relative_xdg_paths() {
        let duplicate = vec![
            AccountRecord {
                name: "one".to_owned(),
                api_key: "ctx7sk-one".to_owned(),
            },
            AccountRecord {
                name: "one".to_owned(),
                api_key: "ctx7sk-two".to_owned(),
            },
        ];
        assert!(validate_accounts(&duplicate).is_err());
        assert!(
            validate_accounts(&[AccountRecord {
                name: "bad name".to_owned(),
                api_key: "ctx7sk-key".to_owned(),
            }])
            .is_err()
        );
        let directory = tempfile::tempdir().unwrap();
        let mut environment = values(directory.path());
        environment.insert("XDG_CONFIG_HOME".to_owned(), "relative".into());
        assert!(accounts_path_from(&|name| environment.get(name).cloned()).is_err());
    }

    #[test]
    fn isolates_runtime_state_by_package_version() {
        let version = env!("CARGO_PKG_VERSION");
        assert!(
            cache_path()
                .unwrap()
                .ends_with(Path::new(APP_DIR).join(version))
        );
        assert!(
            runtime_path()
                .unwrap()
                .ends_with(Path::new(APP_DIR).join(version))
        );
    }

    #[test]
    fn rejects_insecure_directory_without_changing_it() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("insecure");
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(ensure_private_directory(&path).is_err());
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }

    #[test]
    fn writes_private_version_one_files() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("private/accounts.json");
        add_account(&path, "personal", "ctx7sk-key").unwrap();
        assert_eq!(
            fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let value: serde_json::Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(value["version"], 1);
        assert_eq!(value["accounts"][0]["apiKey"], "ctx7sk-key");
    }
}
