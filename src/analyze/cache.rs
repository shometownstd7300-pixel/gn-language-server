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

use crate::storage::{DocumentStorage, DocumentVersion};

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

enum CachedVerifierState {
    Stale,
    ReverifyAfter(Instant),
}

pub struct CachedVerifier {
    path: PathBuf,
    version: DocumentVersion,
    deps: Vec<Arc<CachedVerifier>>,
    state: RwLock<CachedVerifierState>,
}

impl CachedVerifier {
    pub fn new(
        path: PathBuf,
        version: DocumentVersion,
        deps: Vec<Arc<CachedVerifier>>,
        request_time: Instant,
    ) -> Self {
        Self {
            path,
            version,
            deps,
            state: RwLock::new(CachedVerifierState::ReverifyAfter(compute_next_verify(
                request_time,
                version,
            ))),
        }
    }

    pub fn verify(&self, request_time: Instant, storage: &DocumentStorage) -> bool {
        {
            let state_guard = self.state.read().unwrap();
            let expires = match &*state_guard {
                CachedVerifierState::Stale => return false,
                CachedVerifierState::ReverifyAfter(expires) => *expires,
            };
            if request_time <= expires {
                return true;
            }
        }

        {
            let mut state_guard = self.state.write().unwrap();
            let expires = match &*state_guard {
                CachedVerifierState::Stale => return false,
                CachedVerifierState::ReverifyAfter(expires) => *expires,
            };
            if request_time <= expires {
                return true;
            }

            let version = storage.read_version(&self.path);
            if version != self.version {
                *state_guard = CachedVerifierState::Stale;
                return false;
            }

            for dep in &self.deps {
                if !dep.verify(request_time, storage) {
                    *state_guard = CachedVerifierState::Stale;
                    return false;
                }
            }

            *state_guard =
                CachedVerifierState::ReverifyAfter(compute_next_verify(request_time, self.version));
            true
        }
    }
}
