//! The TTL cache of the upstream reads: only successful results are stored, a failed
//! load is not cached, and there is no single-flight (two callers may both load).

use crate::services::UpResult;
use std::future::Future;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub struct TtlCache<T: Clone> {
    ttl: Duration,
    slot: Mutex<Option<(Instant, T)>>,
}

impl<T: Clone> TtlCache<T> {
    pub fn new(ttl: Duration) -> TtlCache<T> {
        TtlCache { ttl, slot: Mutex::new(None) }
    }

    fn fresh(&self) -> Option<T> {
        let g = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        g.as_ref().filter(|(at, _)| at.elapsed() < self.ttl).map(|(_, v)| v.clone())
    }

    /// The cached value if younger than the TTL, else `load()` (stored only when Ok).
    pub async fn get<F, Fut>(&self, load: F) -> UpResult<T>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = UpResult<T>>,
    {
        if let Some(v) = self.fresh() {
            return Ok(v);
        }
        let v = load().await?;
        *self.slot.lock().unwrap_or_else(|e| e.into_inner()) = Some((Instant::now(), v.clone()));
        Ok(v)
    }

    /// Forget the cached value (tests; a later call loads again).
    pub fn clear(&self) {
        *self.slot.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::UpstreamError;

    #[tokio::test]
    async fn caches_only_success() {
        let c: TtlCache<u32> = TtlCache::new(Duration::from_secs(60));
        assert_eq!(c.get(|| async { Err(UpstreamError::Timeout) }).await, Err(UpstreamError::Timeout));
        assert_eq!(c.get(|| async { Ok(1) }).await, Ok(1));
        assert_eq!(c.get(|| async { Ok(2) }).await, Ok(1));
        c.clear();
        assert_eq!(c.get(|| async { Ok(3) }).await, Ok(3));
        let short: TtlCache<u32> = TtlCache::new(Duration::ZERO);
        assert_eq!(short.get(|| async { Ok(1) }).await, Ok(1));
        assert_eq!(short.get(|| async { Ok(2) }).await, Ok(2));
    }
}
