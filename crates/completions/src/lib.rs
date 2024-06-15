#![feature(anonymous_lifetime_in_impl_trait)]

mod context;
mod entity;
mod entity_viewer;
mod parser;
mod querier;
mod settings;
mod to_ui;

use std::path::{Path, PathBuf};

use context::Context;
use entity_viewer::EntityViewer;
use moxide_config::Settings;
use parser::Parser;
use querier::Querier;
use settings::SettingsAdapter;
use to_ui::completion_response;
use tower_lsp::lsp_types::{CompletionParams, CompletionResponse};
use vault::Vault;

pub fn get_completions(
    vault: &Vault,
    _files: &[PathBuf],
    params: &CompletionParams,
    path: &Path,
    settings: &Settings,
) -> Option<CompletionResponse> {
    let cx = Context::new(
        Parser::new(vault),
        Querier::new(vault),
        SettingsAdapter::new(settings),
        EntityViewer::new(vault),
    );

    let location = Location {
        path,
        line: params.text_document_position.position.line as usize,
        character: params.text_document_position.position.character as usize,
    };

    completions(&cx, location)
}

fn completions(cx: &Context, location: Location) -> Option<CompletionResponse> {
    let (named_entity_query, query_syntax_info) = cx.parser().parse_link(location)?;
    let named_entities = cx.querier().query(named_entity_query);
    Some(completion_response(&cx, &query_syntax_info, named_entities))
}

struct Location<'fs> {
    path: &'fs Path,
    line: usize,
    character: usize,
}
