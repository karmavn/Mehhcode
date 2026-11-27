use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use tokio::sync::Mutex as TokioMutex;

type QueueMap = HashMap<String, Weak<TokioMutex<()>>>;

static QUEUES: std::sync::LazyLock<Mutex<QueueMap>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

/// A poisoned queue map must not abort the process during cleanup; the data
/// inside is only a lock registry, so recovering the inner value is safe.
fn lock_queues() -> MutexGuard<'static, QueueMap> {
    QUEUES.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub fn get_mutation_queue_key(file_path: &Path) -> String {
    let canonical = crate::core::context::canonicalize_lossy(file_path);
    let normalized = crate::core::context::normalize_path_for_comparison(&canonical);
    normalized.to_string_lossy().replace('\\', "/")
}

struct QueueCleanupGuard {
    key: String,
}

impl Drop for QueueCleanupGuard {
    fn drop(&mut self) {
        let mut map = lock_queues();
        if map
            .get(&self.key)
            .is_some_and(|weak| weak.strong_count() == 0)
        {
            map.remove(&self.key);
        }
    }
}

pub async fn with_file_mutation_queue_key<F, Fut, T>(file_path: &Path, f: F) -> T
where
    F: FnOnce(String) -> Fut,
    Fut: std::future::Future<Output = T>,
{
    // canonicalize() hits the filesystem; keep it off the Tokio worker.
    let key = {
        let path = file_path.to_path_buf();
        match tokio::task::spawn_blocking(move || get_mutation_queue_key(&path)).await {
            Ok(key) => key,
            Err(_) => get_mutation_queue_key(file_path),
        }
    };

    // Declared before `lock` so unwinding drops lock (and its guard) first.
    let cleanup = QueueCleanupGuard { key: key.clone() };
    let lock = {
        let mut map = lock_queues();
        match map.get(&key).and_then(Weak::upgrade) {
            Some(existing) => existing,
            None => {
                let created = Arc::new(TokioMutex::new(()));
                map.insert(key.clone(), Arc::downgrade(&created));
                created
            }
        }
    };

    let guard = lock.lock().await;
    let result = f(key).await;
    drop(guard);
    drop(lock);
    drop(cleanup);
    result
}

