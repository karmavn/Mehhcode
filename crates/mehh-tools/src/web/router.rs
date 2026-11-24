//! Session-sticky failover router, cooldown tracker, and single-flight search memo.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::providers::{execute_extract, execute_search, WebEndpoints};
use super::types::{ExtractedPage, ProviderError, ProviderId, SearchHit, WebProviderChoice};

pub const SEARCH_MEMO_TTL: Duration = Duration::from_secs(15 * 60);
pub const SEARCH_MEMO_MAX_ENTRIES: usize = 128;

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
    search_sticky: ProviderId,
    extract_sticky: ProviderId,
    cooldowns: HashMap<ProviderId, Instant>,
    memo: HashMap<SearchMemoKey, MemoEntry>,
}

struct InFlightSlot {
    rx: watch::Receiver<Option<Result<SearchOutput, ProviderError>>>,
    progress_rx: watch::Receiver<String>,
    waiters: AtomicUsize,
    worker_cancel: CancellationToken,
}

/// Thread-safe router coordinating sticky provider selection, cooldown failover,
/// search memoization, and single-flight request deduplication.
pub struct WebRouter {
    executor: Arc<dyn ProviderExecutor>,
    state: Arc<Mutex<RouterState>>,
    in_flight: Arc<Mutex<HashMap<SearchMemoKey, Arc<InFlightSlot>>>>,
}

impl WebRouter {
    pub fn new(endpoints: WebEndpoints, client: reqwest::Client) -> Self {
        let session_id = Uuid::new_v4().simple().to_string();
        let executor: Arc<dyn ProviderExecutor> = Arc::new(HttpProviderExecutor {
            endpoints,
            client,
            session_id,
        });
        Self::from_executor(executor)
    }

