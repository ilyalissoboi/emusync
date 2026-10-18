//! In-memory remote for tests and simulations, with fault injection.

use crate::{PutOutcome, Remote, RemoteError, Result};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

#[derive(Debug, Clone)]
pub struct MemoryRemoteOptions {
    /// Enforce `If-None-Match: *`. When false the remote behaves like an S3-compatible service
    /// without conditional writes: the header is ignored and objects are overwritten.
    pub conditional_writes: bool,
    /// Newly written keys stay invisible to `list` for this many list calls (delayed-listing
    /// providers, DESIGN §6.2).
    pub list_delay: u64,
}

impl Default for MemoryRemoteOptions {
    fn default() -> Self {
        Self { conditional_writes: true, list_delay: 0 }
    }
}

struct Object {
    data: Vec<u8>,
    /// Value of `list_calls` at which the object becomes visible to `list`.
    visible_from: u64,
}

pub struct MemoryRemote {
    conditional_writes: bool,
    list_delay: AtomicU64,
    objects: Mutex<BTreeMap<String, Object>>,
    list_calls: AtomicU64,
    offline: AtomicBool,
    fail_puts_after: Mutex<Option<u64>>,
    lose_put_response: AtomicBool,
    /// Request counters, for tests that check how much traffic a cycle causes.
    pub puts: AtomicU64,
    pub gets: AtomicU64,
    pub heads: AtomicU64,
    pub lists: AtomicU64,
}

impl Default for MemoryRemote {
    fn default() -> Self {
        Self::new(MemoryRemoteOptions::default())
    }
}

impl MemoryRemote {
    pub fn new(opts: MemoryRemoteOptions) -> Self {
        Self {
            conditional_writes: opts.conditional_writes,
            list_delay: AtomicU64::new(opts.list_delay),
            objects: Mutex::new(BTreeMap::new()),
            list_calls: AtomicU64::new(0),
            offline: AtomicBool::new(false),
            fail_puts_after: Mutex::new(None),
            lose_put_response: AtomicBool::new(false),
            puts: AtomicU64::new(0),
            gets: AtomicU64::new(0),
            heads: AtomicU64::new(0),
            lists: AtomicU64::new(0),
        }
    }

    /// Changes the listing delay for objects written from now on.
    pub fn set_list_delay(&self, n: u64) {
        self.list_delay.store(n, Ordering::SeqCst);
    }

    /// Simulates a network partition: every request fails with [`RemoteError::Network`].
    pub fn set_offline(&self, offline: bool) {
        self.offline.store(offline, Ordering::SeqCst);
    }

    /// Lets `n` more puts succeed, then fails every put with a network error until cleared
    /// with `None`. Simulates a process killed or a connection dropped mid-upload.
    pub fn fail_puts_after(&self, n: Option<u64>) {
        *self.fail_puts_after.lock().unwrap() = n;
    }

    /// The next put is stored but reported as a network failure, like a connection that drops
    /// after the server committed the write.
    pub fn lose_next_put_response(&self) {
        self.lose_put_response.store(true, Ordering::SeqCst);
    }

    pub fn keys(&self) -> Vec<String> {
        self.objects.lock().unwrap().keys().cloned().collect()
    }

    pub fn raw_get(&self, key: &str) -> Option<Vec<u8>> {
        self.objects.lock().unwrap().get(key).map(|o| o.data.clone())
    }

    /// Overwrites an object behind the protocol's back (for corruption tests).
    pub fn raw_put(&self, key: &str, data: &[u8]) {
        self.objects.lock().unwrap().insert(key.to_string(), Object { data: data.to_vec(), visible_from: 0 });
    }

    fn check_online(&self) -> Result<()> {
        if self.offline.load(Ordering::SeqCst) {
            Err(RemoteError::Network("simulated partition".into()))
        } else {
            Ok(())
        }
    }
}

impl Remote for MemoryRemote {
    fn describe(&self) -> String {
        "memory://".into()
    }

    fn put(&self, key: &str, data: &[u8], if_none_match: bool) -> Result<PutOutcome> {
        self.check_online()?;
        {
            let mut budget = self.fail_puts_after.lock().unwrap();
            if let Some(n) = budget.as_mut() {
                if *n == 0 {
                    return Err(RemoteError::Network("simulated failure during put".into()));
                }
                *n -= 1;
            }
        }
        self.puts.fetch_add(1, Ordering::SeqCst);
        let mut objects = self.objects.lock().unwrap();
        if if_none_match && self.conditional_writes && objects.contains_key(key) {
            return Ok(PutOutcome::AlreadyExists);
        }
        let visible_from = self.list_calls.load(Ordering::SeqCst) + self.list_delay.load(Ordering::SeqCst);
        objects.insert(key.to_string(), Object { data: data.to_vec(), visible_from });
        if self.lose_put_response.swap(false, Ordering::SeqCst) {
            return Err(RemoteError::Network("simulated lost response".into()));
        }
        Ok(PutOutcome::Created)
    }

    fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        self.check_online()?;
        self.gets.fetch_add(1, Ordering::SeqCst);
        Ok(self.objects.lock().unwrap().get(key).map(|o| o.data.clone()))
    }

    fn head(&self, key: &str) -> Result<bool> {
        self.check_online()?;
        self.heads.fetch_add(1, Ordering::SeqCst);
        Ok(self.objects.lock().unwrap().contains_key(key))
    }

    fn list(&self, prefix: &str, start_after: Option<&str>) -> Result<Vec<String>> {
        self.check_online()?;
        self.lists.fetch_add(1, Ordering::SeqCst);
        let now = self.list_calls.fetch_add(1, Ordering::SeqCst);
        let objects = self.objects.lock().unwrap();
        Ok(objects
            .range(prefix.to_string()..)
            .take_while(|(k, _)| k.starts_with(prefix))
            .filter(|(k, o)| o.visible_from <= now && start_after.map_or(true, |s| k.as_str() > s))
            .map(|(k, _)| k.clone())
            .collect())
    }

    fn list_dirs(&self, prefix: &str) -> Result<Vec<String>> {
        self.check_online()?;
        self.lists.fetch_add(1, Ordering::SeqCst);
        let objects = self.objects.lock().unwrap();
        let mut dirs: Vec<String> = objects
            .keys()
            .filter_map(|k| k.strip_prefix(prefix))
            .filter_map(|rest| rest.split_once('/').map(|(d, _)| d.to_string()))
            .collect();
        dirs.dedup();
        Ok(dirs)
    }
}