pub async fn with_file_mutation_queue<Fut, T>(file_path: &Path, future: Fut) -> T
where
    Fut: std::future::Future<Output = T>,
{
    with_file_mutation_queue_key(file_path, |_| future).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::time::{sleep, Duration};

    #[tokio::test]
    async fn same_file_writes_serialize_different_files_parallel() {
        let temp_dir = crate::TempTestDir::new("mutation_queue");
        let file_a = temp_dir.path().join("foo.txt");
        let file_b = temp_dir.path().join("bar.txt");
        std::fs::write(&file_a, "initial a\n").unwrap();
        std::fs::write(&file_b, "initial b\n").unwrap();

        // 1. Same file with canonical path vs relative/non-canonical path
        let active_concurrent = Arc::new(AtomicUsize::new(0));
        let max_concurrent_same_file = Arc::new(AtomicUsize::new(0));

        let f_a1 = file_a.clone();
        let act1 = active_concurrent.clone();
        let max1 = max_concurrent_same_file.clone();
        let task1 = tokio::spawn(async move {
            with_file_mutation_queue(&f_a1, async {
                let cur = act1.fetch_add(1, Ordering::SeqCst) + 1;
                max1.fetch_max(cur, Ordering::SeqCst);
                sleep(Duration::from_millis(50)).await;
                act1.fetch_sub(1, Ordering::SeqCst);
            })
            .await;
        });

        // Use a non-canonical path representation for the same file
        let f_a2 = temp_dir.path().join("./foo.txt");
        let act2 = active_concurrent.clone();
        let max2 = max_concurrent_same_file.clone();
        let task2 = tokio::spawn(async move {
            with_file_mutation_queue(&f_a2, async {
                let cur = act2.fetch_add(1, Ordering::SeqCst) + 1;
                max2.fetch_max(cur, Ordering::SeqCst);
                sleep(Duration::from_millis(50)).await;
                act2.fetch_sub(1, Ordering::SeqCst);
            })
            .await;
        });

        let _ = tokio::join!(task1, task2);
        assert_eq!(
            max_concurrent_same_file.load(Ordering::SeqCst),
            1,
            "two tasks on the same file must be strictly serialized"
        );

        // 2. Different files run concurrently
        let active_diff = Arc::new(AtomicUsize::new(0));
        let max_concurrent_diff = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(tokio::sync::Barrier::new(2));

        let act_a = active_diff.clone();
        let max_diff_a = max_concurrent_diff.clone();
        let barrier_a = barrier.clone();
        let t_a = tokio::spawn(async move {
            with_file_mutation_queue(&file_a, async {
                let cur = act_a.fetch_add(1, Ordering::SeqCst) + 1;
                max_diff_a.fetch_max(cur, Ordering::SeqCst);
                tokio::time::timeout(Duration::from_secs(2), barrier_a.wait())
                    .await
                    .expect("task A timed out waiting for concurrent task B");
                act_a.fetch_sub(1, Ordering::SeqCst);
            })
            .await;
        });

        let act_b = active_diff.clone();
        let max_diff_b = max_concurrent_diff.clone();
        let barrier_b = barrier;
        let t_b = tokio::spawn(async move {
            with_file_mutation_queue(&file_b, async {
                let cur = act_b.fetch_add(1, Ordering::SeqCst) + 1;
                max_diff_b.fetch_max(cur, Ordering::SeqCst);
                tokio::time::timeout(Duration::from_secs(2), barrier_b.wait())
                    .await
                    .expect("task B timed out waiting for concurrent task A");
                act_b.fetch_sub(1, Ordering::SeqCst);
            })
            .await;
        });

        let (res_a, res_b) = tokio::join!(t_a, t_b);
        res_a.unwrap();
        res_b.unwrap();
        assert_eq!(
            max_concurrent_diff.load(Ordering::SeqCst),
            2,
            "two tasks on distinct files must run in parallel"
        );
    }

    #[test]
    fn test_mutation_queue_key_identical_before_and_after_creation() {
        let temp_dir = crate::TempTestDir::new("key_norm");
        let non_existent = temp_dir.path().join("does_not_exist_yet.txt");

        let key_before = get_mutation_queue_key(&non_existent);
        std::fs::write(&non_existent, "hello").unwrap();
        let key_after = get_mutation_queue_key(&non_existent);

        assert_eq!(
            key_before, key_after,
            "Queue key must be identical before and after file creation"
        );
    }

    #[test]
    fn test_mutation_queue_key_multilevel_nonexistent() {
        let temp_dir = crate::TempTestDir::new("key_multilevel");
        let non_existent = temp_dir.path().join("sub1").join("sub2").join("deep_file.txt");

        let key_before = get_mutation_queue_key(&non_existent);
        std::fs::create_dir_all(non_existent.parent().unwrap()).unwrap();
        std::fs::write(&non_existent, "hello deep").unwrap();
        let key_after = get_mutation_queue_key(&non_existent);

        assert_eq!(
            key_before, key_after,
            "Queue key must be identical before and after multilevel directory/file creation"
        );
    }

    #[tokio::test]
    async fn test_mutation_queue_cleanup_on_cancellation() {
        let temp_dir = crate::TempTestDir::new("queue_cleanup");
        let file = temp_dir.path().join("cancel.txt");

        let f1 = file.clone();
        let hold_task = tokio::spawn(async move {
            with_file_mutation_queue(&f1, async {
                sleep(Duration::from_millis(100)).await;
            })
            .await;
        });

        sleep(Duration::from_millis(20)).await;

        let f2 = file.clone();
        let _ = tokio::time::timeout(Duration::from_millis(20), async {
            with_file_mutation_queue(&f2, async {
                panic!("should not run");
            })
            .await;
        })
        .await;

        hold_task.await.unwrap();

        let key = get_mutation_queue_key(&file);
        let map = QUEUES.lock().unwrap();
        assert!(
            !map.contains_key(&key),
            "Queue entry must be removed after hold_task and cancelled task finish"
        );
    }

    #[test]
    fn test_mutation_queue_key_resolves_symlinks_before_folding_parent_dir() {
        let ws = crate::TempTestDir::new("queue_symlink_ws");
        let outside = crate::TempTestDir::new("queue_symlink_outside");
        let outside_sub = outside.path().join("sub");
        std::fs::create_dir_all(&outside_sub).unwrap();
        let link = ws.path().join("symlink");

        #[cfg(unix)]
        let link_res = std::os::unix::fs::symlink(&outside_sub, &link);
        #[cfg(windows)]
        let link_res = std::os::windows::fs::symlink_dir(&outside_sub, &link);
        #[cfg(not(any(unix, windows)))]
        let link_res: std::io::Result<()> = Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "unsupported",
        ));

        if let Err(e) = link_res {
            eprintln!("skipping symlink queue key test: {e}");
            return;
        }

        let via_symlink = link.join("..").join("new.txt");
        let direct_outside = outside.path().join("new.txt");
        let direct_ws = ws.path().join("new.txt");
        assert_eq!(
            get_mutation_queue_key(&via_symlink),
            get_mutation_queue_key(&direct_outside)
        );
        assert_ne!(
            get_mutation_queue_key(&via_symlink),
            get_mutation_queue_key(&direct_ws)
        );
    }
}
