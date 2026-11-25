use super::cloud_manifest_service::CloudGameSummary;
use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

const CACHE_TTL: Duration = Duration::from_secs(5 * 60);

#[derive(Default)]
pub(crate) struct CloudStoreCache {
    state: Mutex<CacheState>,
    loading: Mutex<()>,
}

#[derive(Default)]
struct CacheState {
    generation: u64,
    entry: Option<CachedStore>,
}

struct CachedStore {
    authorization: String,
    loaded_at: Instant,
    games: Vec<CloudGameSummary>,
}

impl CloudStoreCache {
    pub(crate) fn invalidate(&self) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.generation = state.generation.wrapping_add(1);
        state.entry = None;
    }

    pub(crate) fn get_or_load(
        &self,
        authorization: &str,
        force: bool,
        load: impl FnOnce() -> Result<Vec<CloudGameSummary>, String>,
    ) -> Result<(Vec<CloudGameSummary>, bool, Duration), String> {
        if force {
            self.invalidate();
        }
        let _loading = self
            .loading
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let generation = {
            let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            if let Some(entry) = &state.entry {
                let age = entry.loaded_at.elapsed();
                if entry.authorization == authorization && age < CACHE_TTL {
                    return Ok((entry.games.clone(), true, CACHE_TTL - age));
                }
            }
            state.generation
        };
        let games = load()?;
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if generation != state.generation {
            return Err("商店内容或百度授权已变化，请重新加载商店".into());
        }
        state.entry = Some(CachedStore {
            authorization: authorization.to_string(),
            loaded_at: Instant::now(),
            games: games.clone(),
        });
        Ok((games, false, CACHE_TTL))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn reuses_even_empty_results_for_five_minutes() {
        let cache = CloudStoreCache::default();
        assert!(
            !cache
                .get_or_load("account-a", false, || Ok(Vec::new()))
                .unwrap()
                .1
        );
        cache
            .state
            .lock()
            .unwrap()
            .entry
            .as_mut()
            .unwrap()
            .loaded_at = Instant::now() - Duration::from_secs(299);
        assert!(
            cache
                .get_or_load("account-a", false, || panic!("cached result"))
                .unwrap()
                .1
        );
        cache
            .state
            .lock()
            .unwrap()
            .entry
            .as_mut()
            .unwrap()
            .loaded_at = Instant::now() - CACHE_TTL;
        assert!(
            !cache
                .get_or_load("account-a", false, || Ok(Vec::new()))
                .unwrap()
                .1
        );
    }

    #[test]
    fn refresh_invalidation_and_account_change_bypass_cached_results() {
        let cache = CloudStoreCache::default();
        cache
            .get_or_load("account-a", false, || Ok(Vec::new()))
            .unwrap();
        assert!(
            !cache
                .get_or_load("account-a", true, || Ok(Vec::new()))
                .unwrap()
                .1
        );
        assert!(
            !cache
                .get_or_load("account-b", false, || Ok(Vec::new()))
                .unwrap()
                .1
        );
        cache.invalidate();
        assert!(
            !cache
                .get_or_load("account-b", false, || Ok(Vec::new()))
                .unwrap()
                .1
        );
    }

    #[test]
    fn failures_are_not_cached_and_invalidated_requests_cannot_publish() {
        let cache = CloudStoreCache::default();
        assert!(cache
            .get_or_load("account-a", false, || Err("network".into()))
            .is_err());
        assert!(cache
            .get_or_load("account-a", false, || {
                cache.invalidate();
                Ok(Vec::new())
            })
            .is_err());
        assert!(cache.state.lock().unwrap().entry.is_none());
        assert!(
            !cache
                .get_or_load("account-a", false, || Ok(Vec::new()))
                .unwrap()
                .1
        );
    }

    #[test]
    fn concurrent_requests_share_one_complete_list_load() {
        let cache = CloudStoreCache::default();
        let loads = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..4 {
                let cache = &cache;
                let loads = &loads;
                scope.spawn(move || {
                    cache
                        .get_or_load("account-a", false, || {
                            loads.fetch_add(1, Ordering::SeqCst);
                            Ok(Vec::new())
                        })
                        .unwrap();
                });
            }
        });
        assert_eq!(loads.load(Ordering::SeqCst), 1);
    }
}
