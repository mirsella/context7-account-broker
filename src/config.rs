use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::env;
use std::ffi::OsString;
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

const APP_DIR: &str = "context7-account-broker";
const DEFAULT_PORT: u16 = 14197;
const DEFAULT_COOLDOWN_MS: u64 = 30_000;
const DEFAULT_CACHE_DAYS: u64 = 30;
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

pub(crate) struct Settings {
    pub(crate) accounts_path: PathBuf,
    pub(crate) cache_path: PathBuf,
    pub(crate) token_path: PathBuf,
    pub(crate) port: u16,
    pub(crate) cooldown: Duration,
    pub(crate) cache_ttl: Duration,
}

pub(crate) fn settings() -> io::Result<Settings> {
    settings_from(&|name| env::var_os(name))
}

pub(crate) fn accounts_path() -> io::Result<PathBuf> {
    accounts_path_from(&|name| env::var_os(name))
}

pub(crate) fn load_accounts(path: &Path) -> io::Result<Vec<AccountRecord>> {
    load_accounts_from(path, &|name| env::var_os(name))
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

pub(crate) fn read_server_token(path: &Path) -> io::Result<String> {
    ensure_parent(path)?;
    ensure_private_file(path, "server token file")?;
    let token = fs::read_to_string(path)?.trim().to_owned();
    if token.is_empty() {
        return Err(io::Error::other("server token file is empty"));
    }
    Ok(token)
}

pub(crate) fn ensure_server_token(path: &Path) -> io::Result<String> {
    let parent = ensure_parent(path)?;
    match fs::symlink_metadata(path) {
        Ok(_) => return read_server_token(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    let mut random = [0_u8; 32];
    fs::File::open("/dev/urandom")?.read_exact(&mut random)?;
    let token: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
    let (temporary, mut file) = private_temp(path, parent)?;
    let installed = (|| {
        file.write_all(token.as_bytes())?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        match fs::hard_link(&temporary, path) {
            Ok(()) => {
                fs::remove_file(&temporary)?;
                fs::File::open(parent)?.sync_all()?;
                Ok(token)
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                fs::remove_file(&temporary)?;
                read_server_token(path)
            }
            Err(error) => Err(error),
        }
    })();
    if installed.is_err() {
        let _ = fs::remove_file(temporary);
    }
    installed
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

fn settings_from(get: &impl Fn(&str) -> Option<OsString>) -> io::Result<Settings> {
    let port = parse_u64(get, "CONTEXT7_BROKER_PORT", DEFAULT_PORT as u64)?;
    if port > u16::MAX as u64 {
        return Err(io::Error::other("CONTEXT7_BROKER_PORT is out of range"));
    }
    let cache_days = parse_u64(get, "CONTEXT7_CACHE_TTL_DAYS", DEFAULT_CACHE_DAYS)?;
    if cache_days == 0 {
        return Err(io::Error::other("CONTEXT7_CACHE_TTL_DAYS must be positive"));
    }
    let cache_seconds = cache_days
        .checked_mul(86_400)
        .ok_or_else(|| io::Error::other("CONTEXT7_CACHE_TTL_DAYS is out of range"))?;
    Ok(Settings {
        accounts_path: accounts_path_from(get)?,
        cache_path: match get("CONTEXT7_CACHE_DIR") {
            Some(path) => path.into(),
            None => cache_home(get)?.join(APP_DIR),
        },
        token_path: match get("CONTEXT7_BROKER_TOKEN_FILE") {
            Some(path) => path.into(),
            None => config_home(get)?.join(APP_DIR).join("server-token"),
        },
        port: port as u16,
        cooldown: Duration::from_millis(parse_u64(
            get,
            "CONTEXT7_ACCOUNT_COOLDOWN_MS",
            DEFAULT_COOLDOWN_MS,
        )?),
        cache_ttl: Duration::from_secs(cache_seconds),
    })
}

fn accounts_path_from(get: &impl Fn(&str) -> Option<OsString>) -> io::Result<PathBuf> {
    match get("CONTEXT7_BROKER_CONFIG") {
        Some(path) => Ok(path.into()),
        None => Ok(config_home(get)?.join(APP_DIR).join("accounts.json")),
    }
}

fn load_accounts_from(
    path: &Path,
    get: &impl Fn(&str) -> Option<OsString>,
) -> io::Result<Vec<AccountRecord>> {
    let mut accounts = load_configured_accounts(path)?;
    if accounts.is_empty() || env_text(get, "CONTEXT7_BROKER_INCLUDE_ENV")?.as_deref() == Some("1")
    {
        let mut keys: HashSet<_> = accounts
            .iter()
            .map(|account| account.api_key.clone())
            .collect();
        let mut names: HashSet<_> = accounts
            .iter()
            .map(|account| account.name.clone())
            .collect();
        let mut index = 1;
        for api_key in environment_keys(get)? {
            if keys.insert(api_key.clone()) {
                while names.contains(&format!("env-{index}")) {
                    index += 1;
                }
                let name = format!("env-{index}");
                names.insert(name.clone());
                accounts.push(AccountRecord { name, api_key });
                index += 1;
            }
        }
    }
    validate_accounts(&accounts)?;
    Ok(accounts)
}

fn environment_keys(get: &impl Fn(&str) -> Option<OsString>) -> io::Result<Vec<String>> {
    let mut keys = Vec::new();
    for variable in ["CONTEXT7_API_KEYS", "CONTEXT7_API_KEY"] {
        if let Some(value) = env_text(get, variable)? {
            for key in value
                .split(|character: char| character.is_whitespace() || character == ',')
                .filter(|key| !key.is_empty())
            {
                if !key.starts_with("ctx7sk") {
                    return Err(io::Error::other(
                        "environment API key must start with ctx7sk",
                    ));
                }
                if !keys.iter().any(|known| known == key) {
                    keys.push(key.to_owned());
                }
            }
        }
    }
    Ok(keys)
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

fn config_home(get: &impl Fn(&str) -> Option<OsString>) -> io::Result<PathBuf> {
    home_path(get, "XDG_CONFIG_HOME", ".config")
}

fn cache_home(get: &impl Fn(&str) -> Option<OsString>) -> io::Result<PathBuf> {
    home_path(get, "XDG_CACHE_HOME", ".cache")
}

fn home_path(
    get: &impl Fn(&str) -> Option<OsString>,
    xdg_name: &str,
    home_suffix: &str,
) -> io::Result<PathBuf> {
    get(xdg_name)
        .map(PathBuf::from)
        .or_else(|| get("HOME").map(|home| PathBuf::from(home).join(home_suffix)))
        .ok_or_else(|| io::Error::other("HOME is not set"))
}

fn env_text(get: &impl Fn(&str) -> Option<OsString>, name: &str) -> io::Result<Option<String>> {
    get(name)
        .map(|value| {
            value
                .into_string()
                .map_err(|_| io::Error::other(format!("{name} must be valid Unicode")))
        })
        .transpose()
}

fn parse_u64(get: &impl Fn(&str) -> Option<OsString>, name: &str, default: u64) -> io::Result<u64> {
    env_text(get, name)?.map_or(Ok(default), |value| {
        value
            .parse()
            .map_err(|_| io::Error::other(format!("{name} must be an integer")))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn values(home: &Path) -> HashMap<String, OsString> {
        HashMap::from([("HOME".to_owned(), home.as_os_str().to_owned())])
    }

    #[test]
    fn configured_accounts_are_an_allowlist_unless_env_is_enabled() {
        let directory = tempfile::tempdir().unwrap();
        let mut environment = values(directory.path());
        environment.insert(
            "CONTEXT7_BROKER_CONFIG".to_owned(),
            directory.path().join("private/accounts.json").into(),
        );
        environment.insert("CONTEXT7_API_KEYS".to_owned(), "ctx7sk-env".into());
        let get = |name: &str| environment.get(name).cloned();
        let path = accounts_path_from(&get).unwrap();
        add_account(&path, "saved", "ctx7sk-saved").unwrap();
        assert_eq!(load_accounts_from(&path, &get).unwrap().len(), 1);
        environment.insert("CONTEXT7_BROKER_INCLUDE_ENV".to_owned(), "1".into());
        let accounts = load_accounts_from(&path, &|name| environment.get(name).cloned()).unwrap();
        assert_eq!(accounts.len(), 2);
        assert_eq!(accounts[1].name, "env-1");
    }

    #[test]
    fn rejects_invalid_accounts_and_ttl_overflow() {
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
        environment.insert(
            "CONTEXT7_CACHE_TTL_DAYS".to_owned(),
            u64::MAX.to_string().into(),
        );
        assert!(settings_from(&|name| environment.get(name).cloned()).is_err());
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

    #[test]
    fn concurrent_token_creation_converges() {
        let directory = tempfile::tempdir().unwrap();
        let path = std::sync::Arc::new(directory.path().join("private/server-token"));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let path = path.clone();
                std::thread::spawn(move || ensure_server_token(&path).unwrap())
            })
            .collect();
        let tokens: Vec<_> = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect();
        assert_eq!(tokens.iter().collect::<HashSet<_>>().len(), 1);
        assert_eq!(tokens[0].len(), 64);
        assert_eq!(read_server_token(&path).unwrap(), tokens[0]);
        assert_eq!(
            fs::metadata(&*path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
