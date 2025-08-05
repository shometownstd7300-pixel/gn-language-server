// Copyright 2025 Google LLC
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::{
    collections::{btree_map::Entry, BTreeMap},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use tokio::sync::{RwLock, SetOnce};

use crate::{error::Result, storage::DocumentVersion};

pub fn resolve_path(name: &str, root_dir: &Path, current_dir: &Path) -> PathBuf {
    if let Some(rest) = name.strip_prefix("//") {
        root_dir.join(rest)
    } else {
        current_dir.join(name)
    }
}

const CHECK_INTERVAL: Duration = Duration::from_secs(5);

pub fn compute_next_check(t: Instant, version: DocumentVersion) -> Instant {
    match version {
        DocumentVersion::OnDisk { .. } => t + CHECK_INTERVAL,
        // Do not skip version checks for in-memory documents.
        _ => t,
    }
}

pub struct FreshCache<K, V> {
    entries: Arc<RwLock<BTreeMap<K, Arc<SetOnce<Result<V>>>>>>,
}

impl<K, V> FreshCache<K, V> {
    pub fn new() -> Self {
        Self {
            entries: Default::default(),
        }
    }
}

impl<K, V> FreshCache<K, V>
where
    V: Clone,
{
    pub async fn ok_values(&self) -> Vec<V> {
        let values: Vec<_> = self.entries.read().await.values().cloned().collect();
        values
            .into_iter()
            .filter_map(|entry| entry.get().and_then(|result| result.as_ref().ok().cloned()))
            .collect()
    }
}

impl<K, V> FreshCache<K, V>
where
    K: Ord + Clone,
    V: Clone,
{
    pub async fn get_or_insert(
        &self,
        key: K,
        mut v: impl AsyncFnMut(&V) -> Result<bool>,
        f: impl AsyncFnOnce() -> Result<V>,
    ) -> Result<V> {
        if let Some(entry) = self.entries.read().await.get(&key).cloned() {
            let entry = entry.wait().await.clone()?;
            if v(&entry).await? {
                return Ok(entry);
            }
        }

        let vacant_entry = loop {
            match self.entries.write().await.entry(key.clone()) {
                Entry::Vacant(entry) => break entry.insert(Default::default()).clone(),
                Entry::Occupied(entry) => {
                    let entry = entry.get().wait().await.clone()?;
                    if v(&entry).await? {
                        return Ok(entry);
                    }
                }
            }
        };

        let result = f().await;
        vacant_entry.set(result.clone()).ok();
        result
    }
}
