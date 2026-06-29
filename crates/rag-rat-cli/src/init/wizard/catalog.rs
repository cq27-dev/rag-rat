//! Repo-local init wizard catalogs.
//!
//! These entries drive selector UX only. Runtime still persists and consumes plain
//! `[llm.embedding.remote] cookbook` / `gpu` strings.

use toml_edit::{DocumentMut, Item, TableLike};

use super::draft::{MODAL_GPUS, RUNPOD_GPUS};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CookbookEntry {
    pub key: String,
    pub label: String,
    pub command: String,
    pub gpus: Vec<String>,
}

impl CookbookEntry {
    fn builtin(key: &str, label: &str, command: &str, gpus: &[&str]) -> Self {
        Self {
            key: key.to_string(),
            label: label.to_string(),
            command: command.to_string(),
            gpus: gpus.iter().map(|gpu| (*gpu).to_string()).collect(),
        }
    }

    pub(crate) fn custom_current(command: &str, gpu: Option<&str>) -> Self {
        Self {
            key: "custom".to_string(),
            label: format!("Custom: {command}"),
            command: command.to_string(),
            gpus: gpu.into_iter().map(str::to_string).collect(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CookbookCatalog {
    entries: Vec<CookbookEntry>,
}

impl Default for CookbookCatalog {
    fn default() -> Self {
        Self::builtins()
    }
}

impl CookbookCatalog {
    pub(crate) fn builtins() -> Self {
        Self {
            entries: vec![
                CookbookEntry::builtin("modal", "Modal", "@rag-rat/cookbook modal", MODAL_GPUS),
                CookbookEntry::builtin("runpod", "RunPod", "@rag-rat/cookbook runpod", RUNPOD_GPUS),
            ],
        }
    }

    pub(crate) fn from_raw(raw: &str) -> Self {
        let Ok(doc) = raw.parse::<DocumentMut>() else {
            return Self::builtins();
        };
        Self::from_doc(&doc)
    }

    pub(crate) fn from_doc(doc: &DocumentMut) -> Self {
        let mut catalog = Self::builtins();
        let Some(cookbooks) = doc
            .get("init")
            .and_then(Item::as_table_like)
            .and_then(|init| init.get("cookbooks"))
            .and_then(Item::as_table_like)
        else {
            return catalog;
        };

        for (key, item) in cookbooks.iter() {
            let Some(table) = item.as_table_like() else {
                continue;
            };
            catalog.merge_entry(key, table);
        }
        catalog
    }

    pub(crate) fn entries(&self) -> &[CookbookEntry] {
        &self.entries
    }

    pub(crate) fn find_command(&self, command: &str) -> Option<usize> {
        let command = command.trim();
        self.entries.iter().position(|entry| entry.command == command)
    }

    fn merge_entry(&mut self, key: &str, table: &dyn TableLike) {
        let existing = self.entries.iter().position(|entry| entry.key == key);
        let base = existing.and_then(|index| self.entries.get(index).cloned());
        let command = string_value(table, "command")
            .filter(|command| !command.trim().is_empty())
            .or_else(|| base.as_ref().map(|entry| entry.command.clone()));
        let Some(command) = command else {
            return;
        };
        let label = string_value(table, "label")
            .filter(|label| !label.trim().is_empty())
            .or_else(|| base.as_ref().map(|entry| entry.label.clone()))
            .unwrap_or_else(|| key.to_string());
        let gpus = string_array(table, "gpus")
            .or_else(|| base.as_ref().map(|entry| entry.gpus.clone()))
            .unwrap_or_default();
        let entry = CookbookEntry { key: key.to_string(), label, command, gpus };
        if let Some(index) = existing {
            self.entries[index] = entry;
        } else {
            self.entries.push(entry);
        }
    }
}

fn string_value(table: &dyn TableLike, key: &str) -> Option<String> {
    table.get(key)?.as_str().map(|value| value.trim().to_string())
}

fn string_array(table: &dyn TableLike, key: &str) -> Option<Vec<String>> {
    let array = table.get(key)?.as_array()?;
    Some(
        array
            .iter()
            .filter_map(|value| value.as_str().map(str::trim))
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_catalog_extends_builtins_with_custom_cookbook() {
        let catalog = CookbookCatalog::from_raw(
            r#"
            [init.cookbooks.acme]
            label = "Acme GPU"
            command = "./recipes/acme.mjs --pool small"
            gpus = ["a10-small", "h100-large"]
            "#,
        );

        assert_eq!(catalog.entries()[0].key, "modal");
        let acme = catalog.entries().iter().find(|entry| entry.key == "acme").unwrap();
        assert_eq!(acme.label, "Acme GPU");
        assert_eq!(acme.command, "./recipes/acme.mjs --pool small");
        assert_eq!(acme.gpus, ["a10-small", "h100-large"]);
    }

    #[test]
    fn repo_catalog_overrides_builtin_gpu_list_without_repeating_command() {
        let catalog = CookbookCatalog::from_raw(
            r#"
            [init.cookbooks.modal]
            gpus = ["L4", "H200"]
            "#,
        );

        let modal = catalog.entries().iter().find(|entry| entry.key == "modal").unwrap();
        assert_eq!(modal.command, "@rag-rat/cookbook modal");
        assert_eq!(modal.gpus, ["L4", "H200"]);
    }

    #[test]
    fn repo_catalog_ignores_custom_entries_without_commands() {
        let catalog = CookbookCatalog::from_raw(
            r#"
            [init.cookbooks.empty]
            label = "No command"
            gpus = ["A100"]
            "#,
        );

        assert!(catalog.entries().iter().all(|entry| entry.key != "empty"));
    }
}
