// SPDX-License-Identifier: MIT
use crate::{Result, ToolError, fail, http_mcp::HttpClient, mcp, skills::Skills};
use serde_json::{Value, json};
use std::{
    cell::RefCell,
    path::{Path, PathBuf},
};

pub(crate) const INVALID: &str =
    "Invalid extension. Check the name, path, command or HTTP configuration.";
pub(crate) const DUPLICATE: &str = "An extension with this name or folder already exists.";
pub(crate) const LIMIT: &str = "Maximum number of Assistant extensions reached.";
pub(crate) const UNAVAILABLE: &str = "Assistant extensions could not be saved.";

pub(crate) struct Extensions {
    file: PathBuf,
    managed: RefCell<Value>,
    base: Value,
}

impl Extensions {
    pub(crate) fn new(state: &Path, base: &Value) -> Result<Self> {
        let file = state.join("extensions.json");
        let managed = match std::fs::symlink_metadata(&file) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                json!({"servers":{}, "folders":{}})
            }
            _ => crate::service::config_file(&file)?,
        };
        if !managed["servers"].is_object() || !managed["folders"].is_object() {
            return fail(INVALID);
        }
        Ok(Self {
            file,
            managed: RefCell::new(managed),
            base: base.clone(),
        })
    }

    pub(crate) fn summary(&self) -> Value {
        let data = self.managed.borrow();
        let servers = data["servers"].as_object().into_iter().flatten()
            .map(|(name, config)| json!({"name":name, "enabled":config["enabled"], "transport":config["transport"]}))
            .collect::<Vec<_>>();
        let folders = data["folders"]
            .as_object()
            .into_iter()
            .flatten()
            .map(|(path, enabled)| json!({"path":path, "enabled":enabled}))
            .collect::<Vec<_>>();
        json!({"servers":servers, "folders":folders})
    }

    pub(crate) fn servers(&self) -> Value {
        let mut servers = self.base["mcp_servers"]
            .as_object()
            .cloned()
            .unwrap_or_default();
        if let Some(managed) = self.managed.borrow()["servers"].as_object() {
            servers.extend(managed.iter().map(|(name, config)| {
                let mut config = config.clone();
                config["managed"] = json!(true);
                (name.clone(), config)
            }));
        }
        json!(servers)
    }

    pub(crate) fn skills(&self, base: &Skills) -> Skills {
        let mut directories = base.directories.clone();
        for (path, enabled) in self.managed.borrow()["folders"]
            .as_object()
            .into_iter()
            .flatten()
        {
            let path = PathBuf::from(path);
            // A moved or replaced custom folder should not break other skills.
            if enabled == true
                && std::fs::canonicalize(&path).is_ok_and(|actual| actual == path)
                && path.is_dir()
                && !directories.contains(&path)
            {
                directories.push(path);
            }
        }
        Skills { directories }
    }

    pub(crate) fn skill_directory(&self) -> Result<String> {
        self.base["writable_skill_directories"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .find(|directory| {
                self.base["skill_directories"]
                    .as_array()
                    .is_some_and(|paths| paths.contains(&json!(directory)))
                    && Self::folder(directory).is_ok_and(|canonical| canonical == *directory)
            })
            .map(str::to_owned)
            .ok_or_else(|| ToolError("No writable skill folder is configured.".into()))
    }

    pub(crate) fn write_skill(&self, request: &Value) -> Result<Value> {
        let directory = self.skill_directory()?;
        if request["directory"] != directory {
            return fail(INVALID);
        }
        Skills::new(&json!([directory]))?.create(
            &directory,
            request["name"].as_str().unwrap_or_default(),
            request["description"].as_str().unwrap_or_default(),
            request["instructions"].as_str().unwrap_or_default(),
        )
    }

    pub(crate) fn folder(path: &str) -> Result<String> {
        let path = Path::new(path);
        if !path.is_absolute() {
            return fail(INVALID);
        }
        std::fs::canonicalize(path)
            .ok()
            .filter(|path| path.is_dir())
            .and_then(|path| path.into_os_string().into_string().ok())
            .ok_or_else(|| ToolError(INVALID.into()))
    }

    pub(crate) fn validate_server(name: &str, config: &Value) -> Result<()> {
        if !mcp::server_name(name) || config["enabled"].as_bool().is_none() {
            return fail(INVALID);
        }
        match config["transport"].as_str() {
            Some("stdio") => {
                let _ = mcp::StdioClient::new(&config["argv"], &config["environment"])
                    .map_err(|_| ToolError(INVALID.into()))?;
            }
            Some("http") => {
                let _ = HttpClient::with_headers(
                    config["url"].as_str().unwrap_or_default(),
                    None,
                    &config["headers"],
                )
                .map_err(|_| ToolError(INVALID.into()))?;
            }
            _ => return fail(INVALID),
        }
        Ok(())
    }

    pub(crate) fn change(&self, request: &Value) -> Result<Value> {
        let (group, key) = match request["kind"].as_str() {
            Some("server") => ("servers", request["name"].as_str()),
            Some("skill") => ("folders", request["path"].as_str()),
            _ => return fail(INVALID),
        };
        let key = key.ok_or_else(|| ToolError(INVALID.into()))?;
        let mut next = self.managed.borrow().clone();
        let entries = next[group]
            .as_object_mut()
            .ok_or_else(|| ToolError(UNAVAILABLE.into()))?;
        match request["action"].as_str() {
            Some("add") => {
                let (key, item) = if group == "servers" {
                    let config = &request["config"];
                    Self::validate_server(key, config)?;
                    if self.base["mcp_servers"].get(key).is_some() {
                        return fail(DUPLICATE);
                    }
                    let base = self.base["mcp_servers"]
                        .as_object()
                        .map_or(0, serde_json::Map::len);
                    if base + entries.len() >= 8 {
                        return fail(LIMIT);
                    }
                    (key.to_owned(), config.clone())
                } else {
                    if entries.len() >= 32 {
                        return fail(LIMIT);
                    }
                    let folder = Self::folder(key)?;
                    if let Some(expected) = request["expected_path"].as_str()
                        && expected != folder
                    {
                        return fail(INVALID);
                    }
                    (folder, json!(true))
                };
                if entries.contains_key(&key) {
                    return fail(DUPLICATE);
                }
                entries.insert(key, item);
            }
            Some("enable") => {
                let enabled = request["enabled"]
                    .as_bool()
                    .ok_or_else(|| ToolError(INVALID.into()))?;
                let item = entries
                    .get_mut(key)
                    .ok_or_else(|| ToolError(INVALID.into()))?;
                if group == "servers" {
                    item["enabled"] = json!(enabled);
                } else {
                    *item = json!(enabled);
                }
            }
            Some("remove") => {
                entries
                    .remove(key)
                    .ok_or_else(|| ToolError(INVALID.into()))?;
            }
            _ => return fail(INVALID),
        }
        let raw = next.to_string();
        if raw.len() > 65536 {
            return fail(LIMIT);
        }
        let parent = self
            .file
            .parent()
            .ok_or_else(|| ToolError(UNAVAILABLE.into()))?;
        let mut file =
            tempfile::NamedTempFile::new_in(parent).map_err(|_| ToolError(UNAVAILABLE.into()))?;
        use std::io::Write;
        file.write_all(raw.as_bytes())
            .and_then(|()| file.as_file().sync_all())
            .map_err(|_| ToolError(UNAVAILABLE.into()))?;
        file.persist(&self.file)
            .map_err(|_| ToolError(UNAVAILABLE.into()))?;
        self.managed.replace(next);
        Ok(self.summary())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_changes_persist_and_credentials_stay_out_of_listings() {
        let dir = tempfile::tempdir().expect("private state");
        let config = json!({});
        let extensions = Extensions::new(dir.path(), &config).expect("empty extensions");
        let add = json!({"action":"add","kind":"server","name":"docs","config":{
            "enabled":true,"transport":"http","url":"https://example.com/mcp",
            "headers":{"Authorization":"Bearer fixture"}
        }});
        let summary = extensions.change(&add).expect("server registered");
        assert_eq!(
            summary["servers"][0]["name"], "docs",
            "lists the saved server"
        );
        assert!(
            !summary.to_string().contains("Bearer fixture"),
            "keeps headers private"
        );
        let reloaded = Extensions::new(dir.path(), &config).expect("reload saved form");
        assert_eq!(
            reloaded.servers()["docs"]["headers"]["Authorization"],
            "Bearer fixture"
        );
        reloaded
            .change(&json!({"action":"enable","kind":"server","name":"docs","enabled":false}))
            .expect("disable");
        assert_eq!(reloaded.servers()["docs"]["enabled"], false);
        reloaded
            .change(&json!({"action":"remove","kind":"server","name":"docs"}))
            .expect("remove");
        assert!(
            reloaded.servers()["docs"].is_null(),
            "removed entry no longer reaches discovery"
        );
        let mut invalid = add;
        invalid["config"]["headers"]["Authorization"] = json!("Bearer fixture\r\nHost: injected");
        assert!(
            reloaded.change(&invalid).is_err(),
            "headers cannot modify framing"
        );
    }
}
