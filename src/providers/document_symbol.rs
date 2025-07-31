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

use tower_lsp::lsp_types::{DocumentSymbolParams, DocumentSymbolResponse};

use crate::{
    error::{Error, Result},
    server::RequestContext,
};

pub async fn document_symbol(
    context: &RequestContext,
    params: DocumentSymbolParams,
) -> Result<Option<DocumentSymbolResponse>> {
    let Ok(path) = params.text_document.uri.to_file_path() else {
        return Err(Error::General(format!(
            "invalid file URI: {}",
            params.text_document.uri
        )));
    };

    let current_file = context
        .analyzer
        .lock()
        .unwrap()
        .analyze(&path, context.ticket)?;

    Ok(Some(DocumentSymbolResponse::Nested(
        current_file.symbols.clone(),
    )))
}
