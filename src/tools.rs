// SPDX-License-Identifier: MIT
//! A workspace directory capability (no symlink traversal for file tools) and
//! one resource-bounded sandboxed command per call, with no unsandboxed fallback.
use crate::{Result, ToolError, fail, sha256_hex};
use serde_json::{Value, json};
use std::{
    ffi::{CString, OsStr},
    fs::File,
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{ffi::OsStrExt, fs::MetadataExt},
    },
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::io::AsyncReadExt;

pub const MAX_BYTES: usize = 256 * 1024;

#[allow(unsafe_code, reason = "Linux syscall ABI (openat).")]
fn openat(dir: i32, name: impl AsRef<OsStr>, flags: i32, mode: u32) -> std::io::Result<OwnedFd> {
    let name = CString::new(name.as_ref().as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let fd = unsafe { libc::openat(dir, name.as_ptr(), flags | libc::O_CLOEXEC, mode) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn dup(fd: &OwnedFd) -> Result<OwnedFd> {
    fd.try_clone()
        .map_err(|_| ToolError("workspace unavailable".into()))
}

fn read_at(parent: &OwnedFd, name: &str) -> Result<(File, Value)> {
    let unavailable = || ToolError("workspace text file unavailable".into());
    let file = File::from(
        openat(
            parent.as_raw_fd(),
            name,
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK,
            0,
        )
        .map_err(|_| unavailable())?,
    );
    let metadata = file.metadata().map_err(|_| unavailable())?;
    if !metadata.is_file() || metadata.len() > MAX_BYTES as u64 {
        return fail("bounded regular text file required");
    }
    let mut data = Vec::new();
    (&file)
        .take(MAX_BYTES as u64 + 1)
        .read_to_end(&mut data)
        .map_err(|_| unavailable())?;
    if data.len() > MAX_BYTES {
        return fail("file exceeds limit");
    }
    let text = String::from_utf8(data).map_err(|_| unavailable())?;
    Ok((
        file,
        json!({"sha256":sha256_hex(text.as_bytes()), "text":text}),
    ))
}

#[allow(
    unsafe_code,
    reason = "Linux renameat2 exchanges names without discarding either inode."
)]
fn replace_reviewed(
    parent: &OwnedFd,
    temporary: &str,
    name: &str,
    original: &File,
    snapshot: &Value,
    exchanged: &mut bool,
) -> Result<()> {
    let (current, value) = read_at(parent, name)?;
    let a = original
        .metadata()
        .map_err(|_| ToolError("reviewed file unavailable".into()))?;
    let b = current
        .metadata()
        .map_err(|_| ToolError("current file unavailable".into()))?;
    if (a.dev(), a.ino()) != (b.dev(), b.ino()) || value != *snapshot {
        return fail("file changed; review again");
    }
    let from = CString::new(temporary).unwrap();
    let to = CString::new(name).unwrap();
    if unsafe {
        libc::renameat2(
            parent.as_raw_fd(),
            from.as_ptr(),
            parent.as_raw_fd(),
            to.as_ptr(),
            libc::RENAME_EXCHANGE,
        )
    } != 0
    {
        return fail("checked replacement unavailable; original file retained");
    }
    *exchanged = true;
    if unsafe { libc::fsync(parent.as_raw_fd()) } != 0 {
        return fail("replacement published; directory sync failed; backup retained");
    }
    // Keep the exact displaced inode even on success: an editor can still hold
    // it open and write after this readback. Never unlink that recovery copy.
    let displaced = read_at(parent, temporary);
    if displaced.as_ref().map_or(true, |(file, value)| {
        file.metadata()
            .map_or(true, |m| (m.dev(), m.ino()) != (a.dev(), a.ino()))
            || *value != *snapshot
    }) {
        return fail(&format!(
            "concurrent edit preserved at {temporary}; review the target again"
        ));
    }
    Ok(())
}

fn resolved_protected(path: &Path) -> Result<PathBuf> {
    let mut parent = path.to_path_buf();
    let mut suffix = Vec::new();
    loop {
        match std::fs::canonicalize(&parent) {
            Ok(mut resolved) => {
                for name in suffix.iter().rev() {
                    resolved.push(name);
                }
                return Ok(resolved);
            }
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound
                    && !std::fs::symlink_metadata(&parent)
                        .is_ok_and(|m| m.file_type().is_symlink()) =>
            {
                suffix.push(
                    parent
                        .file_name()
                        .ok_or_else(|| ToolError("invalid protected path".into()))?
                        .to_os_string(),
                );
                if !parent.pop() {
                    return fail("invalid protected path");
                }
            }
            Err(_) => return fail("protected path cannot be resolved safely"),
        }
    }
}

pub struct Workspace {
    pub path: PathBuf,
    pub fd: OwnedFd,
    pub exclusions: Vec<Vec<String>>,
    identity: (u64, u64),
}

impl Workspace {
    pub fn new(path: &Path, protected: &[PathBuf]) -> Result<Self> {
        let path =
            std::path::absolute(path).map_err(|_| ToolError("workspace unavailable".into()))?;
        let mut exclusions = Vec::new();
        let mut locations = Vec::new();
        for item in protected {
            let item = std::path::absolute(item)
                .map_err(|_| ToolError("invalid protected path".into()))?;
            locations.push(resolved_protected(&item)?);
            locations.push(item);
        }
        for item in locations {
            if item == path || path.starts_with(&item) {
                return fail("workspace is inside protected service data");
            }
            if let Ok(relative) = item.strip_prefix(&path) {
                exclusions.push(
                    relative
                        .iter()
                        .map(|p| p.to_string_lossy().into_owned())
                        .collect(),
                );
            }
        }
        let fd = openat(
            libc::AT_FDCWD,
            &path,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
            0,
        )
        .map_err(|_| ToolError("workspace unavailable".into()))?;
        let m = File::from(dup(&fd)?)
            .metadata()
            .map_err(|_| ToolError("workspace unavailable".into()))?;
        Ok(Self {
            path,
            fd,
            exclusions,
            identity: (m.dev(), m.ino()),
        })
    }

    fn excluded(&self, parts: &[&str]) -> bool {
        self.exclusions.iter().any(|prefix| {
            parts.len() >= prefix.len()
                && parts[..prefix.len()]
                    .iter()
                    .zip(prefix)
                    .all(|(a, b)| a == b)
        })
    }

    /// The parent directory descriptor and final name of a workspace-relative path.
    fn parent(&self, relative: &str) -> Result<(OwnedFd, String)> {
        if relative.len() > 4096 || relative.contains('\0') {
            return fail("invalid workspace path");
        }
        let pieces = relative.split('/').collect::<Vec<_>>();
        if self.excluded(&pieces) {
            return fail("protected service data is outside tool authority");
        }
        if relative.starts_with('/') || pieces.iter().any(|p| ["", ".", ".."].contains(p)) {
            return fail("workspace-relative path required");
        }
        let mut current = dup(&self.fd)?;
        for part in &pieces[..pieces.len() - 1] {
            current = openat(
                current.as_raw_fd(),
                part,
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
                0,
            )
            .map_err(|_| ToolError("workspace path unavailable".into()))?;
        }
        Ok((current, pieces[pieces.len() - 1].to_owned()))
    }

    pub(crate) fn contains_write_path(&self, path: &str) -> bool {
        self.parent(path).is_ok()
    }

    pub fn read(&self, relative: &str) -> Result<Value> {
        let (parent, name) = self.parent(relative)?;
        read_at(&parent, &name).map(|(_, value)| value)
    }

    /// Checked replacement coordinates harness writers and retains the displaced
    /// inode. Noncooperating writers are not covered by an atomic content CAS;
    /// their displaced edits remain available in the returned backup (with its original permissions).
    #[allow(
        unsafe_code,
        reason = "Linux descriptor-relative publication and flock."
    )]
    pub fn write(&self, relative: &str, text: &str, expected: Option<&str>) -> Result<Value> {
        if text.len() > MAX_BYTES {
            return fail("file content exceeds limit");
        }
        let (parent, name) = self.parent(relative)?;
        let lock = File::from(
            openat(
                parent.as_raw_fd(),
                ".",
                libc::O_RDONLY | libc::O_DIRECTORY,
                0,
            )
            .map_err(|_| ToolError("workspace write unavailable".into()))?,
        );
        if lock.try_lock().is_err() {
            return fail("workspace directory has another writer; retry after review");
        }
        let previous = match read_at(&parent, &name) {
            Ok(value) => Some(value),
            Err(_) => {
                let c_name = CString::new(name.as_str()).unwrap();
                let mut st: libc::stat = unsafe { std::mem::zeroed() };
                let found = unsafe {
                    libc::fstatat(
                        parent.as_raw_fd(),
                        c_name.as_ptr(),
                        &mut st,
                        libc::AT_SYMLINK_NOFOLLOW,
                    )
                };
                if found == 0
                    || std::io::Error::last_os_error().raw_os_error() != Some(libc::ENOENT)
                {
                    return fail("existing file cannot be safely read");
                }
                None
            }
        };
        if previous.as_ref().and_then(|(_, p)| p["sha256"].as_str()) != expected {
            return fail("file changed; review again");
        }
        let temporary = format!(
            ".agent-backup-{}",
            enzimos_platform::files::random_hex(16).map_err(|_| ToolError("random".into()))?
        );
        let c_temp = CString::new(temporary.as_str()).unwrap();
        let failed = || ToolError("workspace write failed".into());
        let mut exchanged = false;
        let result = (|| -> Result<()> {
            let mut file = File::from(
                openat(
                    parent.as_raw_fd(),
                    &temporary,
                    libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW,
                    0o600,
                )
                .map_err(|_| failed())?,
            );
            file.write_all(text.as_bytes()).map_err(|_| failed())?;
            let mode = match &previous {
                Some((original, _)) => original.metadata().map_err(|_| failed())?.mode() & 0o777,
                None => 0o644,
            };
            if unsafe { libc::fchmod(file.as_raw_fd(), mode) } != 0 {
                return Err(failed());
            }
            file.sync_all().map_err(|_| failed())?;
            if let Some((original, snapshot)) = &previous {
                replace_reviewed(
                    &parent,
                    &temporary,
                    &name,
                    original,
                    snapshot,
                    &mut exchanged,
                )?;
            } else {
                let c_name = CString::new(name.as_str()).unwrap();
                if unsafe {
                    libc::linkat(
                        parent.as_raw_fd(),
                        c_temp.as_ptr(),
                        parent.as_raw_fd(),
                        c_name.as_ptr(),
                        0,
                    )
                } != 0
                {
                    return Err(failed());
                }
            }
            if unsafe { libc::fsync(parent.as_raw_fd()) } != 0 {
                return Err(failed());
            }
            Ok(())
        })();
        if !exchanged {
            unsafe { libc::unlinkat(parent.as_raw_fd(), c_temp.as_ptr(), 0) };
        }
        result?;
        let backup = exchanged.then(|| {
            Path::new(relative)
                .with_file_name(&temporary)
                .to_string_lossy()
                .into_owned()
        });
        Ok(json!({"sha256": sha256_hex(text.as_bytes()), "bytes": text.len(), "backup":backup}))
    }

    #[allow(
        unsafe_code,
        reason = "Linux descriptor-relative rename without overwrite."
    )]
    pub(crate) fn rename(&self, source: &str, new_name: &str) -> Result<Value> {
        if new_name.is_empty() || ["..", "."].contains(&new_name) || new_name.contains('/') {
            return fail("rename requires a new name without a folder path");
        }
        let (parent, old_name) = self.parent(source)?;
        let destination = Path::new(source).with_file_name(new_name);
        for relative in [Path::new(source), destination.as_path()] {
            let parts = relative
                .to_str()
                .unwrap_or_default()
                .split('/')
                .collect::<Vec<_>>();
            // Renaming a protected entry's ancestor would move it out of its exclusion.
            if self.excluded(&parts)
                || self.exclusions.iter().any(|prefix| {
                    prefix.len() >= parts.len()
                        && prefix.iter().zip(&parts).all(|(left, right)| left == right)
                })
            {
                return fail("protected service data is outside tool authority");
            }
        }
        let from =
            CString::new(old_name).map_err(|_| ToolError("invalid workspace path".into()))?;
        let to = CString::new(new_name).map_err(|_| ToolError("invalid workspace path".into()))?;
        // SAFETY: The owned descriptor and both C strings outlive the syscall.
        if unsafe {
            libc::renameat2(
                parent.as_raw_fd(),
                from.as_ptr(),
                parent.as_raw_fd(),
                to.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        } == 0
        {
            return Ok(json!({"path": destination, "renamed_from": source}));
        }
        match std::io::Error::last_os_error().kind() {
            std::io::ErrorKind::AlreadyExists => fail("rename destination already exists"),
            std::io::ErrorKind::NotFound => fail("workspace path unavailable"),
            _ => fail("could not rename the workspace entry"),
        }
    }

    pub fn list(&self, relative: Option<&str>) -> Result<Value> {
        let fd = match relative {
            None => dup(&self.fd)?,
            Some(relative) => {
                let (parent, name) = self.parent(relative)?;
                openat(
                    parent.as_raw_fd(),
                    &name,
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
                    0,
                )
                .map_err(|_| ToolError("workspace folder unavailable".into()))?
            }
        };
        let base = relative
            .map(|r| r.split('/').collect::<Vec<_>>())
            .unwrap_or_default();
        let mut entries = Vec::new();
        let listing = std::fs::read_dir(format!("/proc/self/fd/{}", fd.as_raw_fd()))
            .map_err(|_| ToolError("workspace folder unavailable".into()))?;
        for entry in listing.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let mut parts = base.clone();
            parts.push(&name);
            if self.excluded(&parts) {
                continue;
            }
            if entries.len() >= 1000 {
                return fail("folder exceeds entry limit");
            }
            let kind = match entry.file_type() {
                Ok(t) if t.is_symlink() => "symlink",
                Ok(t) if t.is_dir() => "directory",
                _ => "file",
            };
            entries.push((name, kind));
        }
        entries.sort();
        Ok(json!(
            entries
                .into_iter()
                .map(|(name, kind)| json!({"name": name, "kind": kind}))
                .collect::<Vec<_>>()
        ))
    }

    /// The same directory capability through a duplicated descriptor.
    pub fn share(&self) -> Result<Self> {
        Ok(Self {
            path: self.path.clone(),
            fd: dup(&self.fd)?,
            exclusions: self.exclusions.clone(),
            identity: self.identity,
        })
    }

    pub fn unchanged(&self) -> Result<()> {
        let m = std::fs::symlink_metadata(&self.path)
            .map_err(|_| ToolError("workspace identity changed".into()))?;
        if (m.dev(), m.ino()) != self.identity {
            return fail("workspace identity changed");
        }
        Ok(())
    }
}

