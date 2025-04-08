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

use std::{borrow::Cow, process::Stdio};

use pest::Span;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
};
use tower_lsp::lsp_types::{DocumentFormattingParams, TextEdit};

use crate::{analyze::find_workspace_root, binary::find_gn_binary};

use super::{into_rpc_error, new_rpc_error, ProviderContext, RpcResult};

pub async fn formatting(
    context: &ProviderContext,
    params: DocumentFormattingParams,
) -> RpcResult<Option<Vec<TextEdit>>> {
    let Ok(file_path) = params.text_document.uri.to_file_path() else {
        return Err(new_rpc_error(Cow::from(format!(
            "invalid file URI: {}",
            params.text_document.uri
        ))));
    };

    let configs = context.client.configurations().await;
    let gn_path = if let Some(gn_path) = &configs.binary_path {
        if gn_path.exists() {
            gn_path.to_path_buf()
        } else {
            return Err(new_rpc_error(Cow::from(format!(
                "gn binary not found at {}; check configuration value gn.binaryPath",
                gn_path.display()
            ))));
        }
    } else if let Some(gn_path) = find_gn_binary(find_workspace_root(&file_path).ok()) {
        gn_path
    } else {
        return Err(new_rpc_error(Cow::from(
            "gn binary not found; specify configuration value gn.binaryPath",
        )));
    };

    let document = context
        .storage
        .lock()
        .unwrap()
        .read(&file_path)
        .map_err(into_rpc_error)?;

    let mut process = Command::new(gn_path)
        .args(["format", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(into_rpc_error)?;

    let write_task = {
        let mut stdin = process.stdin.take().unwrap();
        let document = document.clone();
        async move {
            // Drop stdin on completion of the task to close the pipe.
            stdin.write_all(document.data.as_bytes()).await
        }
    };

    let mut stdout = process.stdout.take().unwrap();
    let mut formatted = String::new();
    let read_task = stdout.read_to_string(&mut formatted);

    let io_result = tokio::try_join!(write_task, read_task);

    // Check the status first.
    let status = process.wait().await.unwrap();
    if !status.success() {
        return Err(new_rpc_error(Cow::from(format!(
            "gn format failed with status {}",
            status.code().unwrap_or(-1)
        ))));
    }

    // Check the IO result then.
    io_result.map_err(into_rpc_error)?;

    let whole_range = document
        .line_index
        .range(Span::new(&document.data, 0, document.data.len()).unwrap());
    Ok(Some(vec![TextEdit {
        range: whole_range,
        new_text: formatted,
    }]))
}
