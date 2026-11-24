pub mod catalog;
pub mod faux;
pub mod overflow;
pub mod retry;
pub mod stream;
pub mod streaming_json;
pub mod transform_messages;
pub mod types;
pub mod unicode;
pub mod validation;

pub mod anthropic;
pub mod azure_openai;
pub mod bedrock;
pub mod google;
pub mod http_safety;
pub mod mistral;
pub mod model_discovery;
pub mod oauth;
pub mod openai;
pub mod openai_responses;
pub mod sse;

pub use catalog::*;
pub use faux::*;
pub use overflow::*;
pub use retry::*;
pub use stream::*;
pub use streaming_json::*;
pub use transform_messages::*;
pub use types::*;
pub use unicode::*;
pub use validation::*;

pub use anthropic::*;
pub use azure_openai::*;
pub use bedrock::*;
pub use google::*;
pub use http_safety::*;
pub use mistral::*;
pub use model_discovery::*;
pub use oauth::*;
pub use openai::*;
pub use openai_responses::*;
pub use sse::*;

/// Locks a `std::sync::Mutex`, recovering the inner value if a previous holder panicked.
pub fn lock_recover<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// Acquires a shared `std::sync::RwLock` read guard, recovering from poison.
pub fn read_recover<T>(lock: &std::sync::RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(|e| e.into_inner())
}

/// Acquires an exclusive `std::sync::RwLock` write guard, recovering from poison.
pub fn write_recover<T>(lock: &std::sync::RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(|e| e.into_inner())
}