pub(crate) fn sweep_scratch(workspace: &Workspace) {
    fn visit(fd: &impl AsRawFd, device: u64, depth: usize) {
        let root = PathBuf::from(format!("/proc/self/fd/{}", fd.as_raw_fd()));
        let Ok(entries) = std::fs::read_dir(root) else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(metadata) = std::fs::symlink_metadata(entry.path()) else {
                continue;
            };
            if metadata.dev() != device {
                continue;
            }
            let stale = metadata
                .modified()
                .ok()
                .and_then(|time| time.elapsed().ok())
                .is_some_and(|age| age >= Duration::from_secs(24 * 60 * 60));
            if metadata.is_dir() {
                // Descend through an open directory descriptor, even if its name is replaced.
                if depth < 32
                    && let Ok(child) = openat(
                        fd.as_raw_fd(),
                        entry.file_name(),
                        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
                        0,
                    )
                {
                    let child = File::from(child);
                    if child.metadata().is_ok_and(|m| m.dev() == device) {
                        visit(&child, device, depth + 1);
                    }
                }
                if stale {
                    let _ = std::fs::remove_dir(entry.path());
                }
            } else if stale {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    if workspace.unchanged().is_ok() {
        visit(&workspace.fd, workspace.identity.0, 0);
    }
}

async fn drain<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
    total: &std::cell::Cell<usize>,
) -> Result<String> {
    let mut data = Vec::new();
    let mut chunk = [0u8; 16384];
    loop {
        let n = reader
            .read(&mut chunk)
            .await
            .map_err(|_| ToolError("command output unavailable".into()))?;
        if n == 0 {
            return Ok(String::from_utf8_lossy(&data).into_owned());
        }
        total.set(total.get() + n);
        if total.get() > MAX_BYTES {
            return fail("command output exceeds limit");
        }
        data.extend_from_slice(&chunk[..n]);
    }
}

/// Bubblewrap in a systemd user scope with aggregate memory, task and CPU limits.
#[allow(unsafe_code, reason = "Fork-child setup must run before exec.")]
pub async fn command(
    workspace: &Workspace,
    argv: &[String],
    writable: bool,
    network: bool,
    process_test: bool,
) -> Result<Value> {
    let timeout = Duration::from_secs(30);
    if argv.is_empty()
        || argv.len() > 128
        || argv.iter().any(|a| a.len() > 4096 || a.contains('\0'))
    {
        return fail("invalid command arguments");
    }
    workspace.unchanged()?;
    let fd = workspace.fd.as_raw_fd();
    let mut args = [
        "/usr/bin/bwrap",
        "--unshare-all",
        "--die-with-parent",
        "--new-session",
        "--clearenv",
        "--ro-bind",
        "/usr",
        "/usr",
        "--symlink",
        "usr/bin",
        "/bin",
        "--symlink",
        "usr/lib",
        "/lib",
        "--symlink",
        "usr/lib",
        "/lib64",
        "--proc",
        "/proc",
        "--dev",
        "/dev",
        "--tmpfs",
        "/tmp",
        "--dir",
        "/etc",
    ]
    .map(String::from)
    .to_vec();
    args.extend([
        if writable { "--bind" } else { "--ro-bind" }.to_owned(),
        format!("/proc/self/fd/{fd}"),
        "/workspace".into(),
    ]);
    args.extend(
        [
            "--chdir",
            "/workspace",
            "--setenv",
            "HOME",
            "/tmp",
            "--setenv",
            "PATH",
            "/usr/bin",
            "--setenv",
            "LANG",
            "C.UTF-8",
        ]
        .map(String::from),
    );
    // Public trust bundles only; never key stores.
    for bundle in [
        "/etc/ssl/cert.pem",
        "/etc/ssl/certs/ca-certificates.crt",
        "/etc/pki/tls/certs/ca-bundle.crt",
    ] {
        if Path::new(bundle).is_file() {
            args.extend(["--ro-bind".into(), bundle.into(), bundle.into()]);
        }
    }
    for prefix in &workspace.exclusions {
        let source = prefix
            .iter()
            .fold(workspace.path.clone(), |p, part| p.join(part));
        let Ok(m) = std::fs::symlink_metadata(&source) else {
            continue;
        };
        if m.file_type().is_symlink() {
            continue;
        }
        let target = format!("/workspace/{}", prefix.join("/"));
        args.extend(if m.is_dir() {
            vec!["--tmpfs".to_owned(), target]
        } else {
            vec!["--ro-bind".into(), "/dev/null".into(), target]
        });
    }
    if network {
        args.extend(
            [
                "--share-net",
                "--ro-bind",
                "/etc/resolv.conf",
                "/etc/resolv.conf",
            ]
            .map(String::from),
        );
    }
    args.push("--".into());
    args.extend(argv.iter().cloned());
    if !process_test {
        // Aggregate limits cover descendants, not only each process.
        let mut scope = [
            "/usr/bin/systemd-run",
            "--user",
            "--scope",
            "--quiet",
            "--collect",
            "--property=MemoryMax=2G",
            "--property=MemorySwapMax=64M",
            "--property=TasksMax=64",
            "--property=CPUQuota=100%",
            "--",
        ]
        .map(String::from)
        .to_vec();
        scope.extend(args);
        args = scope;
    }
    let mut child = tokio::process::Command::new(&args[0]);
    child
        .args(&args[1..])
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for key in ["PATH", "XDG_RUNTIME_DIR", "DBUS_SESSION_BUS_ADDRESS"] {
        if let Ok(v) = std::env::var(key) {
            child.env(key, v);
        }
    }
    let seconds = timeout.as_secs() as libc::rlim_t;
    unsafe {
        child.pre_exec(move || {
            libc::setsid();
            libc::setpriority(libc::PRIO_PROCESS, 0, 19);
            let mut set: libc::cpu_set_t = std::mem::zeroed();
            libc::sched_getaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &mut set);
            let allowed = (0..libc::CPU_SETSIZE as usize)
                .filter(|c| libc::CPU_ISSET(*c, &set))
                .collect::<Vec<_>>();
            let mut chosen: libc::cpu_set_t = std::mem::zeroed();
            for cpu in allowed.iter().rev().take(2) {
                libc::CPU_SET(*cpu, &mut chosen);
            }
            libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &chosen);
            let limit = |resource, value: libc::rlim_t| {
                libc::setrlimit(
                    resource,
                    &libc::rlimit {
                        rlim_cur: value,
                        rlim_max: value,
                    },
                )
            };
            libc::setrlimit(
                libc::RLIMIT_CPU,
                &libc::rlimit {
                    rlim_cur: seconds + 1,
                    rlim_max: seconds + 2,
                },
            );
            limit(libc::RLIMIT_AS, 2 << 30);
            limit(libc::RLIMIT_FSIZE, 32 << 20);
            limit(libc::RLIMIT_NOFILE, 128);
            limit(libc::RLIMIT_CORE, 0);
            // The workspace descriptor is inherited for bwrap's /proc/self/fd bind.
            let flags = libc::fcntl(fd, libc::F_GETFD);
            libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
            Ok(())
        });
    }
    let mut child = child
        .spawn()
        .map_err(|_| ToolError("command sandbox unavailable".into()))?;
    let group = child.id().map(|p| p as i32);
    let (mut out, mut err) = (child.stdout.take().unwrap(), child.stderr.take().unwrap());
    let total = std::cell::Cell::new(0usize);
    let result = tokio::time::timeout(timeout, async {
        // The first reader over the output bound ends the command at once.
        let (stdout, stderr) = tokio::try_join!(drain(&mut out, &total), drain(&mut err, &total))?;
        let status = child
            .wait()
            .await
            .map_err(|_| ToolError("command unavailable".into()))?;
        Ok::<_, ToolError>(
            json!({"returncode": status.code().unwrap_or(-1), "stdout": stdout, "stderr": stderr}),
        )
    })
    .await;
    if let Some(group) = group {
        unsafe { libc::kill(-group, libc::SIGKILL) };
    }
    let _ = tokio::time::timeout(Duration::from_secs(3), child.wait()).await;
    result.map_err(|_| ToolError("command deadline exceeded".into()))?
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn workspace_dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn scratch_expiry_does_not_follow_symlinks() {
        use std::{fs, fs::FileTimes, time::SystemTime};
        let parent = tempfile::tempdir().expect("fixture");
        let scratch = parent.path().join("scratch");
        fs::create_dir(&scratch).expect("scratch directory");
        let outside = parent.path().join("outside");
        fs::create_dir(&outside).expect("outside directory");
        for file in [scratch.join("old.txt"), outside.join("old.txt")] {
            let f = File::create(file).expect("draft");
            let old = SystemTime::now() - Duration::from_secs(25 * 60 * 60);
            f.set_times(FileTimes::new().set_modified(old))
                .expect("age fixture");
        }
        fs::write(scratch.join("recent.txt"), "draft").expect("recent draft");
        std::os::unix::fs::symlink(&outside, scratch.join("link")).expect("escape link");
        let workspace = Workspace::new(&scratch, &[]).expect("scratch workspace");
        sweep_scratch(&workspace);
        assert!(!scratch.join("old.txt").exists(), "stale draft expired");
        assert!(
            scratch.join("recent.txt").is_file(),
            "recent draft retained"
        );
        assert!(outside.join("old.txt").is_file(), "symlink target retained");
    }

    #[test]
    fn rename_keeps_existing_entries_and_protected_subtrees() {
        let dir = workspace_dir();
        let root = dir.path();
        let private = root.join("settings/private");
        std::fs::create_dir_all(&private).unwrap();
        std::fs::write(root.join("original"), b"photo").unwrap();
        std::fs::write(root.join("occupied"), b"keep").unwrap();
        let ws = Workspace::new(root, &[private]).unwrap();

        assert!(ws.rename("original", "occupied").is_err());
        assert!(ws.rename("original", "../outside").is_err());
        assert!(ws.rename("settings", "renamed-settings").is_err());
        assert_eq!(std::fs::read(root.join("occupied")).unwrap(), b"keep");
        assert_eq!(ws.rename("original", "beach").unwrap()["path"], "beach");
        assert_eq!(std::fs::read(root.join("beach")).unwrap(), b"photo");
        assert!(!root.join("original").exists());
    }

    #[test]
    fn reads_writes_are_digest_bound_and_atomic() {
        let dir = workspace_dir();
        let ws = Workspace::new(dir.path(), &[]).unwrap();
        let created = ws.write("notes.txt", "one", None).unwrap();
        assert_eq!(ws.read("notes.txt").unwrap()["text"], "one");
        assert_eq!(
            ws.write("notes.txt", "two", None).unwrap_err().0,
            "file changed; review again"
        );
        assert!(
            ws.write("notes.txt", "two", created["sha256"].as_str())
                .is_ok()
        );
        assert_eq!(ws.read("notes.txt").unwrap()["text"], "two");
        let replaced = std::fs::read_dir(dir.path())
            .unwrap()
            .find_map(|e| {
                let e = e.unwrap();
                e.file_name()
                    .to_string_lossy()
                    .starts_with(".agent-backup-")
                    .then(|| e.path())
            })
            .unwrap();
        assert_eq!(std::fs::read_to_string(&replaced).unwrap(), "one");
        let original = read_at(&ws.fd, "notes.txt").unwrap();
        std::fs::write(dir.path().join("notes.txt"), "independent edit").unwrap();
        std::fs::write(dir.path().join("prepared"), "agent output").unwrap();
        let mut exchanged = false;
        assert!(
            replace_reviewed(
                &ws.fd,
                "prepared",
                "notes.txt",
                &original.0,
                &original.1,
                &mut exchanged
            )
            .is_err()
        );
        assert!(!exchanged);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("notes.txt")).unwrap(),
            "independent edit"
        );
        std::fs::remove_file(dir.path().join("prepared")).unwrap();
        let mut held = std::fs::OpenOptions::new()
            .append(true)
            .open(dir.path().join("notes.txt"))
            .unwrap();
        let expected = ws.read("notes.txt").unwrap();
        let result = ws
            .write("notes.txt", "agent final", expected["sha256"].as_str())
            .unwrap();
        held.write_all(b" after replacement").unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join(result["backup"].as_str().unwrap())).unwrap(),
            "independent edit after replacement"
        );
        assert_eq!(ws.read("notes.txt").unwrap()["text"], "agent final");
        let lock = File::open(dir.path()).unwrap();
        lock.lock().unwrap();
        assert!(ws.write("busy.txt", "refused", None).is_err());
        assert!(!dir.path().join("busy.txt").exists());

        assert!(!std::fs::read_dir(dir.path()).unwrap().any(|e| {
            e.unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".agent-write")
        }));
    }

    #[test]
    fn symlinks_parents_and_protected_paths_are_refused() {
        let dir = workspace_dir();
        std::fs::create_dir(dir.path().join("secret")).unwrap();
        std::fs::write(dir.path().join("secret/key"), "x").unwrap();
        std::fs::write(dir.path().join("real.txt"), "x").unwrap();
        std::os::unix::fs::symlink(dir.path().join("real.txt"), dir.path().join("link.txt"))
            .unwrap();
        let ws = Workspace::new(dir.path(), &[dir.path().join("secret")]).unwrap();
        assert!(ws.read("link.txt").is_err());
        assert!(ws.read("../etc/passwd").is_err());
        assert!(ws.read("/etc/passwd").is_err());
        assert_eq!(
            ws.read("secret/key").unwrap_err().0,
            "protected service data is outside tool authority"
        );
        let names = ws.list(None).unwrap();
        assert!(
            !names.to_string().contains("secret")
                && names.to_string().contains("\"kind\":\"symlink\"")
        );
        assert!(Workspace::new(&dir.path().join("secret"), &[dir.path().to_path_buf()]).is_err());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn commands_run_in_a_read_only_namespace_without_protected_data() {
        if !Path::new("/usr/bin/bwrap").exists() {
            return;
        }
        let dir = workspace_dir();
        std::fs::write(dir.path().join("data.txt"), "hello").unwrap();
        std::fs::create_dir(dir.path().join("private")).unwrap();
        std::fs::write(dir.path().join("private/token"), "secret").unwrap();
        let ws = Workspace::new(dir.path(), &[dir.path().join("private")]).unwrap();
        let argv = |s: &[&str]| {
            s.iter()
                .map(std::string::ToString::to_string)
                .collect::<Vec<_>>()
        };
        let out = command(
            &ws,
            &argv(&["/usr/bin/cat", "data.txt"]),
            false,
            false,
            true,
        )
        .await
        .unwrap();
        assert_eq!(
            (out["returncode"].as_i64(), out["stdout"].as_str()),
            (Some(0), Some("hello"))
        );
        let hidden = command(&ws, &argv(&["/usr/bin/ls", "private"]), false, false, true)
            .await
            .unwrap();
        assert_eq!(hidden["stdout"], "");
        let readonly = command(&ws, &argv(&["/usr/bin/touch", "new"]), false, false, true)
            .await
            .unwrap();
        assert_ne!(readonly["returncode"], 0);
        assert!(!dir.path().join("new").exists());
        let writable = command(&ws, &argv(&["/usr/bin/touch", "new"]), true, false, true)
            .await
            .unwrap();
        assert_eq!(writable["returncode"], 0);
    }
}
