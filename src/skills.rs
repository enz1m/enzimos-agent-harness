// SPDX-License-Identifier: MIT
//! Instruction discovery and import; skills never expand tool authority and
//! discovery never runs their scripts.
use crate::{Result, ToolError, fail, sha256_hex, uid};
use serde_json::{Value, json};
use std::{
    ffi::CString,
    io::{Read, Write},
    os::{
        fd::FromRawFd,
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
};

pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.split('-').all(|p| {
            !p.is_empty()
                && p.bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        })
}

#[derive(Debug)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub sha256: String,
    pub instructions: String,
}

pub fn read_skill(path: &Path) -> Result<Skill> {
    let unavailable = || ToolError("skill unavailable".into());
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|_| unavailable())?;
    let m = file.metadata().map_err(|_| unavailable())?;
    if !m.is_file() || m.len() > 65536 {
        return fail("bounded regular skill file required");
    }
    let mut raw = Vec::new();
    (&mut file)
        .take(65537)
        .read_to_end(&mut raw)
        .map_err(|_| unavailable())?;
    if raw.len() > 65536 {
        return fail("skill exceeds limit");
    }
    let text = std::str::from_utf8(&raw).map_err(|_| unavailable())?;
    let rest = text
        .strip_prefix("---\n")
        .ok_or_else(|| ToolError("skill frontmatter required".into()))?;
    let (header, body) = rest
        .split_once("\n---\n")
        .ok_or_else(|| ToolError("skill frontmatter required".into()))?;
    let mut fields = std::collections::HashMap::new();
    for line in header.lines() {
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with(char::is_whitespace) {
            // Only single-line metadata; never reinterpret multiline fields.
            return fail("use single-line skill metadata");
        }
        let (key, value) = line
            .split_once(':')
            .ok_or_else(|| ToolError("invalid skill metadata".into()))?;
        let value = value.trim();
        let value = if value.starts_with('"') {
            serde_json::from_str::<String>(value)
                .map_err(|_| ToolError("invalid skill metadata quoting".into()))?
        } else if value.len() >= 2 && value.starts_with('\'') && value.ends_with('\'') {
            value[1..value.len() - 1].replace("''", "'")
        } else {
            value.to_owned()
        };
        if fields.insert(key.to_owned(), value).is_some() {
            return fail("invalid skill metadata");
        }
    }
    let name = fields.get("name").cloned().unwrap_or_default();
    if !valid_name(&name) {
        return fail("invalid skill name");
    }
    if path
        .parent()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        != Some(name.clone())
    {
        return fail("skill name must match folder");
    }
    let description = fields.get("description").cloned().unwrap_or_default();
    if !(1..=1024).contains(&description.chars().count()) || body.trim().is_empty() {
        return fail("skill description and instructions required");
    }
    Ok(Skill {
        name,
        description,
        sha256: sha256_hex(&raw),
        instructions: body.to_owned(),
    })
}

pub struct Skills {
    pub directories: Vec<PathBuf>,
}

impl Skills {
    pub fn new(directories: &Value) -> Result<Self> {
        let directories = directories
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|d| {
                d.as_str()
                    .and_then(|d| std::fs::canonicalize(d).ok())
                    .ok_or_else(|| ToolError("skill directory unavailable".into()))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self { directories })
    }

    pub fn discover(&self) -> Result<Value> {
        let mut rows = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for directory in &self.directories {
            let mut children = std::fs::read_dir(directory)
                .map_err(|_| ToolError("skill directory unavailable".into()))?
                .flatten()
                .map(|e| e.path())
                .collect::<Vec<_>>();
            children.sort();
            for child in children {
                let Ok(m) = std::fs::symlink_metadata(&child) else {
                    continue;
                };
                if !m.is_dir() || !child.join("SKILL.md").exists() {
                    continue;
                }
                let skill = read_skill(&child.join("SKILL.md"))?;
                if !seen.insert(skill.name.clone()) {
                    return fail("duplicate skill identity");
                }
                rows.push(json!({"name": skill.name, "description": skill.description, "sha256": skill.sha256}));
                if rows.len() > 128 || json!(rows).to_string().len() > 8192 {
                    return fail("skill discovery context exceeds limit");
                }
            }
        }
        Ok(json!(rows))
    }

    /// Client-visible reason: the selected skill's content changed or it is gone;
    /// the client should list skills again.
    pub const CHANGED: &str = "skill-changed";

    pub fn load(&self, name: &str, expected: &str) -> Result<Skill> {
        if !valid_name(name) {
            return fail("invalid skill identity");
        }
        let mut matches = Vec::new();
        for directory in &self.directories {
            let child = directory.join(name);
            if std::fs::symlink_metadata(&child).is_ok_and(|m| m.file_type().is_symlink()) {
                return fail("skill folder symlink refused");
            }
            if child.join("SKILL.md").exists() {
                matches.push(read_skill(&child.join("SKILL.md"))?);
            }
        }
        match matches.pop() {
            Some(skill) if matches.is_empty() && skill.sha256 == expected => Ok(skill),
            _ => fail(Self::CHANGED),
        }
    }

