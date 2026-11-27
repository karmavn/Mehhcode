//! Priority-ranked failover router, cooldown tracker, and single-flight search memo.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use mehh_ai::http_safety::DnsPolicy;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::native::{search_mehh, NativeSearchContext, NativeSearchState};
use super::providers::{execute_extract, execute_search, WebEndpoints};
use super::types::{ExtractedPage, ProviderError, ProviderId, SearchHit, WebProviderChoice};
use super::UrlPolicyChecker;

pub const SEARCH_MEMO_TTL: Duration = Duration::from_secs(15 * 60);
pub const SEARCH_MEMO_MAX_ENTRIES: usize = 128;
const SINGLE_FLIGHT_WORKER_TIMEOUT: Duration = Duration::from_secs(120);
const PER_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(7);

pub type ProgressFn = dyn Fn(&str) + Send + Sync;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchOutput {
    pub provider: ProviderId,
    pub hits: Vec<SearchHit>,
    pub cached: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractOutput {
    pub provider: ProviderId,
    pub page: ExtractedPage,
}

#[async_trait]
pub(crate) trait ProviderExecutor: Send + Sync {
    async fn search(
        &self,
        provider: ProviderId,
        query: &str,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<Vec<SearchHit>, ProviderError>;

    async fn extract(
        &self,
        provider: ProviderId,
        url: &str,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<ExtractedPage, ProviderError>;
}

struct HttpProviderExecutor {
    endpoints: WebEndpoints,
    client: reqwest::Client,
    enrich_client: reqwest::Client,
    enrich_dns_policy: DnsPolicy,
    url_policy_checker: Option<Arc<dyn UrlPolicyChecker>>,
    native_state: NativeSearchState,
    session_id: String,
}

#[async_trait]
impl ProviderExecutor for HttpProviderExecutor {
    async fn search(
        &self,
        provider: ProviderId,
        query: &str,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<Vec<SearchHit>, ProviderError> {
        if provider == ProviderId::Mehh {
            let ctx = NativeSearchContext {
                client: &self.client,
                enrich_client: &self.enrich_client,
                enrich_dns_policy: self.enrich_dns_policy,
                url_policy_checker: self.url_policy_checker.as_ref(),
                endpoints: &self.endpoints.native,
                state: Some(&self.native_state),
            };
            return search_mehh(&ctx, query, timeout, cancel).await;
        }
        execute_search(
            &self.client,
            &self.endpoints,
            &self.session_id,
            provider,
            query,
            timeout,
            cancel,
        )
        .await
    }

    async fn extract(
        &self,
        provider: ProviderId,
        url: &str,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<ExtractedPage, ProviderError> {
        execute_extract(
            &self.client,
            &self.endpoints,
            &self.session_id,
            provider,
            url,
            timeout,
            cancel,
        )
        .await
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SearchMemoKey {
    choice: WebProviderChoice,
    normalized_query: String,
}

#[derive(Debug, Clone)]
struct MemoEntry {
    output: SearchOutput,
    inserted_at: Instant,
}

struct RouterState {
    cooldowns: HashMap<ProviderId, Instant>,
    memo: HashMap<SearchMemoKey, MemoEntry>,
}

struct InFlightSlot {
    rx: watch::Receiver<Option<Result<SearchOutput, ProviderError>>>,
    progress_rx: watch::Receiver<String>,
    waiters: AtomicUsize,
    worker_cancel: CancellationToken,
}

/// Thread-safe router coordinating quality-ranked provider priority, cooldown failover,
/// search memoization, and single-flight request deduplication.
pub struct WebRouter {
    executor: Arc<dyn ProviderExecutor>,
    state: Arc<Mutex<RouterState>>,
    in_flight: Arc<Mutex<HashMap<SearchMemoKey, Arc<InFlightSlot>>>>,
    per_attempt_timeout: Duration,
}

impl WebRouter {
    pub fn new(endpoints: WebEndpoints, client: reqwest::Client) -> Self {
        Self::with_native_config(
            endpoints,
            client.clone(),
            client,
            DnsPolicy::PublicOnly,
            None,
        )
    }

    pub fn with_native_config(
        endpoints: WebEndpoints,
        client: reqwest::Client,
        enrich_client: reqwest::Client,
        enrich_dns_policy: DnsPolicy,
        url_policy_checker: Option<Arc<dyn UrlPolicyChecker>>,
    ) -> Self {
        let session_id = Uuid::new_v4().simple().to_string();
        let executor: Arc<dyn ProviderExecutor> = Arc::new(HttpProviderExecutor {
            endpoints,
            client,
            enrich_client,
            enrich_dns_policy,
            url_policy_checker,
            native_state: NativeSearchState::new(),
            session_id,
        });
        Self::from_executor(executor)
    }

    pub(crate) fn from_executor(executor: Arc<dyn ProviderExecutor>) -> Self {
        Self {
            executor,
            state: Arc::new(Mutex::new(RouterState {
                cooldowns: HashMap::new(),
                memo: HashMap::new(),
            })),
            in_flight: Arc::new(Mutex::new(HashMap::new())),
            per_attempt_timeout: PER_ATTEMPT_TIMEOUT,
        }
    }

    #[cfg(test)]
    fn with_per_attempt_timeout(mut self, timeout: Duration) -> Self {
        self.per_attempt_timeout = timeout;
        self
    }

    pub async fn search(
        &self,
        choice: WebProviderChoice,
        query: &str,
        timeout: Duration,
        cancel: &CancellationToken,
        on_progress: Option<&ProgressFn>,
    ) -> Result<SearchOutput, ProviderError> {
        if cancel.is_cancelled() {
            return Err(ProviderError::Cancelled);
        }
        if choice == WebProviderChoice::Disabled {
            return Err(ProviderError::BadRequest(
                "Web search is disabled by settings".to_string(),
            ));
        }

        let key = SearchMemoKey {
            choice,
            normalized_query: normalize_query(query),
        };

        if let Some(cached) = self.lookup_memo(&key) {
            return Ok(SearchOutput {
                provider: cached.provider,
                hits: cached.hits,
                cached: true,
            });
        }

        let slot = self.acquire_or_join_in_flight(key.clone(), query.to_string());
        let _guard = WaiterGuard {
            key,
            slot: slot.clone(),
            in_flight: self.in_flight.clone(),
        };

        let mut rx = slot.rx.clone();
        let mut progress_rx = slot.progress_rx.clone();
        let mut progress_open = true;
        let mut last_progress = String::new();
        let caller_deadline = tokio::time::Instant::now() + timeout;
        let sleep = tokio::time::sleep_until(caller_deadline);
        tokio::pin!(sleep);
        loop {
            emit_progress_if_new(&mut progress_rx, &mut last_progress, on_progress);
            if let Some(result) = rx.borrow_and_update().clone() {
                return result;
            }
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
                _ = &mut sleep => {
                    return Err(ProviderError::VendorUnavailable {
                        status: None,
                        detail: "Web search timed out".to_string(),
                    });
                }
                changed = progress_rx.changed(), if progress_open => {
                    emit_progress_if_new(&mut progress_rx, &mut last_progress, on_progress);
                    if changed.is_err() {
                        progress_open = false;
                    }
                }
                changed = rx.changed() => {
                    if changed.is_err() {
                        emit_progress_if_new(&mut progress_rx, &mut last_progress, on_progress);
                        if let Some(result) = rx.borrow().clone() {
                            return result;
                        }
                        return Err(ProviderError::VendorUnavailable {
                            status: None,
                            detail: "In-flight search task terminated unexpectedly".to_string(),
                        });
                    }
                }
            }
        }
    }

    pub async fn extract_fallback(
        &self,
        choice: WebProviderChoice,
        url: &str,
        timeout: Duration,
        cancel: &CancellationToken,
        on_progress: Option<&ProgressFn>,
    ) -> Result<ExtractOutput, ProviderError> {
        if cancel.is_cancelled() {
            return Err(ProviderError::Cancelled);
        }
        if choice == WebProviderChoice::Disabled {
            return Err(ProviderError::BadRequest(
                "Remote web extraction is disabled by settings".to_string(),
            ));
        }

        let candidates = self.plan_extract_candidates(choice);
        let mut attempted: Vec<ProviderId> = Vec::new();
        let mut last_err: Option<ProviderError> = None;
        let deadline = Instant::now() + timeout;

        for (idx, provider) in candidates.iter().copied().enumerate() {
            if cancel.is_cancelled() {
                return Err(ProviderError::Cancelled);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                if last_err.is_none() {
                    last_err = Some(ProviderError::VendorUnavailable {
                        status: None,
                        detail: "Remote web extraction timed out".to_string(),
                    });
                }
                break;
            }
            if let Some(cb) = on_progress {
                if idx == 0 {
                    cb(&format!("Extracting via {}...", provider.display_name()));
                } else {
                    let prev = candidates[idx - 1].display_name();
                    cb(&format!(
                        "{prev} unavailable, retrying via {}...",
                        provider.display_name()
                    ));
                }
            }
            attempted.push(provider);
            let attempt_timeout = remaining.min(self.per_attempt_timeout);
            match self.executor.extract(provider, url, attempt_timeout, cancel).await {
                Ok(page) => {
                    self.record_extract_success(provider);
                    return Ok(ExtractOutput { provider, page });
                }
                Err(err) => {
                    if !err.should_failover() {
                        return Err(err);
                    }
                    self.record_cooldown(provider, &err);
                    last_err = Some(err);
                }
            }
        }

        Err(annotate_attempted_error(
            last_err.unwrap_or(ProviderError::VendorUnavailable {
                status: None,
                detail: "No extract provider available".to_string(),
            }),
            &attempted,
        ))
    }

    fn lookup_memo(&self, key: &SearchMemoKey) -> Option<SearchOutput> {
        let now = Instant::now();
        let mut state = mehh_ai::lock_recover(&self.state);
        if let Some(entry) = state.memo.get(key) {
            if now.duration_since(entry.inserted_at) < SEARCH_MEMO_TTL {
                return Some(entry.output.clone());
            }
            state.memo.remove(key);
        }
        None
    }

    fn acquire_or_join_in_flight(
        &self,
        key: SearchMemoKey,
        query: String,
    ) -> Arc<InFlightSlot> {
        let mut map = mehh_ai::lock_recover(&self.in_flight);
        if let Some(existing) = map.get(&key) {
            if !existing.worker_cancel.is_cancelled() {
                existing.waiters.fetch_add(1, Ordering::AcqRel);
                return existing.clone();
            }
        }

        let (tx, rx) = watch::channel(None);
        let (progress_tx, progress_rx) = watch::channel(String::new());
        let worker_cancel = CancellationToken::new();
        let slot = Arc::new(InFlightSlot {
            rx,
            progress_rx,
            waiters: AtomicUsize::new(1),
            worker_cancel: worker_cancel.clone(),
        });
        map.insert(key.clone(), slot.clone());
        drop(map);

        let executor = self.executor.clone();
        let state = self.state.clone();
        let in_flight = self.in_flight.clone();
        let slot_for_worker = slot.clone();
        let per_attempt_timeout = self.per_attempt_timeout;

        tokio::spawn(async move {
            let cleanup = WorkerCleanupGuard {
                key: key.clone(),
                slot: slot_for_worker,
                in_flight,
            };
            let result = execute_search_with_failover(
                executor,
                state.clone(),
                key.choice,
                &query,
                SearchTimeouts {
                    total: SINGLE_FLIGHT_WORKER_TIMEOUT,
                    per_attempt: per_attempt_timeout,
                },
                &worker_cancel,
                &progress_tx,
            )
            .await;

            if let Ok(ref output) = result {
                store_memo(&state, key, output.clone());
            }
            drop(cleanup);
            let _ = tx.send(Some(result));
        });

        slot
    }

    fn plan_extract_candidates(&self, choice: WebProviderChoice) -> Vec<ProviderId> {
        let now = Instant::now();
        let mut state = mehh_ai::lock_recover(&self.state);
        state.cooldowns.retain(|_, until| *until > now);

        match choice {
            WebProviderChoice::Fixed(id) if id.supports_extract() => {
                let mut ordered = Vec::with_capacity(ProviderId::ALL_EXTRACT.len());
                if !state.cooldowns.contains_key(&id) {
                    ordered.push(id);
                }
                for p in ProviderId::ALL_EXTRACT {
                    if p != id && !state.cooldowns.contains_key(&p) {
                        ordered.push(p);
                    }
                }
                if ordered.is_empty() {
                    let earliest = ProviderId::ALL_EXTRACT
                        .iter()
                        .copied()
                        .min_by_key(|p| state.cooldowns.get(p).copied())
                        .unwrap_or(id);
                    vec![earliest]
                } else {
                    ordered
                }
            }
            _ => prioritized_candidates(&ProviderId::ALL_EXTRACT, &state.cooldowns),
        }
    }

    fn record_extract_success(&self, provider: ProviderId) {
        let mut state = mehh_ai::lock_recover(&self.state);
        state.cooldowns.remove(&provider);
    }

    fn record_cooldown(&self, provider: ProviderId, err: &ProviderError) {
        if let Some(dur) = err.cooldown_duration() {
            let mut state = mehh_ai::lock_recover(&self.state);
            state.cooldowns.insert(provider, Instant::now() + dur);
        }
    }
}

fn emit_progress_if_new(
    prx: &mut watch::Receiver<String>,
    last: &mut String,
    on_progress: Option<&ProgressFn>,
) {
    let borrowed = prx.borrow_and_update();
    if !borrowed.is_empty() && borrowed.as_str() != last.as_str() {
        last.clear();
        last.push_str(&borrowed);
        drop(borrowed);
        if let Some(cb) = on_progress {
            cb(last);
        }
    }
}

struct WaiterGuard {
    key: SearchMemoKey,
    slot: Arc<InFlightSlot>,
    in_flight: Arc<Mutex<HashMap<SearchMemoKey, Arc<InFlightSlot>>>>,
}

impl Drop for WaiterGuard {
    fn drop(&mut self) {
        let mut map = mehh_ai::lock_recover(&self.in_flight);
        let prev = self.slot.waiters.fetch_sub(1, Ordering::AcqRel);
        if prev == 1 {
            self.slot.worker_cancel.cancel();
            if map
                .get(&self.key)
                .is_some_and(|entry| Arc::ptr_eq(entry, &self.slot))
            {
                map.remove(&self.key);
            }
        }
    }
}

struct WorkerCleanupGuard {
    key: SearchMemoKey,
    slot: Arc<InFlightSlot>,
    in_flight: Arc<Mutex<HashMap<SearchMemoKey, Arc<InFlightSlot>>>>,
}

impl Drop for WorkerCleanupGuard {
    fn drop(&mut self) {
        let mut map = mehh_ai::lock_recover(&self.in_flight);
        if map
            .get(&self.key)
            .is_some_and(|entry| Arc::ptr_eq(entry, &self.slot))
        {
            map.remove(&self.key);
        }
    }
}

#[derive(Clone, Copy)]
struct SearchTimeouts {
    total: Duration,
    per_attempt: Duration,
}

async fn execute_search_with_failover(
    executor: Arc<dyn ProviderExecutor>,
    state: Arc<Mutex<RouterState>>,
    choice: WebProviderChoice,
    query: &str,
    timeouts: SearchTimeouts,
    cancel: &CancellationToken,
    progress_tx: &watch::Sender<String>,
) -> Result<SearchOutput, ProviderError> {
    let candidates = match choice {
        WebProviderChoice::Disabled => {
            return Err(ProviderError::BadRequest(
                "Web search is disabled by settings".to_string(),
            ));
        }
        WebProviderChoice::Fixed(id) => vec![id],
        WebProviderChoice::Auto => {
            let now = Instant::now();
            let mut guard = mehh_ai::lock_recover(&state);
            guard.cooldowns.retain(|_, until| *until > now);
            prioritized_candidates(&ProviderId::ALL_SEARCH, &guard.cooldowns)
        }
    };

    let mut attempted: Vec<ProviderId> = Vec::new();
    let mut last_err: Option<ProviderError> = None;
    let deadline = Instant::now() + timeouts.total;

    for (idx, provider) in candidates.iter().copied().enumerate() {
        if cancel.is_cancelled() {
            return Err(ProviderError::Cancelled);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            if last_err.is_none() {
                last_err = Some(ProviderError::VendorUnavailable {
                    status: None,
                    detail: "Web search timed out".to_string(),
                });
            }
            break;
        }
        let msg = if idx == 0 {
            format!("Searching via {}...", provider.display_name())
        } else {
            let prev = candidates[idx - 1].display_name();
            format!("{prev} unavailable, retrying via {}...", provider.display_name())
        };
        let _ = progress_tx.send(msg);

        attempted.push(provider);
        let attempt_timeout = remaining.min(timeouts.per_attempt);
        match executor.search(provider, query, attempt_timeout, cancel).await {
            Ok(hits) => {
                let mut guard = mehh_ai::lock_recover(&state);
                guard.cooldowns.remove(&provider);
                return Ok(SearchOutput {
                    provider,
                    hits,
                    cached: false,
                });
            }
            Err(err) => {
                if !err.should_failover() {
                    return Err(err);
                }
                if let Some(dur) = err.cooldown_duration() {
                    let mut guard = mehh_ai::lock_recover(&state);
                    guard.cooldowns.insert(provider, Instant::now() + dur);
                }
                last_err = Some(err);
            }
        }
    }

    let final_err = last_err.unwrap_or(ProviderError::VendorUnavailable {
        status: None,
        detail: "No search provider available".to_string(),
    });
    if candidates.len() <= 1 {
        Err(final_err)
    } else {
        Err(annotate_attempted_error(final_err, &attempted))
    }
}

fn prioritized_candidates(
    all: &[ProviderId],
    cooldowns: &HashMap<ProviderId, Instant>,
) -> Vec<ProviderId> {
    let healthy: Vec<ProviderId> = all
        .iter()
        .copied()
        .filter(|p| !cooldowns.contains_key(p))
        .collect();
    if !healthy.is_empty() {
        return healthy;
    }

    // All candidates are cooling down: attempt the earliest-expiring provider once.
    let earliest = all
        .iter()
        .copied()
        .min_by_key(|p| cooldowns.get(p).copied())
        .unwrap_or(all[0]);
    vec![earliest]
}

fn annotate_attempted_error(err: ProviderError, attempted: &[ProviderId]) -> ProviderError {
    if attempted.is_empty() {
        return err;
    }
    let names: Vec<&str> = attempted.iter().map(|p| p.as_str()).collect();
    let suffix = format!(" (attempted: {})", names.join(", "));
    match err {
        ProviderError::RateLimited {
            retry_after,
            detail,
        } => ProviderError::RateLimited {
            retry_after,
            detail: format!("{detail}{suffix}"),
        },
        ProviderError::VendorUnavailable { status, detail } => ProviderError::VendorUnavailable {
            status,
            detail: format!("{detail}{suffix}"),
        },
        other => other,
    }
}

fn store_memo(state: &Arc<Mutex<RouterState>>, key: SearchMemoKey, output: SearchOutput) {
    let now = Instant::now();
    let mut guard = mehh_ai::lock_recover(state);
    guard
        .memo
        .retain(|_, entry| now.duration_since(entry.inserted_at) < SEARCH_MEMO_TTL);
    if !guard.memo.contains_key(&key) && guard.memo.len() >= SEARCH_MEMO_MAX_ENTRIES {
        if let Some(oldest_key) = guard
            .memo
            .iter()
            .min_by_key(|(_, entry)| entry.inserted_at)
            .map(|(k, _)| k.clone())
        {
            guard.memo.remove(&oldest_key);
        }
    }
    guard.memo.insert(
        key,
        MemoEntry {
            output,
            inserted_at: now,
        },
    );
}

fn normalize_query(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for word in raw.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        if word.bytes().all(|b| !b.is_ascii_uppercase()) && word.is_ascii() {
            out.push_str(word);
        } else {
            for ch in word.chars() {
                for lc in ch.to_lowercase() {
                    out.push(lc);
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    type SearchBehavior =
        Box<dyn Fn(ProviderId, &str, CancellationToken) -> Result<Vec<SearchHit>, ProviderError> + Send + Sync>;
    type ExtractBehavior =
        Box<dyn Fn(ProviderId, &str, CancellationToken) -> Result<ExtractedPage, ProviderError> + Send + Sync>;

    struct StubExecutor {
        calls: Mutex<Vec<ProviderId>>,
        recorded_timeouts: Mutex<Vec<Duration>>,
        search_delay: Duration,
        on_search: Option<SearchBehavior>,
        on_extract: Option<ExtractBehavior>,
    }

    impl StubExecutor {
        fn new_search(
            delay: Duration,
            f: impl Fn(ProviderId, &str, CancellationToken) -> Result<Vec<SearchHit>, ProviderError>
                + Send
                + Sync
                + 'static,
        ) -> Arc<Self> {
            Arc::new(Self {
                calls: Mutex::new(Vec::new()),
                recorded_timeouts: Mutex::new(Vec::new()),
                search_delay: delay,
                on_search: Some(Box::new(f)),
                on_extract: None,
            })
        }

        fn new_extract(
            f: impl Fn(ProviderId, &str, CancellationToken) -> Result<ExtractedPage, ProviderError>
                + Send
                + Sync
                + 'static,
        ) -> Arc<Self> {
            Self::new_extract_with_delay(Duration::ZERO, f)
        }

        fn new_extract_with_delay(
            delay: Duration,
            f: impl Fn(ProviderId, &str, CancellationToken) -> Result<ExtractedPage, ProviderError>
                + Send
                + Sync
                + 'static,
        ) -> Arc<Self> {
            Arc::new(Self {
                calls: Mutex::new(Vec::new()),
                recorded_timeouts: Mutex::new(Vec::new()),
                search_delay: delay,
                on_search: None,
                on_extract: Some(Box::new(f)),
            })
        }
    }

    #[async_trait]
    impl ProviderExecutor for StubExecutor {
        async fn search(
            &self,
            provider: ProviderId,
            query: &str,
            timeout: Duration,
            cancel: &CancellationToken,
        ) -> Result<Vec<SearchHit>, ProviderError> {
            self.calls.lock().unwrap().push(provider);
            self.recorded_timeouts.lock().unwrap().push(timeout);
            if !self.search_delay.is_zero() {
                tokio::select! {
                    _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
                    _ = tokio::time::sleep(timeout) => {
                        return Err(ProviderError::VendorUnavailable {
                            status: None,
                            detail: "stub search timeout".into(),
                        });
                    }
                    _ = tokio::time::sleep(self.search_delay) => {}
                }
            }
            self.on_search.as_ref().unwrap()(provider, query, cancel.clone())
        }

        async fn extract(
            &self,
            provider: ProviderId,
            url: &str,
            timeout: Duration,
            cancel: &CancellationToken,
        ) -> Result<ExtractedPage, ProviderError> {
            self.calls.lock().unwrap().push(provider);
            self.recorded_timeouts.lock().unwrap().push(timeout);
            if !self.search_delay.is_zero() {
                tokio::select! {
                    _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
                    _ = tokio::time::sleep(timeout) => {
                        return Err(ProviderError::VendorUnavailable {
                            status: None,
                            detail: "stub extract timeout".into(),
                        });
                    }
                    _ = tokio::time::sleep(self.search_delay) => {}
                }
            }
            self.on_extract.as_ref().unwrap()(provider, url, cancel.clone())
        }
    }

    #[tokio::test]
    async fn test_auto_search_follows_priority_and_recovers_after_cooldown() {
        let failing_provider = Arc::new(Mutex::new(None::<ProviderId>));
        let fail_clone = failing_provider.clone();
        let exec = StubExecutor::new_search(Duration::ZERO, move |p, _q, _| {
            if *fail_clone.lock().unwrap() == Some(p) {
                return Err(ProviderError::RateLimited {
                    retry_after: Some(Duration::from_secs(60)),
                    detail: "429".into(),
                });
            }
            Ok(vec![SearchHit {
                url: format!("https://{}.example", p.as_str()),
                title: None,
                published: None,
                content: None,
            }])
        });
        let router = WebRouter::from_executor(exec.clone());
        let cancel = CancellationToken::new();

        // Priority #1 (Mehh) always runs first.
        let r1 = router
            .search(WebProviderChoice::Auto, "query one", Duration::from_secs(5), &cancel, None)
            .await
            .unwrap();
        assert_eq!(r1.provider, ProviderId::ALL_SEARCH[0]);

        // Fail #1 -> immediately falls back to #2 (Exa) and places #1 on cooldown.
        *failing_provider.lock().unwrap() = Some(ProviderId::ALL_SEARCH[0]);
        let r2 = router
            .search(WebProviderChoice::Auto, "query two", Duration::from_secs(5), &cancel, None)
            .await
            .unwrap();
        assert_eq!(r2.provider, ProviderId::ALL_SEARCH[1]);

        // While #1 is cooled down, #2 is tried directly with a single call.
        let calls_before = exec.calls.lock().unwrap().len();
        let r3 = router
            .search(WebProviderChoice::Auto, "query three", Duration::from_secs(5), &cancel, None)
            .await
            .unwrap();
        assert_eq!(r3.provider, ProviderId::ALL_SEARCH[1]);
        assert_eq!(exec.calls.lock().unwrap().len(), calls_before + 1);

        // Once #1's cooldown expires, Auto returns to priority #1 (no sticky trap on fallback).
        *failing_provider.lock().unwrap() = None;
        router
            .state
            .lock()
            .unwrap()
            .cooldowns
            .insert(ProviderId::ALL_SEARCH[0], Instant::now() - Duration::from_secs(1));
        let r4 = router
            .search(WebProviderChoice::Auto, "query four", Duration::from_secs(5), &cancel, None)
            .await
            .unwrap();
        assert_eq!(r4.provider, ProviderId::ALL_SEARCH[0]);
    }

    #[tokio::test]
    async fn test_auto_search_rotates_on_401_403_5xx_and_stops_on_bad_request() {
        let exec_rotate = StubExecutor::new_search(Duration::ZERO, |p, _q, _| match p {
            ProviderId::Mehh => Err(ProviderError::VendorUnavailable {
                status: Some(403),
                detail: "403".into(),
            }),
            _ => Ok(vec![]),
        });
        let router = WebRouter::from_executor(exec_rotate);
        let out = router
            .search(
                WebProviderChoice::Auto,
                "rotate",
                Duration::from_secs(5),
                &CancellationToken::new(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(out.provider, ProviderId::Exa);
        assert!(router
            .state
            .lock()
            .unwrap()
            .cooldowns
            .contains_key(&ProviderId::Mehh));

        let exec_bad = StubExecutor::new_search(Duration::ZERO, |_p, _q, _| {
            Err(ProviderError::BadRequest("invalid syntax".into()))
        });
        let router_bad = WebRouter::from_executor(exec_bad.clone());
        let err = router_bad
            .search(
                WebProviderChoice::Auto,
                "bad",
                Duration::from_secs(5),
                &CancellationToken::new(),
                None,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ProviderError::BadRequest(_)));
        assert_eq!(exec_bad.calls.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn test_fixed_search_never_fails_over() {
        let exec = StubExecutor::new_search(Duration::ZERO, |_, _, _| Err(ProviderError::RateLimited {
            retry_after: Some(Duration::from_secs(30)),
            detail: "429".into(),
        }));
        let router = WebRouter::from_executor(exec.clone());
        let err = router
            .search(
                WebProviderChoice::Fixed(ProviderId::Tavily),
                "fixed",
                Duration::from_secs(5),
                &CancellationToken::new(),
                None,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ProviderError::RateLimited { .. }));
        assert_eq!(*exec.calls.lock().unwrap(), vec![ProviderId::Tavily]);
    }

    #[tokio::test]
    async fn test_extract_fallback_rotates_and_prioritizes_fixed_extractor() {
        let exec = StubExecutor::new_extract(|p, url, _| {
            if p == ProviderId::Exa {
                return Err(ProviderError::RateLimited {
                    retry_after: None,
                    detail: "busy".into(),
                });
            }
            Ok(ExtractedPage {
                url: url.to_string(),
                final_url: None,
                title: None,
                content: p.as_str().to_string(),
            })
        });
        let router = WebRouter::from_executor(exec.clone());
        let out = router
            .extract_fallback(
                WebProviderChoice::Fixed(ProviderId::Exa),
                "https://docs.rs",
                Duration::from_secs(5),
                &CancellationToken::new(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(exec.calls.lock().unwrap()[0], ProviderId::Exa);
        assert_eq!(out.provider, ProviderId::Parallel);

        // Search-only pin still rotates across extractors.
        let exec_tavily = StubExecutor::new_extract(|p, url, _| {
            Ok(ExtractedPage {
                url: url.to_string(),
                final_url: None,
                title: None,
                content: p.as_str().to_string(),
            })
        });
        let r_tavily = WebRouter::from_executor(exec_tavily.clone());
        let out2 = r_tavily
            .extract_fallback(
                WebProviderChoice::Fixed(ProviderId::Tavily),
                "https://docs.rs",
                Duration::from_secs(5),
                &CancellationToken::new(),
                None,
            )
            .await
            .unwrap();
        assert!(out2.provider.supports_extract());

        // Disabled blocks remote fallback.
        let err_dis = r_tavily
            .extract_fallback(
                WebProviderChoice::Disabled,
                "https://docs.rs",
                Duration::from_secs(5),
                &CancellationToken::new(),
                None,
            )
            .await
            .unwrap_err();
        assert!(matches!(err_dis, ProviderError::BadRequest(_)));

        // OriginBlocked stops immediately without rotating.
        let exec_origin = StubExecutor::new_extract(|_, _, _| {
            Err(ProviderError::OriginBlocked {
                status: 403,
                detail: "target forbidden".into(),
            })
        });
        let r_origin = WebRouter::from_executor(exec_origin.clone());
        let err_origin = r_origin
            .extract_fallback(
                WebProviderChoice::Auto,
                "https://docs.rs",
                Duration::from_secs(5),
                &CancellationToken::new(),
                None,
            )
            .await
            .unwrap_err();
        assert!(matches!(
            err_origin,
            ProviderError::OriginBlocked { status: 403, .. }
        ));
        assert_eq!(exec_origin.calls.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn test_search_memo_normalizes_query_and_evicts_at_128() {
        let exec = StubExecutor::new_search(Duration::ZERO, |_p, q, _| {
            if q.contains("fail") {
                return Err(ProviderError::VendorUnavailable {
                    status: Some(500),
                    detail: "err".into(),
                });
            }
            Ok(vec![SearchHit {
                url: "https://docs.rs".into(),
                title: None,
                published: None,
                content: None,
            }])
        });
        let router = WebRouter::from_executor(exec.clone());
        let cancel = CancellationToken::new();

        let first = router
            .search(
                WebProviderChoice::Fixed(ProviderId::Exa),
                "  Rust   Reqwest ",
                Duration::from_secs(5),
                &cancel,
                None,
            )
            .await
            .unwrap();
        assert!(!first.cached);

        let second = router
            .search(
                WebProviderChoice::Fixed(ProviderId::Exa),
                "rust reqwest",
                Duration::from_secs(5),
                &cancel,
                None,
            )
            .await
            .unwrap();
        assert!(second.cached);
        assert_eq!(exec.calls.lock().unwrap().len(), 1);

        // Errors are never cached.
        assert!(router
            .search(
                WebProviderChoice::Fixed(ProviderId::Exa),
                "fail query",
                Duration::from_secs(5),
                &cancel,
                None,
            )
            .await
            .is_err());

        for i in 0..129 {
            router
                .search(
                    WebProviderChoice::Fixed(ProviderId::Exa),
                    &format!("unique query {i}"),
                    Duration::from_secs(5),
                    &cancel,
                    None,
                )
                .await
                .unwrap();
        }
        assert!(router.state.lock().unwrap().memo.len() <= SEARCH_MEMO_MAX_ENTRIES);
    }

    #[tokio::test]
    async fn test_single_flight_deduplicates_and_isolates_cancellation() {
        let exec_count = Arc::new(AtomicUsize::new(0));
        let c_clone = exec_count.clone();
        let exec = StubExecutor::new_search(Duration::from_millis(60), move |p, _q, _| {
            c_clone.fetch_add(1, Ordering::SeqCst);
            Ok(vec![SearchHit {
                url: format!("https://{}.dev", p.as_str()),
                title: Some("ok".into()),
                published: None,
                content: None,
            }])
        });
        let router = Arc::new(WebRouter::from_executor(exec));

        let c1 = CancellationToken::new();
        let c2 = CancellationToken::new();
        let (r1, r2) = tokio::join!(
            router.search(WebProviderChoice::Auto, "coalesce me", Duration::from_secs(5), &c1, None),
            router.search(WebProviderChoice::Auto, "coalesce me", Duration::from_secs(5), &c2, None),
        );
        assert!(r1.is_ok() && r2.is_ok());
        assert_eq!(exec_count.load(Ordering::SeqCst), 1);

        // Case 2: Caller 1 cancels mid-flight while Caller 2 waits on the same normalized query.
        let exec_count2 = Arc::new(AtomicUsize::new(0));
        let c2_clone = exec_count2.clone();
        let exec2 = StubExecutor::new_search(Duration::from_millis(80), move |p, _q, cancel| {
            assert!(!cancel.is_cancelled(), "worker must NOT cancel while Caller 2 is waiting");
            c2_clone.fetch_add(1, Ordering::SeqCst);
            Ok(vec![SearchHit {
                url: format!("https://{}.dev", p.as_str()),
                title: Some("survived".into()),
                published: None,
                content: None,
            }])
        });
        let router2 = Arc::new(WebRouter::from_executor(exec2));

        let caller1_cancel = CancellationToken::new();
        let caller2_cancel = CancellationToken::new();
        let cancel_trigger = caller1_cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            cancel_trigger.cancel();
        });

        let (res1, res2) = tokio::join!(
            router2.search(
                WebProviderChoice::Auto,
                "isolated query",
                Duration::from_secs(5),
                &caller1_cancel,
                None
            ),
            router2.search(
                WebProviderChoice::Auto,
                "ISOLATED   QUERY",
                Duration::from_secs(5),
                &caller2_cancel,
                None
            ),
        );
        assert_eq!(res1.unwrap_err(), ProviderError::Cancelled);
        assert_eq!(res2.unwrap().hits[0].title.as_deref(), Some("survived"));
        assert_eq!(exec_count2.load(Ordering::SeqCst), 1);
        assert!(router2.in_flight.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_failover_respects_single_shared_deadline() {
        let exec = StubExecutor::new_search(Duration::from_millis(90), |_, _, _| {
            Err(ProviderError::VendorUnavailable {
                status: Some(503),
                detail: "down".into(),
            })
        });
        let router = WebRouter::from_executor(exec.clone());
        let cancel = CancellationToken::new();
        let err = router
            .search(
                WebProviderChoice::Auto,
                "shared deadline",
                Duration::from_millis(160),
                &cancel,
                None,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ProviderError::VendorUnavailable { .. }));
        let timeouts = exec.recorded_timeouts.lock().unwrap().clone();
        assert!(
            timeouts.len() <= 2 && !timeouts.is_empty(),
            "expected shared deadline to cap attempts at 1..=2, ran {:?}",
            timeouts
        );
        assert!(
            timeouts.iter().all(|t| *t <= PER_ATTEMPT_TIMEOUT),
            "expected per-attempt timeout cap: {:?}",
            timeouts
        );

        let direct_exec = StubExecutor::new_search(Duration::from_millis(90), |_, _, _| {
            Err(ProviderError::VendorUnavailable {
                status: Some(503),
                detail: "down".into(),
            })
        });
        let direct_router = WebRouter::from_executor(direct_exec.clone());
        let (progress_tx, _progress_rx) = watch::channel(String::new());
        let direct_budget = Duration::from_millis(160);
        let direct_err = execute_search_with_failover(
            direct_exec.clone(),
            direct_router.state.clone(),
            WebProviderChoice::Auto,
            "direct shared deadline",
            SearchTimeouts {
                total: direct_budget,
                per_attempt: PER_ATTEMPT_TIMEOUT,
            },
            &cancel,
            &progress_tx,
        )
        .await
        .unwrap_err();
        assert!(matches!(direct_err, ProviderError::VendorUnavailable { .. }));
        let direct_timeouts = direct_exec.recorded_timeouts.lock().unwrap().clone();
        assert!(
            direct_timeouts.len() == 2
                && direct_timeouts[1] < direct_timeouts[0]
                && direct_timeouts[0] <= direct_budget,
            "expected decreasing remaining budget across failover: {:?}",
            direct_timeouts
        );
    }

    #[tokio::test]
    async fn test_extract_fallback_respects_single_shared_deadline() {
        let exec = StubExecutor::new_extract_with_delay(Duration::from_millis(90), |_, _, _| {
            Err(ProviderError::VendorUnavailable {
                status: Some(503),
                detail: "down".into(),
            })
        });
        let router = WebRouter::from_executor(exec.clone());
        let cancel = CancellationToken::new();
        let err = router
            .extract_fallback(
                WebProviderChoice::Auto,
                "https://example.com/slow",
                Duration::from_millis(160),
                &cancel,
                None,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ProviderError::VendorUnavailable { .. }));
        let timeouts = exec.recorded_timeouts.lock().unwrap().clone();
        assert!(
            timeouts.len() <= 2 && !timeouts.is_empty(),
            "expected shared extract deadline to cap attempts at 1..=2, ran {:?}",
            timeouts
        );
        if timeouts.len() == 2 {
            assert!(
                timeouts[1] < timeouts[0],
                "expected decreasing remaining budget across extract fallback: {:?}",
                timeouts
            );
        }
    }

    #[tokio::test]
    async fn test_single_flight_waiter_honors_own_timeout() {
        let exec = StubExecutor::new_search(Duration::from_millis(250), |p, _, _| {
            Ok(vec![SearchHit {
                url: format!("https://{}.dev", p.as_str()),
                title: Some("slow-ok".into()),
                published: None,
                content: None,
            }])
        });
        let router = Arc::new(WebRouter::from_executor(exec));
        let c1 = CancellationToken::new();
        let c2 = CancellationToken::new();

        let r1_fut = router.search(
            WebProviderChoice::Auto,
            "slow query",
            Duration::from_secs(5),
            &c1,
            None,
        );
        let r2_fut = async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            router
                .search(
                    WebProviderChoice::Auto,
                    "slow query",
                    Duration::from_millis(60),
                    &c2,
                    None,
                )
                .await
        };

        let (r1, r2) = tokio::join!(r1_fut, r2_fut);
        assert!(r1.is_ok(), "caller 1 should complete: {r1:?}");
        assert!(r2.is_err(), "caller 2 should honor its 60ms timeout");
    }

    #[tokio::test]
    async fn test_single_flight_short_first_caller_does_not_starve_longer_joiner_and_cleans_up_before_wake() {
        let exec = StubExecutor::new_search(Duration::from_millis(160), |p, _, _| {
            Ok(vec![SearchHit {
                url: format!("https://{}.dev", p.as_str()),
                title: Some("ok".into()),
                published: None,
                content: None,
            }])
        });
        let router = Arc::new(WebRouter::from_executor(exec.clone()));
        let c1 = CancellationToken::new();
        let c2 = CancellationToken::new();

        let r1_fut = router.search(
            WebProviderChoice::Auto,
            "short first caller",
            Duration::from_millis(50),
            &c1,
            None,
        );
        let r2_fut = async {
            tokio::time::sleep(Duration::from_millis(10)).await;
            router
                .search(
                    WebProviderChoice::Auto,
                    "short first caller",
                    Duration::from_secs(5),
                    &c2,
                    None,
                )
                .await
        };

        let (r1, r2) = tokio::join!(r1_fut, r2_fut);
        assert!(r1.is_err(), "caller 1 must honor its own 50ms timeout");
        assert!(
            r2.is_ok(),
            "caller 2 must succeed with its 5s timeout despite caller 1's 50ms timeout: {r2:?}"
        );
        assert_eq!(exec.calls.lock().unwrap().len(), 1);

        // Verify worker cleanup removes the slot before waiters resume on error,
        // so an immediate retry starts a fresh execution rather than joining a finished slot.
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_clone = calls.clone();
        let exec_err_then_ok = StubExecutor::new_search(Duration::from_millis(20), move |p, _, _| {
            let n = calls_clone.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                return Err(ProviderError::BadRequest("transient".into()));
            }
            Ok(vec![SearchHit {
                url: format!("https://{}.dev", p.as_str()),
                title: Some("recovered".into()),
                published: None,
                content: None,
            }])
        });
        let router2 = WebRouter::from_executor(exec_err_then_ok);
        let err = router2
            .search(
                WebProviderChoice::Auto,
                "retry after fail",
                Duration::from_secs(5),
                &CancellationToken::new(),
                None,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ProviderError::BadRequest(_)));
        assert!(
            router2.in_flight.lock().unwrap().is_empty(),
            "in_flight must be cleared before caller wakes"
        );
        let ok = router2
            .search(
                WebProviderChoice::Auto,
                "retry after fail",
                Duration::from_secs(5),
                &CancellationToken::new(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(ok.hits[0].title.as_deref(), Some("recovered"));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn test_stalled_sticky_provider_fails_over_at_per_attempt_timeout() {
        struct RecordAndFailFirstExecutor {
            first: ProviderId,
            recorded_timeouts: Mutex<Vec<(ProviderId, Duration)>>,
        }

        #[async_trait]
        impl ProviderExecutor for RecordAndFailFirstExecutor {
            async fn search(
                &self,
                provider: ProviderId,
                _query: &str,
                timeout: Duration,
                _cancel: &CancellationToken,
            ) -> Result<Vec<SearchHit>, ProviderError> {
                self.recorded_timeouts
                    .lock()
                    .unwrap()
                    .push((provider, timeout));
                if provider == self.first {
                    return Err(ProviderError::VendorUnavailable {
                        status: None,
                        detail: "attempt timed out".into(),
                    });
                }
                Ok(vec![SearchHit {
                    url: format!("https://{}.example", provider.as_str()),
                    title: Some("ok".into()),
                    published: None,
                    content: None,
                }])
            }

            async fn extract(
                &self,
                provider: ProviderId,
                url: &str,
                timeout: Duration,
                _cancel: &CancellationToken,
            ) -> Result<ExtractedPage, ProviderError> {
                self.recorded_timeouts
                    .lock()
                    .unwrap()
                    .push((provider, timeout));
                if provider == self.first {
                    return Err(ProviderError::VendorUnavailable {
                        status: None,
                        detail: "extract attempt timed out".into(),
                    });
                }
                Ok(ExtractedPage {
                    url: url.to_string(),
                    final_url: None,
                    title: None,
                    content: provider.as_str().to_string(),
                })
            }
        }

        let per_attempt = Duration::from_secs(7);
        assert_eq!(PER_ATTEMPT_TIMEOUT, per_attempt);
        let exec = Arc::new(RecordAndFailFirstExecutor {
            first: ProviderId::Mehh,
            recorded_timeouts: Mutex::new(Vec::new()),
        });
        let router = WebRouter::from_executor(exec.clone());
        let caller_deadline = per_attempt + Duration::from_secs(8);
        let cancel = CancellationToken::new();

        let search_out = router
            .search(
                WebProviderChoice::Auto,
                "stall priority search",
                caller_deadline,
                &cancel,
                None,
            )
            .await
            .expect("search must fail over to provider #2");
        assert_eq!(search_out.provider, ProviderId::Exa);

        // Place Mehh failure expectations separately; for extract #1 is Exa.
        let extract_exec = Arc::new(RecordAndFailFirstExecutor {
            first: ProviderId::Exa,
            recorded_timeouts: Mutex::new(Vec::new()),
        });
        let extract_router = WebRouter::from_executor(extract_exec);
        let extract_out = extract_router
            .extract_fallback(
                WebProviderChoice::Auto,
                "https://example.com",
                caller_deadline,
                &cancel,
                None,
            )
            .await
            .expect("extract must fail over to provider #2");
        assert_ne!(extract_out.provider, ProviderId::Exa);

        let recorded = exec.recorded_timeouts.lock().unwrap().clone();
        assert!(
            recorded.iter().all(|(_, t)| *t <= PER_ATTEMPT_TIMEOUT),
            "every attempt must be capped at PER_ATTEMPT_TIMEOUT: {recorded:?}"
        );
        assert!(
            router
                .state
                .lock()
                .unwrap()
                .cooldowns
                .contains_key(&ProviderId::Mehh),
            "provider #1 must be placed on cooldown"
        );

        // Also exercise real async stall past per_attempt_timeout while caller deadline > per_attempt_timeout.
        struct StallFirstExecutor {
            stalled: ProviderId,
            stall_dur: Duration,
        }

        #[async_trait]
        impl ProviderExecutor for StallFirstExecutor {
            async fn search(
                &self,
                provider: ProviderId,
                _query: &str,
                timeout: Duration,
                cancel: &CancellationToken,
            ) -> Result<Vec<SearchHit>, ProviderError> {
                if provider == self.stalled {
                    tokio::select! {
                        _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
                        _ = tokio::time::sleep(timeout) => {
                            return Err(ProviderError::VendorUnavailable {
                                status: None,
                                detail: "attempt timed out".into(),
                            });
                        }
                        _ = tokio::time::sleep(self.stall_dur) => {}
                    }
                }
                Ok(vec![SearchHit {
                    url: format!("https://{}.example", provider.as_str()),
                    title: Some("ok".into()),
                    published: None,
                    content: None,
                }])
            }

            async fn extract(
                &self,
                provider: ProviderId,
                url: &str,
                timeout: Duration,
                cancel: &CancellationToken,
            ) -> Result<ExtractedPage, ProviderError> {
                if provider == self.stalled {
                    tokio::select! {
                        _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
                        _ = tokio::time::sleep(timeout) => {
                            return Err(ProviderError::VendorUnavailable {
                                status: None,
                                detail: "extract timed out".into(),
                            });
                        }
                        _ = tokio::time::sleep(self.stall_dur) => {}
                    }
                }
                Ok(ExtractedPage {
                    url: url.to_string(),
                    final_url: None,
                    title: None,
                    content: provider.as_str().to_string(),
                })
            }
        }

        let stall_exec = Arc::new(StallFirstExecutor {
            stalled: ProviderId::Mehh,
            stall_dur: Duration::from_millis(250),
        });
        let stall_router =
            WebRouter::from_executor(stall_exec).with_per_attempt_timeout(Duration::from_millis(60));
        let s_out = stall_router
            .search(
                WebProviderChoice::Auto,
                "real stall",
                Duration::from_millis(400),
                &cancel,
                None,
            )
            .await
            .expect("stalled search must fail over");
        assert_eq!(s_out.provider, ProviderId::Exa);
        assert!(stall_router
            .state
            .lock()
            .unwrap()
            .cooldowns
            .contains_key(&ProviderId::Mehh));

        let stall_extract_exec = Arc::new(StallFirstExecutor {
            stalled: ProviderId::Exa,
            stall_dur: Duration::from_millis(250),
        });
        let stall_extract_router = WebRouter::from_executor(stall_extract_exec)
            .with_per_attempt_timeout(Duration::from_millis(60));
        let e_out = stall_extract_router
            .extract_fallback(
                WebProviderChoice::Auto,
                "https://example.com",
                Duration::from_millis(400),
                &cancel,
                None,
            )
            .await
            .expect("stalled extract must fail over");
        assert_ne!(e_out.provider, ProviderId::Exa);
    }
}
