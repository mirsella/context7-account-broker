use crate::config::{AccountRecord, ensure_private_directory, write_private};
use crate::context7::{Context7, Error, Quota, Request, Upstream};
use futures::future::{BoxFuture, FutureExt, Shared};
use reqwest::StatusCode;
use rmcp::model::{CallToolResult, ContentBlock};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{Mutex, Semaphore};

const UPSTREAM_CONCURRENCY: usize = 4;
type SharedCall = Shared<BoxFuture<'static, CallToolResult>>;

struct Account {
    name: String,
    api_key: String,
    quota: Option<Quota>,
    cooldown_until: Option<SystemTime>,
}

struct Pool {
    accounts: Vec<Account>,
    next: usize,
}

impl Pool {
    fn order(&mut self, affinity: Option<&str>, now: SystemTime) -> Vec<usize> {
        let len = self.accounts.len();
        if len == 0 {
            return Vec::new();
        }
        let start = self.next % len;
        self.next = (self.next + 1) % len;
        let mut order: Vec<_> = (0..len).collect();
        order.sort_by(|left, right| {
            let left_account = &self.accounts[*left];
            let right_account = &self.accounts[*right];
            (affinity == Some(right_account.name.as_str()))
                .cmp(&(affinity == Some(left_account.name.as_str())))
                .then_with(|| {
                    quota_usage(left_account, now).total_cmp(&quota_usage(right_account, now))
                })
                .then_with(|| ((*left + len - start) % len).cmp(&((*right + len - start) % len)))
        });
        order
    }

    fn available(&self, index: usize, now: SystemTime) -> bool {
        !self.accounts[index]
            .cooldown_until
            .is_some_and(|until| until > now)
    }
}

pub(crate) struct Broker {
    context7: Context7,
    cache: Cache,
    cooldown: Duration,
    semaphore: Semaphore,
    pool: Mutex<Pool>,
    inflight: StdMutex<HashMap<String, SharedCall>>,
}

impl Broker {
    pub(crate) fn new(
        records: Vec<AccountRecord>,
        context7: Context7,
        cache_path: impl Into<PathBuf>,
        cache_ttl: Duration,
        cooldown: Duration,
    ) -> io::Result<Arc<Self>> {
        Ok(Arc::new(Self {
            context7,
            cache: Cache::new(cache_path, cache_ttl)?,
            cooldown,
            semaphore: Semaphore::new(UPSTREAM_CONCURRENCY),
            pool: Mutex::new(Pool {
                accounts: records
                    .into_iter()
                    .map(|record| Account {
                        name: record.name,
                        api_key: record.api_key,
                        quota: None,
                        cooldown_until: None,
                    })
                    .collect(),
                next: 0,
            }),
            inflight: StdMutex::new(HashMap::new()),
        }))
    }

    pub(crate) async fn call(self: &Arc<Self>, request: Request) -> CallToolResult {
        let request_key = Cache::request_hash(&request);
        let (shared, start) = {
            let mut inflight = self.inflight.lock().expect("in-flight mutex poisoned");
            if let Some(shared) = inflight.get(&request_key) {
                (shared.clone(), false)
            } else {
                let broker = Arc::downgrade(self);
                let call_key = request_key.clone();
                let shared = async move {
                    broker
                        .upgrade()
                        .expect("an awaiting caller must own the broker")
                        .call_uncached(&call_key, request)
                        .await
                }
                .boxed()
                .shared();
                inflight.insert(request_key.clone(), shared.clone());
                (shared, true)
            }
        };
        if start {
            let broker = Arc::downgrade(self);
            let cleanup = shared.clone();
            tokio::spawn(async move {
                let _ = std::panic::AssertUnwindSafe(cleanup).catch_unwind().await;
                if let Some(broker) = broker.upgrade() {
                    broker
                        .inflight
                        .lock()
                        .expect("in-flight mutex poisoned")
                        .remove(&request_key);
                }
            });
        }
        shared.await
    }

