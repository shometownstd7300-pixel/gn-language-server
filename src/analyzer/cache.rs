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
    path::PathBuf,
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};

use crate::common::storage::{DocumentStorage, DocumentVersion};

const VERIFY_INTERVAL: Duration = Duration::from_secs(5);

fn compute_next_verify(t: Instant, version: DocumentVersion) -> Instant {
    match version {
        DocumentVersion::OnDisk { .. }
        | DocumentVersion::IoError
        | DocumentVersion::AnalysisError => t + VERIFY_INTERVAL,
        // Do not skip verification for in-memory documents.
        DocumentVersion::InMemory { .. } => t,
    }
}

enum CacheState {
    Stale,
    Fresh { expires: Instant },
}

pub struct CacheNode {
    path: PathBuf,
    version: DocumentVersion,
    deps: Vec<Arc<CacheNode>>,
    state: RwLock<CacheState>,
}

impl CacheNode {
    pub fn new(
        path: PathBuf,
        version: DocumentVersion,
        deps: Vec<Arc<CacheNode>>,
        request_time: Instant,
    ) -> Arc<Self> {
        Arc::new(Self {
            path,
            version,
            deps,
            state: RwLock::new(CacheState::Fresh {
                expires: compute_next_verify(request_time, version),
            }),
        })
    }

    pub fn verify(&self, request_time: Instant, storage: &DocumentStorage) -> bool {
        // Fast path with a read lock.
        let expires = match &*self.state.read().unwrap() {
            CacheState::Stale => return false,
            CacheState::Fresh { expires } => *expires,
        };
        if request_time <= expires {
            if !self.verify_deps(request_time, storage) {
                *self.state.write().unwrap() = CacheState::Stale;
                return false;
            }
            return true;
        }

        // Slow path with a write lock.
        let mut state_guard = self.state.write().unwrap();
        let expires = match &*state_guard {
            CacheState::Stale => return false,
            CacheState::Fresh { expires } => *expires,
        };
        if request_time <= expires {
            if !self.verify_deps(request_time, storage) {
                *state_guard = CacheState::Stale;
                return false;
            }
            return true;
        }

        let version = storage.read_version(&self.path);
        if version != self.version {
            *state_guard = CacheState::Stale;
            return false;
        }

        if !self.verify_deps(request_time, storage) {
            *state_guard = CacheState::Stale;
            return false;
        }

        *state_guard = CacheState::Fresh {
            expires: compute_next_verify(request_time, self.version),
        };
        true
    }

    fn verify_deps(&self, request_time: Instant, storage: &DocumentStorage) -> bool {
        for dep in &self.deps {
            if !dep.verify(request_time, storage) {
                return false;
            }
        }
        true
    }
}
