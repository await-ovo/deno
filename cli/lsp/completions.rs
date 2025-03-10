// Copyright 2018-2023 the Deno authors. All rights reserved. MIT license.

use super::client::Client;
use super::config::ConfigSnapshot;
use super::documents::Documents;
use super::documents::DocumentsFilter;
use super::lsp_custom;
use super::npm::CliNpmSearchApi;
use super::npm::NpmSearchApi;
use super::registries::ModuleRegistry;
use super::tsc;

use crate::util::path::is_importable_ext;
use crate::util::path::relative_specifier;
use crate::util::path::specifier_to_file_path;

use deno_ast::LineAndColumnIndex;
use deno_ast::SourceTextInfo;
use deno_core::normalize_path;
use deno_core::resolve_path;
use deno_core::resolve_url;
use deno_core::serde::Deserialize;
use deno_core::serde::Serialize;
use deno_core::serde_json::json;
use deno_core::url::Position;
use deno_core::ModuleSpecifier;
use import_map::ImportMap;
use once_cell::sync::Lazy;
use regex::Regex;
use std::sync::Arc;
use tower_lsp::lsp_types as lsp;

static FILE_PROTO_RE: Lazy<Regex> =
  lazy_regex::lazy_regex!(r#"^file:/{2}(?:/[A-Za-z]:)?"#);

const CURRENT_PATH: &str = ".";
const PARENT_PATH: &str = "..";
const LOCAL_PATHS: &[&str] = &[CURRENT_PATH, PARENT_PATH];
pub(crate) const IMPORT_COMMIT_CHARS: &[&str] = &["\"", "'"];

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompletionItemData {
  #[serde(skip_serializing_if = "Option::is_none")]
  pub documentation: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub tsc: Option<tsc::CompletionItemData>,
}

/// Check if the origin can be auto-configured for completions, and if so, send
/// a notification to the client.
async fn check_auto_config_registry(
  url_str: &str,
  config: &ConfigSnapshot,
  client: &Client,
  module_registries: &ModuleRegistry,
) {
  // check to see if auto discovery is enabled
  if config.settings.workspace.suggest.imports.auto_discover {
    if let Ok(specifier) = resolve_url(url_str) {
      let scheme = specifier.scheme();
      let path = &specifier[Position::BeforePath..];
      if scheme.starts_with("http")
        && !path.is_empty()
        && url_str.ends_with(path)
      {
        // check to see if this origin is already explicitly set
        let in_config =
          config.settings.workspace.suggest.imports.hosts.iter().any(
            |(h, _)| {
              resolve_url(h).map(|u| u.origin()) == Ok(specifier.origin())
            },
          );
        // if it isn't in the configuration, we will check to see if it supports
        // suggestions and send a notification to the client.
        if !in_config {
          let origin = specifier.origin().ascii_serialization();
          let suggestions =
            module_registries.check_origin(&origin).await.is_ok();
          // we are only sending registry state when enabled now, but changing
          // the custom notification would make older versions of the plugin
          // incompatible.
          // TODO(@kitsonk) clean up protocol when doing v2 of suggestions
          if suggestions {
            client.send_registry_state_notification(
              lsp_custom::RegistryStateNotificationParams {
                origin,
                suggestions,
              },
            );
          }
        }
      }
    }
  }
}

/// Ranges from the graph for specifiers include the leading and maybe trailing quote,
/// which we want to ignore when replacing text.
fn to_narrow_lsp_range(
  text_info: &SourceTextInfo,
  range: &deno_graph::Range,
) -> lsp::Range {
  let end_byte_index = text_info
    .loc_to_source_pos(LineAndColumnIndex {
      line_index: range.end.line,
      column_index: range.end.character,
    })
    .as_byte_index(text_info.range().start);
  let start_byte_index = text_info
    .loc_to_source_pos(LineAndColumnIndex {
      line_index: range.start.line,
      column_index: range.start.character,
    })
    .as_byte_index(text_info.range().start);
  let text_bytes = text_info.text_str().as_bytes();
  let is_empty = end_byte_index - 1 == start_byte_index;
  let has_trailing_quote =
    !is_empty && matches!(text_bytes[end_byte_index - 1], b'"' | b'\'');
  lsp::Range {
    start: lsp::Position {
      line: range.start.line as u32,
      // skip the leading quote
      character: (range.start.character + 1) as u32,
    },
    end: lsp::Position {
      line: range.end.line as u32,
      character: if has_trailing_quote {
        range.end.character - 1 // do not include it
      } else {
        range.end.character
      } as u32,
    },
  }
}

/// Given a specifier, a position, and a snapshot, optionally return a
/// completion response, which will be valid import completions for the specific
/// context.
#[allow(clippy::too_many_arguments)]
pub async fn get_import_completions(
  specifier: &ModuleSpecifier,
  position: &lsp::Position,
  config: &ConfigSnapshot,
  client: &Client,
  module_registries: &ModuleRegistry,
  npm_search_api: &CliNpmSearchApi,
  documents: &Documents,
  maybe_import_map: Option<Arc<ImportMap>>,
) -> Option<lsp::CompletionResponse> {
  let document = documents.get(specifier)?;
  let (text, _, range) = document.get_maybe_dependency(position)?;
  let range = to_narrow_lsp_range(&document.text_info(), &range);
  if let Some(completion_list) = get_import_map_completions(
    specifier,
    &text,
    &range,
    maybe_import_map.clone(),
    documents,
  ) {
    // completions for import map specifiers
    Some(lsp::CompletionResponse::List(completion_list))
  } else if text.starts_with("./") || text.starts_with("../") {
    // completions for local relative modules
    Some(lsp::CompletionResponse::List(lsp::CompletionList {
      is_incomplete: false,
      items: get_local_completions(specifier, &text, &range)?,
    }))
  } else if text.starts_with("npm:") {
    let items =
      get_npm_completions(specifier, &text, &range, npm_search_api).await?;
    Some(lsp::CompletionResponse::List(lsp::CompletionList {
      is_incomplete: !items.is_empty(),
      items,
    }))
  } else if !text.is_empty() {
    // completion of modules from a module registry or cache
    check_auto_config_registry(&text, config, client, module_registries).await;
    let offset = if position.character > range.start.character {
      (position.character - range.start.character) as usize
    } else {
      0
    };
    let maybe_list = module_registries
      .get_completions(&text, offset, &range, |specifier| {
        documents.exists(specifier)
      })
      .await;
    let list = maybe_list.unwrap_or_else(|| lsp::CompletionList {
      items: get_workspace_completions(specifier, &text, &range, documents),
      is_incomplete: false,
    });
    Some(lsp::CompletionResponse::List(list))
  } else {
    // the import specifier is empty, so provide all possible specifiers we are
    // aware of
    let mut items: Vec<lsp::CompletionItem> = LOCAL_PATHS
      .iter()
      .map(|s| lsp::CompletionItem {
        label: s.to_string(),
        kind: Some(lsp::CompletionItemKind::FOLDER),
        detail: Some("(local)".to_string()),
        sort_text: Some("1".to_string()),
        insert_text: Some(s.to_string()),
        commit_characters: Some(
          IMPORT_COMMIT_CHARS.iter().map(|&c| c.into()).collect(),
        ),
        ..Default::default()
      })
      .collect();
    let mut is_incomplete = false;
    if let Some(import_map) = maybe_import_map {
      items.extend(get_base_import_map_completions(import_map.as_ref()));
    }
    if let Some(origin_items) =
      module_registries.get_origin_completions(&text, &range)
    {
      is_incomplete = origin_items.is_incomplete;
      items.extend(origin_items.items);
    }
    Some(lsp::CompletionResponse::List(lsp::CompletionList {
      is_incomplete,
      items,
    }))
  }
}

/// When the specifier is an empty string, return all the keys from the import
/// map as completion items.
fn get_base_import_map_completions(
  import_map: &ImportMap,
) -> Vec<lsp::CompletionItem> {
  import_map
    .imports()
    .keys()
    .map(|key| {
      // for some strange reason, keys that start with `/` get stored in the
      // import map as `file:///`, and so when we pull the keys out, we need to
      // change the behavior
      let mut label = if key.starts_with("file://") {
        FILE_PROTO_RE.replace(key, "").to_string()
      } else {
        key.to_string()
      };
      let kind = if key.ends_with('/') {
        label.pop();
        Some(lsp::CompletionItemKind::FOLDER)
      } else {
        Some(lsp::CompletionItemKind::FILE)
      };
      lsp::CompletionItem {
        label: label.clone(),
        kind,
        detail: Some("(import map)".to_string()),
        sort_text: Some(label.clone()),
        insert_text: Some(label),
        commit_characters: Some(
          IMPORT_COMMIT_CHARS.iter().map(|&c| c.into()).collect(),
        ),
        ..Default::default()
      }
    })
    .collect()
}

/// Given an existing specifier, return any completions that could apply derived
/// from the import map. There are two main type of import map keys, those that
/// a literal, which don't end in `/`, which expects a one for one replacement
/// of specifier to specifier, and then those that end in `/` which indicates
/// that the path post the `/` should be appended to resolved specifier. This
/// handles both cases, pulling any completions from the workspace completions.
fn get_import_map_completions(
  specifier: &ModuleSpecifier,
  text: &str,
  range: &lsp::Range,
  maybe_import_map: Option<Arc<ImportMap>>,
  documents: &Documents,
) -> Option<lsp::CompletionList> {
  if !text.is_empty() {
    if let Some(import_map) = maybe_import_map {
      let mut items = Vec::new();
      for key in import_map.imports().keys() {
        // for some reason, the import_map stores keys that begin with `/` as
        // `file:///` in its index, so we have to reverse that here
        let key = if key.starts_with("file://") {
          FILE_PROTO_RE.replace(key, "").to_string()
        } else {
          key.to_string()
        };
        if text.starts_with(&key) && key.ends_with('/') {
          if let Ok(resolved) = import_map.resolve(&key, specifier) {
            let resolved = resolved.to_string();
            let workspace_items: Vec<lsp::CompletionItem> = documents
              .documents(DocumentsFilter::AllDiagnosable)
              .into_iter()
              .filter_map(|d| {
                let specifier_str = d.specifier().to_string();
                let new_text = specifier_str.replace(&resolved, &key);
                if specifier_str.starts_with(&resolved) {
                  let label = specifier_str.replace(&resolved, "");
                  let text_edit =
                    Some(lsp::CompletionTextEdit::Edit(lsp::TextEdit {
                      range: *range,
                      new_text: new_text.clone(),
                    }));
                  Some(lsp::CompletionItem {
                    label,
                    kind: Some(lsp::CompletionItemKind::MODULE),
                    detail: Some("(import map)".to_string()),
                    sort_text: Some("1".to_string()),
                    filter_text: Some(new_text),
                    text_edit,
                    commit_characters: Some(
                      IMPORT_COMMIT_CHARS.iter().map(|&c| c.into()).collect(),
                    ),
                    ..Default::default()
                  })
                } else {
                  None
                }
              })
              .collect();
            items.extend(workspace_items);
          }
        } else if key.starts_with(text) && text != key {
          let mut label = key.to_string();
          let kind = if key.ends_with('/') {
            label.pop();
            Some(lsp::CompletionItemKind::FOLDER)
          } else {
            Some(lsp::CompletionItemKind::MODULE)
          };
          let text_edit = Some(lsp::CompletionTextEdit::Edit(lsp::TextEdit {
            range: *range,
            new_text: label.clone(),
          }));
          items.push(lsp::CompletionItem {
            label,
            kind,
            detail: Some("(import map)".to_string()),
            sort_text: Some("1".to_string()),
            text_edit,
            commit_characters: Some(
              IMPORT_COMMIT_CHARS.iter().map(|&c| c.into()).collect(),
            ),
            ..Default::default()
          });
        }
        if !items.is_empty() {
          return Some(lsp::CompletionList {
            items,
            is_incomplete: false,
          });
        }
      }
    }
  }
  None
}

/// Return local completions that are relative to the base specifier.
fn get_local_completions(
  base: &ModuleSpecifier,
  current: &str,
  range: &lsp::Range,
) -> Option<Vec<lsp::CompletionItem>> {
  if base.scheme() != "file" {
    return None;
  }

  let mut base_path = specifier_to_file_path(base).ok()?;
  base_path.pop();
  let mut current_path = normalize_path(base_path.join(current));
  // if the current text does not end in a `/` then we are still selecting on
  // the parent and should show all completions from there.
  let is_parent = if !current.ends_with('/') {
    current_path.pop();
    true
  } else {
    false
  };
  let cwd = std::env::current_dir().ok()?;
  if current_path.is_dir() {
    let items = std::fs::read_dir(current_path).ok()?;
    Some(
      items
        .filter_map(|de| {
          let de = de.ok()?;
          let label = de.path().file_name()?.to_string_lossy().to_string();
          let entry_specifier = resolve_path(de.path().to_str()?, &cwd).ok()?;
          if &entry_specifier == base {
            return None;
          }
          let full_text = relative_specifier(base, &entry_specifier)?;
          // this weeds out situations where we are browsing in the parent, but
          // we want to filter out non-matches when the completion is manually
          // invoked by the user, but still allows for things like `../src/../`
          // which is silly, but no reason to not allow it.
          if is_parent && !full_text.starts_with(current) {
            return None;
          }
          let text_edit = Some(lsp::CompletionTextEdit::Edit(lsp::TextEdit {
            range: *range,
            new_text: full_text.clone(),
          }));
          let filter_text = if full_text.starts_with(current) {
            Some(full_text)
          } else {
            Some(format!("{current}{label}"))
          };
          match de.file_type() {
            Ok(file_type) if file_type.is_dir() => Some(lsp::CompletionItem {
              label,
              kind: Some(lsp::CompletionItemKind::FOLDER),
              filter_text,
              sort_text: Some("1".to_string()),
              text_edit,
              commit_characters: Some(
                IMPORT_COMMIT_CHARS.iter().map(|&c| c.into()).collect(),
              ),
              ..Default::default()
            }),
            Ok(file_type) if file_type.is_file() => {
              if is_importable_ext(&de.path()) {
                Some(lsp::CompletionItem {
                  label,
                  kind: Some(lsp::CompletionItemKind::FILE),
                  detail: Some("(local)".to_string()),
                  filter_text,
                  sort_text: Some("1".to_string()),
                  text_edit,
                  commit_characters: Some(
                    IMPORT_COMMIT_CHARS.iter().map(|&c| c.into()).collect(),
                  ),
                  ..Default::default()
                })
              } else {
                None
              }
            }
            _ => None,
          }
        })
        .collect(),
    )
  } else {
    None
  }
}

fn get_relative_specifiers(
  base: &ModuleSpecifier,
  specifiers: Vec<ModuleSpecifier>,
) -> Vec<String> {
  specifiers
    .iter()
    .filter_map(|s| {
      if s != base {
        Some(relative_specifier(base, s).unwrap_or_else(|| s.to_string()))
      } else {
        None
      }
    })
    .collect()
}

/// Get completions for `npm:` specifiers.
async fn get_npm_completions(
  referrer: &ModuleSpecifier,
  specifier: &str,
  range: &lsp::Range,
  npm_search_api: &impl NpmSearchApi,
) -> Option<Vec<lsp::CompletionItem>> {
  debug_assert!(specifier.starts_with("npm:"));
  let bare_specifier = &specifier[4..];

  // Find the index of the '@' delimiting the package name and version, if any.
  let v_index = if bare_specifier.starts_with('@') {
    bare_specifier
      .find('/')
      .filter(|idx| !bare_specifier[1..*idx].is_empty())
      .and_then(|idx| {
        bare_specifier[idx..]
          .find('@')
          .filter(|idx2| !bare_specifier[(idx + 1)..*idx2].is_empty())
          .filter(|idx2| !bare_specifier[(idx + 1)..*idx2].contains('/'))
      })
  } else {
    bare_specifier
      .find('@')
      .filter(|idx| !bare_specifier[..*idx].is_empty())
      .filter(|idx| !bare_specifier[..*idx].contains('/'))
  };

  // First try to match `npm:some-package@<version-to-complete>`.
  if let Some(v_index) = v_index {
    let package_name = &bare_specifier[..v_index];
    let v_prefix = &bare_specifier[(v_index + 1)..];
    let versions = &npm_search_api
      .package_info(package_name)
      .await
      .ok()?
      .versions;
    let mut versions = versions.keys().collect::<Vec<_>>();
    versions.sort();
    let items = versions
      .into_iter()
      .rev()
      .enumerate()
      .filter_map(|(idx, version)| {
        let version = version.to_string();
        if !version.starts_with(v_prefix) {
          return None;
        }
        let specifier = format!("npm:{}@{}", package_name, &version);
        let command = Some(lsp::Command {
          title: "".to_string(),
          command: "deno.cache".to_string(),
          arguments: Some(vec![json!([&specifier]), json!(referrer)]),
        });
        let text_edit = Some(lsp::CompletionTextEdit::Edit(lsp::TextEdit {
          range: *range,
          new_text: specifier.clone(),
        }));
        Some(lsp::CompletionItem {
          label: specifier,
          kind: Some(lsp::CompletionItemKind::FILE),
          detail: Some("(npm)".to_string()),
          sort_text: Some(format!("{:0>10}", idx + 1)),
          text_edit,
          command,
          commit_characters: Some(
            IMPORT_COMMIT_CHARS.iter().map(|&c| c.into()).collect(),
          ),
          ..Default::default()
        })
      })
      .collect();
    return Some(items);
  }

  // Otherwise match `npm:<package-to-complete>`.
  let names = npm_search_api.search(bare_specifier).await.ok()?;
  let items = names
    .iter()
    .enumerate()
    .map(|(idx, name)| {
      let specifier = format!("npm:{}", name);
      let command = Some(lsp::Command {
        title: "".to_string(),
        command: "deno.cache".to_string(),
        arguments: Some(vec![json!([&specifier]), json!(referrer)]),
      });
      let text_edit = Some(lsp::CompletionTextEdit::Edit(lsp::TextEdit {
        range: *range,
        new_text: specifier.clone(),
      }));
      lsp::CompletionItem {
        label: specifier,
        kind: Some(lsp::CompletionItemKind::FILE),
        detail: Some("(npm)".to_string()),
        sort_text: Some(format!("{:0>10}", idx + 1)),
        text_edit,
        command,
        commit_characters: Some(
          IMPORT_COMMIT_CHARS.iter().map(|&c| c.into()).collect(),
        ),
        ..Default::default()
      }
    })
    .collect();
  Some(items)
}

/// Get workspace completions that include modules in the Deno cache which match
/// the current specifier string.
fn get_workspace_completions(
  specifier: &ModuleSpecifier,
  current: &str,
  range: &lsp::Range,
  documents: &Documents,
) -> Vec<lsp::CompletionItem> {
  let workspace_specifiers = documents
    .documents(DocumentsFilter::AllDiagnosable)
    .into_iter()
    .map(|d| d.specifier().clone())
    .collect();
  let specifier_strings =
    get_relative_specifiers(specifier, workspace_specifiers);
  specifier_strings
    .into_iter()
    .filter_map(|label| {
      if label.starts_with(current) {
        let detail = Some(
          if label.starts_with("http:") || label.starts_with("https:") {
            "(remote)".to_string()
          } else if label.starts_with("data:") {
            "(data)".to_string()
          } else {
            "(local)".to_string()
          },
        );
        let text_edit = Some(lsp::CompletionTextEdit::Edit(lsp::TextEdit {
          range: *range,
          new_text: label.clone(),
        }));
        Some(lsp::CompletionItem {
          label,
          kind: Some(lsp::CompletionItemKind::FILE),
          detail,
          sort_text: Some("1".to_string()),
          text_edit,
          commit_characters: Some(
            IMPORT_COMMIT_CHARS.iter().map(|&c| c.into()).collect(),
          ),
          ..Default::default()
        })
      } else {
        None
      }
    })
    .collect()
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::cache::GlobalHttpCache;
  use crate::cache::HttpCache;
  use crate::lsp::documents::Documents;
  use crate::lsp::documents::LanguageId;
  use crate::lsp::npm::NpmSearchApi;
  use crate::AnyError;
  use async_trait::async_trait;
  use deno_core::resolve_url;
  use deno_graph::Range;
  use deno_npm::registry::NpmPackageInfo;
  use deno_npm::registry::NpmRegistryApi;
  use deno_npm::registry::TestNpmRegistryApi;
  use std::collections::HashMap;
  use std::path::Path;
  use test_util::TempDir;

  #[derive(Default)]
  struct TestNpmSearchApi(
    HashMap<String, Arc<Vec<String>>>,
    TestNpmRegistryApi,
  );

  #[async_trait]
  impl NpmSearchApi for TestNpmSearchApi {
    async fn search(&self, query: &str) -> Result<Arc<Vec<String>>, AnyError> {
      match self.0.get(query) {
        Some(names) => Ok(names.clone()),
        None => Ok(Arc::new(vec![])),
      }
    }

    async fn package_info(
      &self,
      name: &str,
    ) -> Result<Arc<NpmPackageInfo>, AnyError> {
      self.1.package_info(name).await.map_err(|e| e.into())
    }
  }

  fn mock_documents(
    fixtures: &[(&str, &str, i32, LanguageId)],
    source_fixtures: &[(&str, &str)],
    location: &Path,
  ) -> Documents {
    let cache = Arc::new(GlobalHttpCache::new(
      location.to_path_buf(),
      crate::cache::RealDenoCacheEnv,
    ));
    let mut documents = Documents::new(cache);
    for (specifier, source, version, language_id) in fixtures {
      let specifier =
        resolve_url(specifier).expect("failed to create specifier");
      documents.open(specifier, *version, *language_id, (*source).into());
    }
    let http_cache = GlobalHttpCache::new(
      location.to_path_buf(),
      crate::cache::RealDenoCacheEnv,
    );
    for (specifier, source) in source_fixtures {
      let specifier =
        resolve_url(specifier).expect("failed to create specifier");
      http_cache
        .set(&specifier, HashMap::default(), source.as_bytes())
        .expect("could not cache file");
      assert!(
        documents.get(&specifier).is_some(),
        "source could not be setup"
      );
    }
    documents
  }

  fn setup(
    temp_dir: &TempDir,
    documents: &[(&str, &str, i32, LanguageId)],
    sources: &[(&str, &str)],
  ) -> Documents {
    let location = temp_dir.path().join("deps");
    mock_documents(documents, sources, location.as_path())
  }

  #[test]
  fn test_get_relative_specifiers() {
    let base = resolve_url("file:///a/b/c.ts").unwrap();
    let specifiers = vec![
      resolve_url("file:///a/b/c.ts").unwrap(),
      resolve_url("file:///a/b/d.ts").unwrap(),
      resolve_url("file:///a/c/c.ts").unwrap(),
      resolve_url("file:///a/b/d/d.ts").unwrap(),
      resolve_url("https://deno.land/x/a/b/c.ts").unwrap(),
    ];
    assert_eq!(
      get_relative_specifiers(&base, specifiers),
      vec![
        "./d.ts".to_string(),
        "../c/c.ts".to_string(),
        "./d/d.ts".to_string(),
        "https://deno.land/x/a/b/c.ts".to_string(),
      ]
    );
  }

  #[test]
  fn test_get_local_completions() {
    let temp_dir = TempDir::new();
    let fixtures = temp_dir.path().join("fixtures");
    std::fs::create_dir(&fixtures).expect("could not create");
    let dir_a = fixtures.join("a");
    std::fs::create_dir(&dir_a).expect("could not create");
    let dir_b = dir_a.join("b");
    std::fs::create_dir(&dir_b).expect("could not create");
    let file_c = dir_a.join("c.ts");
    std::fs::write(&file_c, b"").expect("could not create");
    let file_d = dir_b.join("d.ts");
    std::fs::write(file_d, b"").expect("could not create");
    let file_e = dir_a.join("e.txt");
    std::fs::write(file_e, b"").expect("could not create");
    let file_f = dir_a.join("f.mjs");
    std::fs::write(file_f, b"").expect("could not create");
    let file_g = dir_a.join("g.json");
    std::fs::write(file_g, b"").expect("could not create");
    let specifier =
      ModuleSpecifier::from_file_path(file_c).expect("could not create");
    let actual = get_local_completions(
      &specifier,
      "./",
      &lsp::Range {
        start: lsp::Position {
          line: 0,
          character: 20,
        },
        end: lsp::Position {
          line: 0,
          character: 22,
        },
      },
    );
    assert!(actual.is_some());
    let actual = actual.unwrap();
    assert_eq!(actual.len(), 3);
    for item in actual {
      match item.text_edit {
        Some(lsp::CompletionTextEdit::Edit(text_edit)) => {
          assert!(["./b", "./f.mjs", "./g.json"]
            .contains(&text_edit.new_text.as_str()));
        }
        _ => unreachable!(),
      }
    }
  }

  #[tokio::test]
  async fn test_get_workspace_completions() {
    let specifier = resolve_url("file:///a/b/c.ts").unwrap();
    let range = lsp::Range {
      start: lsp::Position {
        line: 0,
        character: 20,
      },
      end: lsp::Position {
        line: 0,
        character: 21,
      },
    };
    let temp_dir = TempDir::new();
    let documents = setup(
      &temp_dir,
      &[
        (
          "file:///a/b/c.ts",
          "import * as d from \"h\"",
          1,
          LanguageId::TypeScript,
        ),
        ("file:///a/c.ts", r#""#, 1, LanguageId::TypeScript),
      ],
      &[("https://deno.land/x/a/b/c.ts", "console.log(1);\n")],
    );
    let actual = get_workspace_completions(&specifier, "h", &range, &documents);
    assert_eq!(
      actual,
      vec![lsp::CompletionItem {
        label: "https://deno.land/x/a/b/c.ts".to_string(),
        kind: Some(lsp::CompletionItemKind::FILE),
        detail: Some("(remote)".to_string()),
        sort_text: Some("1".to_string()),
        text_edit: Some(lsp::CompletionTextEdit::Edit(lsp::TextEdit {
          range: lsp::Range {
            start: lsp::Position {
              line: 0,
              character: 20
            },
            end: lsp::Position {
              line: 0,
              character: 21,
            }
          },
          new_text: "https://deno.land/x/a/b/c.ts".to_string(),
        })),
        commit_characters: Some(
          IMPORT_COMMIT_CHARS.iter().map(|&c| c.into()).collect()
        ),
        ..Default::default()
      }]
    );
  }

  #[tokio::test]
  async fn test_get_npm_completions() {
    let npm_search_api = TestNpmSearchApi(
      vec![(
        "puppe".to_string(),
        Arc::new(vec![
          "puppeteer".to_string(),
          "puppeteer-core".to_string(),
          "puppeteer-extra-plugin-stealth".to_string(),
          "puppeteer-extra-plugin".to_string(),
        ]),
      )]
      .into_iter()
      .collect(),
      Default::default(),
    );
    let range = lsp::Range {
      start: lsp::Position {
        line: 0,
        character: 23,
      },
      end: lsp::Position {
        line: 0,
        character: 32,
      },
    };
    let referrer = ModuleSpecifier::parse("file:///referrer.ts").unwrap();
    let actual =
      get_npm_completions(&referrer, "npm:puppe", &range, &npm_search_api)
        .await
        .unwrap();
    assert_eq!(
      actual,
      vec![
        lsp::CompletionItem {
          label: "npm:puppeteer".to_string(),
          kind: Some(lsp::CompletionItemKind::FILE),
          detail: Some("(npm)".to_string()),
          sort_text: Some("0000000001".to_string()),
          text_edit: Some(lsp::CompletionTextEdit::Edit(lsp::TextEdit {
            range,
            new_text: "npm:puppeteer".to_string(),
          })),
          command: Some(lsp::Command {
            title: "".to_string(),
            command: "deno.cache".to_string(),
            arguments: Some(vec![json!(["npm:puppeteer"]), json!(&referrer)])
          }),
          commit_characters: Some(
            IMPORT_COMMIT_CHARS.iter().map(|&c| c.into()).collect()
          ),
          ..Default::default()
        },
        lsp::CompletionItem {
          label: "npm:puppeteer-core".to_string(),
          kind: Some(lsp::CompletionItemKind::FILE),
          detail: Some("(npm)".to_string()),
          sort_text: Some("0000000002".to_string()),
          text_edit: Some(lsp::CompletionTextEdit::Edit(lsp::TextEdit {
            range,
            new_text: "npm:puppeteer-core".to_string(),
          })),
          command: Some(lsp::Command {
            title: "".to_string(),
            command: "deno.cache".to_string(),
            arguments: Some(vec![
              json!(["npm:puppeteer-core"]),
              json!(&referrer)
            ])
          }),
          commit_characters: Some(
            IMPORT_COMMIT_CHARS.iter().map(|&c| c.into()).collect()
          ),
          ..Default::default()
        },
        lsp::CompletionItem {
          label: "npm:puppeteer-extra-plugin-stealth".to_string(),
          kind: Some(lsp::CompletionItemKind::FILE),
          detail: Some("(npm)".to_string()),
          sort_text: Some("0000000003".to_string()),
          text_edit: Some(lsp::CompletionTextEdit::Edit(lsp::TextEdit {
            range,
            new_text: "npm:puppeteer-extra-plugin-stealth".to_string(),
          })),
          command: Some(lsp::Command {
            title: "".to_string(),
            command: "deno.cache".to_string(),
            arguments: Some(vec![
              json!(["npm:puppeteer-extra-plugin-stealth"]),
              json!(&referrer)
            ])
          }),
          commit_characters: Some(
            IMPORT_COMMIT_CHARS.iter().map(|&c| c.into()).collect()
          ),
          ..Default::default()
        },
        lsp::CompletionItem {
          label: "npm:puppeteer-extra-plugin".to_string(),
          kind: Some(lsp::CompletionItemKind::FILE),
          detail: Some("(npm)".to_string()),
          sort_text: Some("0000000004".to_string()),
          text_edit: Some(lsp::CompletionTextEdit::Edit(lsp::TextEdit {
            range,
            new_text: "npm:puppeteer-extra-plugin".to_string(),
          })),
          command: Some(lsp::Command {
            title: "".to_string(),
            command: "deno.cache".to_string(),
            arguments: Some(vec![
              json!(["npm:puppeteer-extra-plugin"]),
              json!(&referrer)
            ])
          }),
          commit_characters: Some(
            IMPORT_COMMIT_CHARS.iter().map(|&c| c.into()).collect()
          ),
          ..Default::default()
        },
      ]
    );
  }

  #[tokio::test]
  async fn test_get_npm_completions_for_versions() {
    let npm_search_api = TestNpmSearchApi::default();
    npm_search_api
      .1
      .ensure_package_version("puppeteer", "20.9.0");
    npm_search_api
      .1
      .ensure_package_version("puppeteer", "21.0.0");
    npm_search_api
      .1
      .ensure_package_version("puppeteer", "21.0.1");
    npm_search_api
      .1
      .ensure_package_version("puppeteer", "21.0.2");
    let range = lsp::Range {
      start: lsp::Position {
        line: 0,
        character: 23,
      },
      end: lsp::Position {
        line: 0,
        character: 37,
      },
    };
    let referrer = ModuleSpecifier::parse("file:///referrer.ts").unwrap();
    let actual =
      get_npm_completions(&referrer, "npm:puppeteer@", &range, &npm_search_api)
        .await
        .unwrap();
    assert_eq!(
      actual,
      vec![
        lsp::CompletionItem {
          label: "npm:puppeteer@21.0.2".to_string(),
          kind: Some(lsp::CompletionItemKind::FILE),
          detail: Some("(npm)".to_string()),
          sort_text: Some("0000000001".to_string()),
          text_edit: Some(lsp::CompletionTextEdit::Edit(lsp::TextEdit {
            range,
            new_text: "npm:puppeteer@21.0.2".to_string(),
          })),
          command: Some(lsp::Command {
            title: "".to_string(),
            command: "deno.cache".to_string(),
            arguments: Some(vec![
              json!(["npm:puppeteer@21.0.2"]),
              json!(&referrer)
            ])
          }),
          commit_characters: Some(
            IMPORT_COMMIT_CHARS.iter().map(|&c| c.into()).collect()
          ),
          ..Default::default()
        },
        lsp::CompletionItem {
          label: "npm:puppeteer@21.0.1".to_string(),
          kind: Some(lsp::CompletionItemKind::FILE),
          detail: Some("(npm)".to_string()),
          sort_text: Some("0000000002".to_string()),
          text_edit: Some(lsp::CompletionTextEdit::Edit(lsp::TextEdit {
            range,
            new_text: "npm:puppeteer@21.0.1".to_string(),
          })),
          command: Some(lsp::Command {
            title: "".to_string(),
            command: "deno.cache".to_string(),
            arguments: Some(vec![
              json!(["npm:puppeteer@21.0.1"]),
              json!(&referrer)
            ])
          }),
          commit_characters: Some(
            IMPORT_COMMIT_CHARS.iter().map(|&c| c.into()).collect()
          ),
          ..Default::default()
        },
        lsp::CompletionItem {
          label: "npm:puppeteer@21.0.0".to_string(),
          kind: Some(lsp::CompletionItemKind::FILE),
          detail: Some("(npm)".to_string()),
          sort_text: Some("0000000003".to_string()),
          text_edit: Some(lsp::CompletionTextEdit::Edit(lsp::TextEdit {
            range,
            new_text: "npm:puppeteer@21.0.0".to_string(),
          })),
          command: Some(lsp::Command {
            title: "".to_string(),
            command: "deno.cache".to_string(),
            arguments: Some(vec![
              json!(["npm:puppeteer@21.0.0"]),
              json!(&referrer)
            ])
          }),
          commit_characters: Some(
            IMPORT_COMMIT_CHARS.iter().map(|&c| c.into()).collect()
          ),
          ..Default::default()
        },
        lsp::CompletionItem {
          label: "npm:puppeteer@20.9.0".to_string(),
          kind: Some(lsp::CompletionItemKind::FILE),
          detail: Some("(npm)".to_string()),
          sort_text: Some("0000000004".to_string()),
          text_edit: Some(lsp::CompletionTextEdit::Edit(lsp::TextEdit {
            range,
            new_text: "npm:puppeteer@20.9.0".to_string(),
          })),
          command: Some(lsp::Command {
            title: "".to_string(),
            command: "deno.cache".to_string(),
            arguments: Some(vec![
              json!(["npm:puppeteer@20.9.0"]),
              json!(&referrer)
            ])
          }),
          commit_characters: Some(
            IMPORT_COMMIT_CHARS.iter().map(|&c| c.into()).collect()
          ),
          ..Default::default()
        },
      ]
    );
  }

  #[test]
  fn test_to_narrow_lsp_range() {
    let text_info = SourceTextInfo::from_string(r#""te""#.to_string());
    let range = to_narrow_lsp_range(
      &text_info,
      &Range {
        specifier: ModuleSpecifier::parse("https://deno.land").unwrap(),
        start: deno_graph::Position {
          line: 0,
          character: 0,
        },
        end: deno_graph::Position {
          line: 0,
          character: text_info.text_str().chars().count(),
        },
      },
    );
    assert_eq!(range.start.character, 1);
    assert_eq!(
      range.end.character,
      (text_info.text_str().chars().count() - 1) as u32
    );
  }

  #[test]
  fn test_to_narrow_lsp_range_no_trailing_quote() {
    let text_info = SourceTextInfo::from_string(r#""te"#.to_string());
    let range = to_narrow_lsp_range(
      &text_info,
      &Range {
        specifier: ModuleSpecifier::parse("https://deno.land").unwrap(),
        start: deno_graph::Position {
          line: 0,
          character: 0,
        },
        end: deno_graph::Position {
          line: 0,
          character: text_info.text_str().chars().count(),
        },
      },
    );
    assert_eq!(range.start.character, 1);
    assert_eq!(
      range.end.character,
      text_info.text_str().chars().count() as u32
    );
  }
}