    /// Explicit user control-plane import of instruction text; never a model tool.
    #[allow(unsafe_code, reason = "Linux syscall ABI (mkdirat, openat).")]
    pub fn create(
        &self,
        directory: &str,
        name: &str,
        description: &str,
        instructions: &str,
    ) -> Result<Value> {
        if !valid_name(name) {
            return fail("invalid skill identity");
        }
        if !(1..=1024).contains(&description.chars().count()) {
            return fail("invalid skill description");
        }
        if instructions.trim().is_empty() {
            return fail("skill instructions required");
        }
        let root = std::path::absolute(directory)
            .map_err(|_| ToolError("skill destination not registered".into()))?;
        if !self.directories.contains(&root) {
            return fail("skill destination not registered");
        }
        let description_json = serde_json::to_string(description).unwrap_or_default();
        let raw =
            format!("---\nname: {name}\ndescription: {description_json}\n---\n{instructions}")
                .into_bytes();
        if raw.len() > 65536 {
            return fail("skill exceeds limit");
        }
        let refuse = || ToolError("skill creation unavailable or identity already exists".into());
        let root_dir = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(&root)
            .map_err(|_| refuse())?;
        let m = root_dir.metadata().map_err(|_| refuse())?;
        if m.uid() != uid() || m.mode() & 0o022 != 0 {
            return fail("owned non-writable-by-others skill destination required");
        }
        use std::os::fd::AsRawFd;
        let c_name = CString::new(name).unwrap();
        if unsafe { libc::mkdirat(root_dir.as_raw_fd(), c_name.as_ptr(), 0o700) } != 0 {
            return Err(refuse());
        }
        let written = (|| -> Option<()> {
            let child = unsafe {
                libc::openat(
                    root_dir.as_raw_fd(),
                    c_name.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if child < 0 {
                return None;
            }
            let child = unsafe { std::fs::File::from_raw_fd(child) };
            let skill = CString::new("SKILL.md").unwrap();
            let fd = unsafe {
                libc::openat(
                    child.as_raw_fd(),
                    skill.as_ptr(),
                    libc::O_WRONLY
                        | libc::O_CREAT
                        | libc::O_EXCL
                        | libc::O_NOFOLLOW
                        | libc::O_CLOEXEC,
                    0o600,
                )
            };
            if fd < 0 {
                return None;
            }
            let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
            file.write_all(&raw).ok()?;
            file.sync_all().ok()?;
            child.sync_all().ok()?;
            root_dir.sync_all().ok()
        })();
        if written.is_none() {
            let _ = std::fs::remove_dir_all(root.join(name));
            return Err(refuse());
        }
        Ok(json!({"name": name, "description": description, "sha256": sha256_hex(&raw)}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_discover_and_digest_bound_load() {
        let dir = tempfile::tempdir().unwrap();
        let skills = Skills::new(&json!([dir.path()])).unwrap();
        let created = skills
            .create(
                dir.path().to_str().unwrap(),
                "release-notes",
                "Write \"notes\"",
                "Summarize changes.",
            )
            .unwrap();
        let listed = skills.discover().unwrap();
        assert_eq!(listed[0]["sha256"], created["sha256"]);
        assert_eq!(listed[0]["description"], "Write \"notes\"");
        let sha = created["sha256"].as_str().unwrap();
        assert_eq!(
            skills.load("release-notes", sha).unwrap().instructions,
            "Summarize changes."
        );
        assert!(skills.load("release-notes", &"0".repeat(64)).is_err());
        assert!(
            skills
                .create(dir.path().to_str().unwrap(), "release-notes", "again", "x")
                .is_err()
        );
        assert!(skills.create("/elsewhere", "other", "d", "x").is_err());
        assert!(
            skills
                .create(dir.path().to_str().unwrap(), "Bad_Name", "d", "x")
                .is_err()
        );
    }

    #[test]
    fn malformed_and_mismatched_skills_refused() {
        let dir = tempfile::tempdir().unwrap();
        let write = |folder: &str, text: &str| {
            std::fs::create_dir_all(dir.path().join(folder)).unwrap();
            std::fs::write(dir.path().join(folder).join("SKILL.md"), text).unwrap();
            read_skill(&dir.path().join(folder).join("SKILL.md"))
        };
        assert!(
            write(
                "ok",
                "---\nname: ok\ndescription: 'it''s fine'\n---\nDo it."
            )
            .is_ok()
        );
        assert_eq!(
            write("other", "---\nname: ok\ndescription: d\n---\nx")
                .unwrap_err()
                .0,
            "skill name must match folder"
        );
        assert!(
            write(
                "multi",
                "---\nname: multi\ndescription: >\n  folded\n---\nx"
            )
            .is_err()
        );
        assert!(write("nofront", "name: nofront\n").is_err());
        assert!(write("empty", "---\nname: empty\ndescription: d\n---\n   ").is_err());
    }
}