    async fn call_uncached(&self, request_key: &str, request: Request) -> CallToolResult {
        let cache = self.cache.clone();
        let subject = request.subject().to_owned();
        let affinity = tokio::task::spawn_blocking(move || cache.affinity(&subject))
            .await
            .expect("cache read task panicked");
        let candidates = {
            let mut pool = self.pool.lock().await;
            let order = pool.order(affinity.as_deref(), SystemTime::now());
            order
                .into_iter()
                .map(|index| {
                    let account = &pool.accounts[index];
                    (index, account.name.clone(), account.api_key.clone())
                })
                .collect::<Vec<_>>()
        };

        let affinity_valid = affinity
            .as_deref()
            .is_some_and(|name| candidates.iter().any(|(_, candidate, _)| candidate == name));
        let lookup: Vec<_> = candidates
            .iter()
            .map(|(_, name, key)| (name.clone(), key.clone()))
            .collect();
        let cache = self.cache.clone();
        let cache_key = request_key.to_owned();
        let subject = request.subject().to_owned();
        if let Some(result) = tokio::task::spawn_blocking(move || {
            for (account, api_key) in lookup {
                if let Some(result) = cache.result(&cache_key, &api_key) {
                    if !affinity_valid && let Err(error) = cache.set_affinity(&subject, &account) {
                        eprintln!("warning: failed to restore affinity cache: {error}");
                    }
                    return Some(result);
                }
            }
            None
        })
        .await
        .expect("cache lookup task panicked")
        {
            return result;
        }

        let mut shared_failures = 0;
        let mut last_error = None;
        for (index, account_name, api_key) in candidates {
            if !self.pool.lock().await.available(index, SystemTime::now()) {
                continue;
            }
            let permit = self
                .semaphore
                .acquire()
                .await
                .expect("broker never closes its semaphore");
            if !self.pool.lock().await.available(index, SystemTime::now()) {
                continue;
            }
            let response = self.context7.call(&api_key, &request).await;
            drop(permit);
            match response {
                Ok(Upstream {
                    status,
                    result,
                    quota,
                    libraries,
                }) if status.is_success() => {
                    {
                        let mut pool = self.pool.lock().await;
                        pool.accounts[index].quota = quota;
                        pool.accounts[index].cooldown_until = None;
                    }
                    if result.is_error != Some(true) {
                        let cache = self.cache.clone();
                        let cache_key = request_key.to_owned();
                        let api_key = api_key.clone();
                        let cached_result = result.clone();
                        let libraries: Vec<_> = std::iter::once(request.subject().to_owned())
                            .chain(libraries)
                            .collect();
                        tokio::task::spawn_blocking(move || {
                            if let Err(error) =
                                cache.set_result(&cache_key, &api_key, &cached_result)
                            {
                                eprintln!("warning: failed to write result cache: {error}");
                            }
                            for library in libraries {
                                if let Err(error) = cache.set_affinity(&library, &account_name) {
                                    eprintln!("warning: failed to write affinity cache: {error}");
                                }
                            }
                        })
                        .await
                        .expect("cache write task panicked");
                    }
                    return result;
                }
                Ok(response)
                    if matches!(
                        response.status,
                        StatusCode::UNAUTHORIZED
                            | StatusCode::FORBIDDEN
                            | StatusCode::TOO_MANY_REQUESTS
                    ) =>
                {
                    last_error = Some(result_text(&response.result));
                    let mut pool = self.pool.lock().await;
                    let account = &mut pool.accounts[index];
                    account.quota = response.quota;
                    account.cooldown_until =
                        Some(if response.status == StatusCode::TOO_MANY_REQUESTS {
                            retry_deadline(account.quota.as_ref())
                        } else {
                            SystemTime::now() + self.cooldown
                        });
                }
                Ok(response)
                    if response.status == StatusCode::REQUEST_TIMEOUT
                        || response.status.as_u16() == 425
                        || response.status.is_server_error() =>
                {
                    last_error = Some(result_text(&response.result));
                    shared_failures += 1;
                    if shared_failures == 2 {
                        return broker_error(format!(
                            "Context7 request failed after two account attempts: {}",
                            last_error.as_deref().unwrap_or_default()
                        ));
                    }
                }
                Ok(response) => return response.result,
                Err(Error::Network(message)) => {
                    last_error = Some(message);
                    shared_failures += 1;
                    if shared_failures == 2 {
                        return broker_error(format!(
                            "Context7 request failed after two account attempts: {}",
                            last_error.as_deref().unwrap_or_default()
                        ));
                    }
                }
                Err(Error::Invalid(message)) => return broker_error(message),
            }
        }
        broker_error(
            last_error
                .unwrap_or_else(|| "No Context7 accounts are currently available.".to_owned()),
        )
    }
}

pub(crate) fn fingerprint(api_key: &str) -> String {
    sha256_hex(api_key.as_bytes())[..12].to_owned()
}

fn quota_usage(account: &Account, now: SystemTime) -> f64 {
    account
        .quota
        .as_ref()
        .filter(|quota| quota.limit > 0 && quota.reset_at > now)
        .map(|quota| (quota.limit - quota.remaining) as f64 / quota.limit as f64)
        .unwrap_or(-1.0)
}