    pub(crate) fn from_executor(executor: Arc<dyn ProviderExecutor>) -> Self {
        let seed = Uuid::new_v4().as_u128();
        let search_idx = (seed as usize) % ProviderId::ALL_SEARCH.len();
        let extract_idx = ((seed >> 16) as usize) % ProviderId::ALL_EXTRACT.len();
        Self {
            executor,
            state: Arc::new(Mutex::new(RouterState {
                search_sticky: ProviderId::ALL_SEARCH[search_idx],
                extract_sticky: ProviderId::ALL_EXTRACT[extract_idx],
                cooldowns: HashMap::new(),
                memo: HashMap::new(),
            })),
            in_flight: Arc::new(Mutex::new(HashMap::new())),
        }
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

        let slot = self.acquire_or_join_in_flight(key.clone(), query.to_string(), timeout);
        let _guard = WaiterGuard {
            key,
            slot: slot.clone(),
            in_flight: self.in_flight.clone(),
        };

        let mut rx = slot.rx.clone();
        let mut progress_rx = slot.progress_rx.clone();
        let mut progress_open = true;
        let mut last_progress = String::new();
        loop {
            emit_progress_if_new(&mut progress_rx, &mut last_progress, on_progress);
            if let Some(result) = rx.borrow_and_update().clone() {
                return result;
            }
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
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

        for (idx, provider) in candidates.iter().copied().enumerate() {
            if cancel.is_cancelled() {
                return Err(ProviderError::Cancelled);
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
            match self.executor.extract(provider, url, timeout, cancel).await {
                Ok(page) => {
                    self.record_extract_success(choice, provider);
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
        state
            .memo
            .retain(|_, entry| now.duration_since(entry.inserted_at) < SEARCH_MEMO_TTL);
        state.memo.get(key).map(|entry| entry.output.clone())
    }

    fn acquire_or_join_in_flight(
        &self,
        key: SearchMemoKey,
        query: String,
        timeout: Duration,
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

        tokio::spawn(async move {
            let _cleanup = WorkerCleanupGuard {
                key: key.clone(),
                slot: slot_for_worker,
                in_flight,
            };
            let result = execute_search_with_failover(
                executor,
                state.clone(),
                key.choice,
                &query,
                timeout,
                &worker_cancel,
                &progress_tx,
            )
            .await;

            if let Ok(ref output) = result {
                store_memo(&state, key, output.clone());
            }
            let _ = tx.send(Some(result));
        });

        slot
    }

    fn plan_extract_candidates(&self, choice: WebProviderChoice) -> Vec<ProviderId> {
        let now = Instant::now();
        let mut state = mehh_ai::lock_recover(&self.state);
        state.cooldowns.retain(|_, until| *until > now);

        let preferred = match choice {
            WebProviderChoice::Fixed(id) if id.supports_extract() => id,
            _ => state.extract_sticky,
        };
        ordered_candidates(&ProviderId::ALL_EXTRACT, preferred, &state.cooldowns)
    }

    fn record_extract_success(&self, choice: WebProviderChoice, provider: ProviderId) {
        let mut state = mehh_ai::lock_recover(&self.state);
        state.cooldowns.remove(&provider);
        if choice == WebProviderChoice::Auto {
            state.extract_sticky = provider;
        }
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
    let msg = prx.borrow_and_update().clone();
    if !msg.is_empty() && msg != *last {
        *last = msg.clone();
        if let Some(cb) = on_progress {
            cb(&msg);
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

async fn execute_search_with_failover(
    executor: Arc<dyn ProviderExecutor>,
    state: Arc<Mutex<RouterState>>,
    choice: WebProviderChoice,
    query: &str,
    timeout: Duration,
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
            ordered_candidates(&ProviderId::ALL_SEARCH, guard.search_sticky, &guard.cooldowns)
        }
    };

    let mut attempted: Vec<ProviderId> = Vec::new();
    let mut last_err: Option<ProviderError> = None;

    for (idx, provider) in candidates.iter().copied().enumerate() {
        if cancel.is_cancelled() {
            return Err(ProviderError::Cancelled);
        }
        let msg = if idx == 0 {
            format!("Searching via {}...", provider.display_name())
        } else {
            let prev = candidates[idx - 1].display_name();
            format!("{prev} unavailable, retrying via {}...", provider.display_name())
        };
        let _ = progress_tx.send(msg);

        attempted.push(provider);
        match executor.search(provider, query, timeout, cancel).await {
            Ok(hits) => {
                let mut guard = mehh_ai::lock_recover(&state);
                guard.cooldowns.remove(&provider);
                if choice == WebProviderChoice::Auto {
                    guard.search_sticky = provider;
                }
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

fn ordered_candidates(
    all: &[ProviderId],
    preferred: ProviderId,
    cooldowns: &HashMap<ProviderId, Instant>,
) -> Vec<ProviderId> {
    let start_idx = all.iter().position(|p| *p == preferred).unwrap_or(0);
    let mut healthy = Vec::with_capacity(all.len());
    for offset in 0..all.len() {
        let p = all[(start_idx + offset) % all.len()];
        if !cooldowns.contains_key(&p) {
            healthy.push(p);
        }
    }
    if !healthy.is_empty() {
        return healthy;
    }

    // All candidates are cooling down: attempt the earliest-expiring provider once.
    let earliest = all
        .iter()
        .copied()
        .min_by_key(|p| cooldowns.get(p).copied())
        .unwrap_or(preferred);
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
    raw.split_whitespace()
        .map(|w| w.to_lowercase())
        .collect::<Vec<_>>()
        .join(" ")
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
            Arc::new(Self {
                calls: Mutex::new(Vec::new()),
                search_delay: Duration::ZERO,
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
            _timeout: Duration,
            cancel: &CancellationToken,
        ) -> Result<Vec<SearchHit>, ProviderError> {
            self.calls.lock().unwrap().push(provider);
            if !self.search_delay.is_zero() {
                tokio::select! {
                    _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
                    _ = tokio::time::sleep(self.search_delay) => {}
                }
            }
            self.on_search.as_ref().unwrap()(provider, query, cancel.clone())
        }

        async fn extract(
            &self,
            provider: ProviderId,
            url: &str,
            _timeout: Duration,
            cancel: &CancellationToken,
        ) -> Result<ExtractedPage, ProviderError> {
            self.calls.lock().unwrap().push(provider);
            self.on_extract.as_ref().unwrap()(provider, url, cancel.clone())
        }
    }

    #[tokio::test]
    async fn test_auto_search_is_sticky_until_cooldown() {
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

        let r1 = router
            .search(WebProviderChoice::Auto, "query one", Duration::from_secs(5), &cancel, None)
            .await
            .unwrap();
        let r2 = router
            .search(WebProviderChoice::Auto, "query two", Duration::from_secs(5), &cancel, None)
            .await
            .unwrap();
        assert_eq!(r1.provider, r2.provider);

        *failing_provider.lock().unwrap() = Some(r1.provider);
        let r3 = router
            .search(
                WebProviderChoice::Auto,
                "query three",
                Duration::from_secs(5),
                &cancel,
                None,
            )
            .await
            .unwrap();
        assert_ne!(r3.provider, r1.provider);

        // Next call stays on new sticky provider and skips cooled-down r1.provider.
        let calls_before = exec.calls.lock().unwrap().len();
        let r4 = router
            .search(
                WebProviderChoice::Auto,
                "query four",
                Duration::from_secs(5),
                &cancel,
                None,
            )
            .await
            .unwrap();
        assert_eq!(r4.provider, r3.provider);
        assert_eq!(exec.calls.lock().unwrap().len(), calls_before + 1);
    }

    #[tokio::test]
    async fn test_auto_search_rotates_on_401_403_5xx_and_stops_on_bad_request() {
        let exec_rotate = StubExecutor::new_search(Duration::ZERO, |p, _q, _| match p {
            ProviderId::Exa => Err(ProviderError::VendorUnavailable {
                status: Some(403),
                detail: "403".into(),
            }),
            _ => Ok(vec![]),
        });
        let router = WebRouter::from_executor(exec_rotate);
        router.state.lock().unwrap().search_sticky = ProviderId::Exa;
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
        assert_ne!(out.provider, ProviderId::Exa);
        assert!(router
            .state
            .lock()
            .unwrap()
            .cooldowns
            .contains_key(&ProviderId::Exa));

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
}
