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

use std::fmt::Display;

use serde_json::Value;
use tower_lsp::lsp_types::{ConfigurationItem, MessageType};

use crate::providers::RpcResult;

#[derive(Clone)]
pub struct TestableClient {
    client: Option<tower_lsp::Client>,
}

impl TestableClient {
    pub fn new(client: tower_lsp::Client) -> Self {
        Self {
            client: Some(client),
        }
    }

    #[cfg(test)]
    pub fn new_for_testing() -> Self {
        Self { client: None }
    }

    pub async fn log_message<M: Display>(&self, typ: MessageType, message: M) {
        if let Some(client) = &self.client {
            client.log_message(typ, message).await;
        }
    }

    pub async fn configuration(&self, items: Vec<ConfigurationItem>) -> RpcResult<Vec<Value>> {
        if let Some(client) = &self.client {
            client.configuration(items).await
        } else {
            Ok(Vec::new())
        }
    }
}