fn result_text(result: &CallToolResult) -> String {
    result
        .content
        .first()
        .and_then(|content| content.as_text())
        .map(|text| text.text.clone())
        .unwrap_or_else(|| "Context7 request failed.".to_owned())
}

fn broker_error(message: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(format!(
        "[context7-account-broker] {}",
        message.into()
    ))])
}

fn retry_deadline(quota: Option<&Quota>) -> SystemTime {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let next_day = UNIX_EPOCH + Duration::from_secs((now / 86_400 + 1) * 86_400);
    quota
        .map(|quota| std::cmp::min(quota.reset_at, next_day))
        .unwrap_or(next_day)
}

#[derive(Clone)]
struct Cache {
    root: PathBuf,
    ttl: Duration,
}

impl Cache {
    fn new(root: impl Into<PathBuf>, ttl: Duration) -> io::Result<Self> {
        let root = root.into();
        ensure_private_directory(&root)?;
        let cache = Self { root, ttl };
        cache.cleanup()?;
        Ok(cache)
    }

    fn request_hash(request: &Request) -> String {
        sha256_hex(&serde_json::to_vec(&(1, request)).expect("requests serialize"))
    }

    fn result(&self, request: &str, api_key: &str) -> Option<CallToolResult> {
        let path = self.root.join(format!(
            "result-{request}-{}.json",
            sha256_hex(api_key.as_bytes())
        ));
        let bytes = self.read(&path)?;
        match serde_json::from_slice(&bytes) {
            Ok(result) => Some(result),
            Err(error) => {
                eprintln!(
                    "warning: removing corrupt cache file {}: {error}",
                    path.display()
                );
                remove_cache_file(&path);
                None
            }
        }
    }

    fn set_result(&self, request: &str, api_key: &str, result: &CallToolResult) -> io::Result<()> {
        write_private(
            &self.root.join(format!(
                "result-{request}-{}.json",
                sha256_hex(api_key.as_bytes())
            )),
            &serde_json::to_vec(result).map_err(io::Error::other)?,
        )
    }

    fn affinity(&self, library: &str) -> Option<String> {
        let path = self.affinity_path(library);
        let bytes = self.read(&path)?;
        match serde_json::from_slice(&bytes) {
            Ok(account) => Some(account),
            Err(error) => {
                eprintln!(
                    "warning: removing corrupt cache file {}: {error}",
                    path.display()
                );
                remove_cache_file(&path);
                None
            }
        }
    }

    fn set_affinity(&self, library: &str, account: &str) -> io::Result<()> {
        write_private(
            &self.affinity_path(library),
            &serde_json::to_vec(account).map_err(io::Error::other)?,
        )
    }

    fn affinity_path(&self, library: &str) -> PathBuf {
        self.root.join(format!(
            "affinity-{}.json",
            sha256_hex(library.to_lowercase().as_bytes())
        ))
    }

    fn read(&self, path: &Path) -> Option<Vec<u8>> {
        let metadata = match fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return None,
            Err(error) => {
                eprintln!(
                    "warning: cannot read cache metadata {}: {error}",
                    path.display()
                );
                return None;
            }
        };
        match metadata.modified().and_then(|modified| {
            modified
                .elapsed()
                .map_err(|error| io::Error::other(error.to_string()))
        }) {
            Ok(age) if age < self.ttl => {}
            Ok(_) => {
                remove_cache_file(path);
                return None;
            }
            Err(error) => {
                eprintln!(
                    "warning: invalid cache timestamp {}: {error}",
                    path.display()
                );
                remove_cache_file(path);
                return None;
            }
        }
        match fs::read(path) {
            Ok(bytes) => Some(bytes),
            Err(error) => {
                eprintln!(
                    "warning: cannot read cache file {}: {error}",
                    path.display()
                );
                None
            }
        }
    }

    fn cleanup(&self) -> io::Result<()> {
        for entry in fs::read_dir(&self.root)? {
            let path = entry?.path();
            if path.is_file() && broker_cache_name(&path) {
                let _ = self.read(&path);
            }
        }
        Ok(())
    }
}

fn broker_cache_name(path: &Path) -> bool {
    let Some(name) = path.file_stem().and_then(|name| name.to_str()) else {
        return false;
    };
    let hash =
        |value: &str| value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit());
    name.strip_prefix("affinity-").is_some_and(hash)
        || name
            .strip_prefix("result-")
            .and_then(|value| value.split_once('-'))
            .is_some_and(|(request, account)| hash(request) && hash(account))
}

fn remove_cache_file(path: &Path) {
    if !broker_cache_name(path) {
        return;
    }
    if let Err(error) = fs::remove_file(path)
        && error.kind() != io::ErrorKind::NotFound
    {
        eprintln!(
            "warning: cannot remove cache file {}: {error}",
            path.display()
        );
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context7::Upstream;
    use rmcp::model::ContentBlock;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    fn records(count: usize) -> Vec<AccountRecord> {
        (0..count)
            .map(|index| AccountRecord {
                name: format!("account-{index}"),
                api_key: format!("ctx7sk-{index}"),
            })
            .collect()
    }

    fn request(query: &str) -> Request {
        Request::Docs {
            query: query.to_owned(),
            library_id: "/x/y".to_owned(),
        }
    }

    fn success(text: &str) -> Upstream {
        Upstream {
            status: StatusCode::OK,
            result: CallToolResult::success(vec![ContentBlock::text(text)]),
            quota: None,
            libraries: Vec::new(),
        }
    }

    fn broker(context7: Context7, count: usize, path: &Path) -> Arc<Broker> {
        Broker::new(
            records(count),
            context7,
            path.join("cache"),
            Duration::from_secs(3600),
            Duration::from_secs(30),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn construction_is_offline_and_success_is_cached() {
        let directory = tempfile::tempdir().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let sender_calls = Arc::clone(&calls);
        let context7 = Context7::with_sender(move |_, _| {
            let calls = Arc::clone(&sender_calls);
            async move {
                calls.fetch_add(1, AtomicOrdering::SeqCst);
                Ok(success("docs"))
            }
        });
        let broker = broker(context7, 1, directory.path());
        assert_eq!(calls.load(AtomicOrdering::SeqCst), 0);
        assert_eq!(
            broker.call(request("q")).await.content[0]
                .as_text()
                .unwrap()
                .text,
            "docs"
        );
        let affinity = broker.cache.affinity_path("/x/y");
        fs::remove_file(&affinity).unwrap();
        while !broker.inflight.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
        broker.call(request("q")).await;
        assert_eq!(calls.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(broker.cache.affinity("/x/y").as_deref(), Some("account-0"));
    }

    #[tokio::test]
    async fn auth_fails_over_but_shared_failures_do_not_poison_accounts() {
        let directory = tempfile::tempdir().unwrap();
        let calls = Arc::new(StdMutex::new(Vec::new()));
        let sender_calls = Arc::clone(&calls);
        let context7 = Context7::with_sender(move |key, request| {
            let calls = Arc::clone(&sender_calls);
            async move {
                calls.lock().unwrap().push(key.clone());
                if key == "ctx7sk-0"
                    && matches!(request, Request::Docs { ref query, .. } if query == "auth")
                {
                    return Ok(Upstream {
                        status: StatusCode::UNAUTHORIZED,
                        result: CallToolResult::error(vec![ContentBlock::text("bad key")]),
                        quota: None,
                        libraries: Vec::new(),
                    });
                }
                if matches!(request, Request::Docs { ref query, .. } if query.starts_with("shared"))
                {
                    return Err(Error::Network("network down".to_owned()));
                }
                Ok(success("ok"))
            }
        });
        let broker = broker(context7, 3, directory.path());
        assert_eq!(
            broker.call(request("auth")).await.content[0]
                .as_text()
                .unwrap()
                .text,
            "ok"
        );
        broker.call(request("shared-1")).await;
        broker.call(request("shared-2")).await;
        let calls = calls.lock().unwrap();
        assert_eq!(calls.iter().filter(|key| *key == "ctx7sk-0").count(), 1);
        assert_eq!(calls.len(), 6);
    }

    #[tokio::test]
    async fn canceled_waiter_does_not_cancel_or_retain_the_producer() {
        let directory = tempfile::tempdir().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let sender_calls = Arc::clone(&calls);
        let context7 = Context7::with_sender(move |_, _| {
            let calls = Arc::clone(&sender_calls);
            async move {
                calls.fetch_add(1, AtomicOrdering::SeqCst);
                tokio::time::sleep(Duration::from_millis(25)).await;
                Ok(success("done"))
            }
        });
        let broker = broker(context7, 1, directory.path());
        let task_broker = Arc::clone(&broker);
        let task = tokio::spawn(async move { task_broker.call(request("q")).await });
        tokio::time::sleep(Duration::from_millis(5)).await;
        task.abort();
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert!(broker.inflight.lock().unwrap().is_empty());
        broker.call(request("q")).await;
        assert_eq!(calls.load(AtomicOrdering::SeqCst), 1);
    }

    #[test]
    fn pool_rotates_unknown_accounts_and_prefers_lower_usage() {
        let mut pool = Pool {
            accounts: records(3)
                .into_iter()
                .map(|record| Account {
                    name: record.name,
                    api_key: record.api_key,
                    quota: None,
                    cooldown_until: None,
                })
                .collect(),
            next: 0,
        };
        let now = SystemTime::now();
        assert_eq!(pool.order(None, now), [0, 1, 2]);
        assert_eq!(pool.order(None, now), [1, 2, 0]);
        pool.accounts[0].quota = Some(Quota {
            limit: 100,
            remaining: 10,
            reset_at: now + Duration::from_secs(60),
            blocked: false,
        });
        pool.accounts[1].quota = Some(Quota {
            limit: 100,
            remaining: 80,
            reset_at: now + Duration::from_secs(60),
            blocked: false,
        });
        pool.accounts[2].quota = Some(Quota {
            limit: 100,
            remaining: 50,
            reset_at: now + Duration::from_secs(60),
            blocked: false,
        });
        assert_eq!(pool.order(None, now), [1, 2, 0]);
        pool.accounts[0].quota.as_mut().unwrap().reset_at = now - Duration::from_secs(1);
        assert_eq!(quota_usage(&pool.accounts[0], now), -1.0);
    }

    #[tokio::test]
    async fn queued_call_rechecks_cooldowns_after_the_semaphore() {
        let directory = tempfile::tempdir().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let sender_calls = Arc::clone(&calls);
        let context7 = Context7::with_sender(move |_, _| {
            let calls = Arc::clone(&sender_calls);
            async move {
                calls.fetch_add(1, AtomicOrdering::SeqCst);
                Ok(success("unexpected"))
            }
        });
        let broker = broker(context7, 2, directory.path());
        let permits = broker.semaphore.acquire_many(4).await.unwrap();
        let task_broker = Arc::clone(&broker);
        let task = tokio::spawn(async move { task_broker.call(request("queued")).await });
        loop {
            if broker.pool.lock().await.next == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
        let mut pool = broker.pool.lock().await;
        for account in &mut pool.accounts {
            account.cooldown_until = Some(SystemTime::now() + Duration::from_secs(60));
        }
        drop(pool);
        drop(permits);
        assert_eq!(task.await.unwrap().is_error, Some(true));
        assert_eq!(calls.load(AtomicOrdering::SeqCst), 0);
    }

    #[test]
    fn cache_persists_and_uses_one_mtime_ttl() {
        let directory = tempfile::tempdir().unwrap();
        let cache = directory.path().join("cache");
        let result = CallToolResult::success(vec![ContentBlock::text("cached")]);
        let key = Cache::request_hash(&request("q"));
        Cache::new(&cache, Duration::from_secs(60))
            .unwrap()
            .set_result(&key, "ctx7sk-key", &result)
            .unwrap();
        assert_eq!(
            Cache::new(&cache, Duration::from_secs(60))
                .unwrap()
                .result(&key, "ctx7sk-key")
                .unwrap()
                .content[0]
                .as_text()
                .unwrap()
                .text,
            "cached"
        );
        assert!(
            Cache::new(&cache, Duration::ZERO)
                .unwrap()
                .result(&key, "ctx7sk-key")
                .is_none()
        );
    }

    #[test]
    fn cache_cleanup_leaves_expired_unrelated_files_alone() {
        let directory = tempfile::tempdir().unwrap();
        let cache = directory.path().join("cache");
        ensure_private_directory(&cache).unwrap();
        let unrelated = cache.join("unrelated.json");
        let misleading = cache.join("result-not-ours.json");
        fs::write(&unrelated, "leave me").unwrap();
        fs::write(&misleading, "leave me too").unwrap();
        Cache::new(cache, Duration::ZERO).unwrap();
        assert_eq!(fs::read_to_string(unrelated).unwrap(), "leave me");
        assert_eq!(fs::read_to_string(misleading).unwrap(), "leave me too");
    }

    #[test]
    fn quota_retry_uses_the_earlier_deadline() {
        let next_day = retry_deadline(None);
        let quota = Quota {
            limit: 1,
            remaining: 0,
            reset_at: next_day + Duration::from_secs(1),
            blocked: true,
        };
        assert_eq!(retry_deadline(Some(&quota)), next_day);
        let earlier = Quota {
            reset_at: next_day - Duration::from_secs(1),
            ..quota
        };
        assert_eq!(retry_deadline(Some(&earlier)), earlier.reset_at);
    }
}
