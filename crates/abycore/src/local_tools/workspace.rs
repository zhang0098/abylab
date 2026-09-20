use super::{LocalToolConfig, PermissionMode};
use crate::{CancellationToken, ToolContext, ToolError, ToolOutput};
use cap_std::fs::{Dir, DirBuilder, File, Metadata, OpenOptions};
use std::{
    collections::HashMap,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex, Weak},
};
use tokio::time::Instant;

pub(super) type ToolResult<T> = Result<T, ToolError>;

/// Ephemeral observations, owned by an Agent rather than a shared tool bundle.
#[derive(Default)]
pub(crate) struct LocalSession {
    observed: Mutex<HashMap<PathBuf, Option<FileVersion>>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct FileVersion {
    len: u64,
    modified: Option<std::time::SystemTime>,
    #[cfg(unix)]
    identity: (u64, u64, i64, i64),
}

pub(super) fn version(metadata: &Metadata) -> FileVersion {
    FileVersion {
        len: metadata.len(),
        modified: metadata.modified().ok().map(|time| time.into_std()),
        #[cfg(unix)]
        identity: {
            use cap_std::fs::MetadataExt;
            (
                metadata.dev(),
                metadata.ino(),
                metadata.ctime(),
                metadata.ctime_nsec(),
            )
        },
    }
}

impl LocalSession {
    pub(super) fn observe(&self, path: PathBuf, value: Option<FileVersion>) {
        self.observed
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(path, value);
    }
    pub(super) fn guard(
        &self,
        path: &Path,
        current: Option<&Metadata>,
        edit: bool,
    ) -> ToolResult<()> {
        let observations = self.observed.lock().unwrap_or_else(|p| p.into_inner());
        match observations.get(path) {
            Some(Some(expected)) => {
                if current.map(version).as_ref() != Some(expected) {
                    return Err(failed(
                        "FS_STALE_VERSION: file changed since it was read; read it again before modifying it",
                    ));
                }
            }
            _ if edit => return Err(failed("FS_NOT_OBSERVED: read the file before editing it")),
            _ if current.is_some() => {
                return Err(failed(
                    "FS_NOT_OBSERVED: read the existing file before overwriting it",
                ));
            }
            _ => {}
        }
        Ok(())
    }
}

pub(super) struct Workspace {
    pub config: LocalToolConfig,
    pub capability_root: PathBuf,
    pub workspace_path: PathBuf,
    pub writable_roots: Vec<PathBuf>,
    pub directory: Dir,
    pub mutations: Mutex<HashMap<PathBuf, Weak<Mutex<()>>>>,
}

impl Workspace {
    pub fn path(&self, input: &str) -> ToolResult<PathBuf> {
        if input.trim().is_empty() || input.contains('\0') || input.contains("://") {
            return Err(failed("file_path must be a nonempty local path"));
        }
        let path = Path::new(input);
        let absolute = if path.is_absolute() {
            path.to_owned()
        } else {
            self.config.root.join(path)
        };
        let relative = absolute
            .strip_prefix(&self.capability_root)
            .map_err(|_| failed("path is outside the local filesystem capability root"))?;
        if relative.components().any(|c| {
            !matches!(
                c,
                Component::Normal(_) | Component::CurDir | Component::ParentDir
            )
        }) {
            return Err(failed(
                "path is outside the local filesystem capability root",
            ));
        }
        Ok(if relative.as_os_str().is_empty() {
            ".".into()
        } else {
            relative.into()
        })
    }

    pub fn absolute(&self, path: &Path) -> PathBuf {
        self.capability_root.join(path)
    }

    pub fn workspace_child(&self, path: impl AsRef<Path>) -> PathBuf {
        self.workspace_path.join(path)
    }

    pub fn enforce_mutation(&self, path: &Path) -> ToolResult<()> {
        let target = self.absolute(path);
        let denied = match self.config.permission_mode {
            PermissionMode::ReadOnly => true,
            PermissionMode::WorkspaceWrite => {
                !target.starts_with(&self.config.root)
                    && !self
                        .writable_roots
                        .iter()
                        .any(|root| target.starts_with(root))
            }
            PermissionMode::FullAccess => false,
        };
        if denied {
            return Err(failed(format!(
                "FS_SANDBOX_DENIED: cannot write \"{}\": file access denied under {} mode",
                target.display(),
                self.config.permission_mode
            )));
        }
        Ok(())
    }

    /// Resolve aliases within the capability, including missing suffixes for create.
    pub fn resolve(&self, input: &str) -> ToolResult<PathBuf> {
        let mut path = self.expand_links(self.path(input)?)?;
        let mut missing = Vec::new();
        loop {
            match self.directory.canonicalize(&path) {
                Ok(mut resolved) => {
                    for part in missing.iter().rev() {
                        resolved.push(part);
                    }
                    return Ok(resolved);
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    if self
                        .directory
                        .symlink_metadata(&path)
                        .is_ok_and(|m| m.is_symlink())
                    {
                        return Err(failed("cannot resolve dangling symbolic link"));
                    }
                    let name = path
                        .file_name()
                        .ok_or_else(|| io_error("resolve path", error))?
                        .to_owned();
                    missing.push(name);
                    path = path
                        .parent()
                        .filter(|p| !p.as_os_str().is_empty())
                        .unwrap_or(Path::new("."))
                        .into();
                }
                Err(error) => return Err(io_error("resolve path inside root", error)),
            }
        }
    }

    /// Translate internal absolute symlinks back into capability-relative paths.
    /// All link inspection uses the root handle, never ambient filesystem access.
    fn expand_links(&self, mut path: PathBuf) -> ToolResult<PathBuf> {
        for _ in 0..40 {
            let parts: Vec<_> = path
                .components()
                .map(|p| p.as_os_str().to_owned())
                .collect();
            let mut prefix = PathBuf::new();
            let mut expanded = None;
            for (index, part) in parts.iter().enumerate() {
                prefix.push(part);
                if let Ok(target) = self.directory.read_link_contents(&prefix) {
                    let mut target = if target.is_absolute() {
                        target
                            .strip_prefix(&self.capability_root)
                            .map_err(|_| {
                                failed(
                                    "symbolic link points outside the local filesystem capability root",
                                )
                            })?
                            .to_owned()
                    } else {
                        prefix.parent().unwrap_or(Path::new(".")).join(target)
                    };
                    if self
                        .directory
                        .symlink_metadata(&target)
                        .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
                    {
                        return Err(failed("cannot resolve dangling symbolic link"));
                    }
                    for suffix in &parts[index + 1..] {
                        target.push(suffix);
                    }
                    expanded = Some(target);
                    break;
                }
            }
            match expanded {
                Some(target) => path = target,
                None => return Ok(path),
            }
        }
        Err(failed("too many symbolic links"))
    }

    pub fn output_limit(&self, context: &ToolContext) -> usize {
        self.config
            .max_output_bytes
            .min(context.max_output_bytes.saturating_sub(12))
    }
    fn lock(&self, path: &Path) -> Arc<Mutex<()>> {
        let mut locks = self.mutations.lock().unwrap_or_else(|p| p.into_inner());
        locks.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = locks.get(path).and_then(Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(Mutex::new(()));
        locks.insert(path.into(), Arc::downgrade(&lock));
        lock
    }
    pub fn check_size(&self, bytes: usize) -> ToolResult<()> {
        if self.config.max_file_bytes.is_some_and(|cap| bytes > cap) {
            return Err(failed("file exceeds the host max_file_bytes limit"));
        }
        Ok(())
    }
}

#[derive(Clone)]
pub(super) struct Operation {
    pub cancellation: CancellationToken,
    pub deadline: Instant,
    pub output_limit: usize,
    pub session: Arc<LocalSession>,
}
impl Operation {
    pub fn check(&self) -> ToolResult<()> {
        if self.cancellation.is_cancelled() || Instant::now() >= self.deadline {
            return Err(ToolError::Uncertain(
                "local operation interrupted; verify any filesystem changes before continuing"
                    .into(),
            ));
        }
        Ok(())
    }
}

/// Blocking syscalls and per-target mutation locks stay off the async runtime.
pub(super) async fn run_filesystem<F>(
    workspace: Arc<Workspace>,
    context: ToolContext,
    input: String,
    mutation: bool,
    work: F,
) -> ToolResult<ToolOutput>
where
    F: FnOnce(&Workspace, &Path, &Operation) -> ToolResult<ToolOutput> + Send + 'static,
{
    let cancellation = context.cancellation.child_token();
    let _guard = cancellation.clone().drop_guard();
    let operation = Operation {
        cancellation: cancellation.clone(),
        deadline: context.deadline,
        output_limit: workspace.output_limit(&context),
        session: context.local_session.clone(),
    };
    operation.check()?;
    let handle = tokio::task::spawn_blocking(move || {
        operation.check()?;
        let path = workspace.resolve(&input)?;
        if mutation {
            workspace.enforce_mutation(&path)?;
        }
        let lock = mutation.then(|| workspace.lock(&path));
        let _lock = lock
            .as_ref()
            .map(|lock| lock.lock().unwrap_or_else(|p| p.into_inner()));
        operation.check()?;
        work(&workspace, &path, &operation)
    });
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err(ToolError::Uncertain("local operation cancelled; verify its result".into())),
        _ = tokio::time::sleep_until(context.deadline) => Err(ToolError::Uncertain("local operation timed out; verify its result".into())),
        result = handle => result.map_err(|_| ToolError::Uncertain("filesystem worker failed; verify its result".into()))?,
    }
}

pub(super) fn inspect(directory: &Dir, path: &Path) -> ToolResult<Option<Metadata>> {
    match directory.symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => Ok(Some(metadata)),
        Ok(_) => Err(failed("FS_NOT_REGULAR_FILE: path is not a regular file")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(io_error("inspect file", error)),
    }
}

pub(super) fn open_file(workspace: &Workspace, path: &Path) -> ToolResult<(File, Metadata)> {
    if inspect(&workspace.directory, path)?.is_none() {
        return Err(failed("FS_NOT_FOUND: file does not exist"));
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.custom_flags(
            (rustix::fs::OFlags::NONBLOCK | rustix::fs::OFlags::NOFOLLOW).bits() as i32,
        );
    }
    let file = workspace
        .directory
        .open_with(path, &options)
        .map_err(|e| io_error("open file", e))?;
    let metadata = file
        .metadata()
        .map_err(|e| io_error("inspect open file", e))?;
    if !metadata.is_file() {
        return Err(failed("FS_NOT_REGULAR_FILE: path is not a regular file"));
    }
    if workspace
        .config
        .max_file_bytes
        .is_some_and(|cap| metadata.len() > cap as u64)
    {
        return Err(failed("file exceeds the host max_file_bytes limit"));
    }
    Ok((file, metadata))
}

pub(super) fn read_file(
    workspace: &Workspace,
    path: &Path,
    operation: &Operation,
) -> ToolResult<String> {
    read_file_limited(workspace, path, operation, None)
}

pub(super) fn read_file_limited(
    workspace: &Workspace,
    path: &Path,
    operation: &Operation,
    limit: Option<usize>,
) -> ToolResult<String> {
    let (mut file, metadata) = open_file(workspace, path)?;
    let mut bytes = Vec::new();
    let mut buffer = [0; 8192];
    loop {
        operation.check()?;
        let count = file
            .read(&mut buffer)
            .map_err(|e| io_error("read file", e))?;
        if count == 0 {
            break;
        }
        workspace.check_size(bytes.len().saturating_add(count))?;
        if limit.is_some_and(|limit| bytes.len().saturating_add(count) >= limit) {
            return Err(failed("file exceeds the optional diff basis limit"));
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
    verify(&workspace.directory, path, Some(&metadata))?;
    let text = String::from_utf8(bytes).map_err(|_| failed("FS_NOT_TEXT: invalid UTF-8 text"))?;
    if text.contains('\0') {
        return Err(failed("FS_NOT_TEXT: binary file"));
    }
    Ok(text.strip_prefix('\u{feff}').unwrap_or(&text).to_owned())
}

pub(super) fn verify(directory: &Dir, path: &Path, previous: Option<&Metadata>) -> ToolResult<()> {
    if inspect(directory, path)?.as_ref().map(version) != previous.map(version) {
        return Err(failed(
            "FS_STALE_VERSION: file changed during the operation; read it again",
        ));
    }
    Ok(())
}

/// Private staging plus atomic publication. Creation uses a no-replace hard link.
pub(super) fn replace(
    workspace: &Workspace,
    path: &Path,
    content: &str,
    previous: Option<&Metadata>,
    operation: &Operation,
) -> ToolResult<FileVersion> {
    workspace.check_size(content.len())?;
    operation.check()?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    workspace
        .directory
        .create_dir_all(parent)
        .map_err(|e| io_error("create parent directories", e))?;
    let directory = workspace
        .directory
        .open_dir(parent)
        .map_err(|e| io_error("open parent directory", e))?;
    let name = path
        .file_name()
        .ok_or_else(|| failed("file_path must name a file"))?;
    let stage_name = PathBuf::from(format!(".abycore-{:032x}.tmpdir", rand::random::<u128>()));
    let builder = {
        let mut builder = DirBuilder::new();
        // 0700 is a unix mode; Windows inherits the parent directory's ACL.
        #[cfg(unix)]
        {
            use cap_std::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder
    };
    directory
        .create_dir_with(&stage_name, &builder)
        .map_err(|e| io_error("create staging directory", e))?;
    let mut temporary = Temporary {
        parent: &directory,
        name: stage_name,
        directory: None,
    };
    let staging = directory
        .open_dir(&temporary.name)
        .map_err(|e| io_error("open staging directory", e))?;
    temporary.directory = Some(staging);
    let staging = temporary.directory.as_ref().expect("staging directory");
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = staging
        .open_with("content", &options)
        .map_err(|e| io_error("create temporary file", e))?;
    for chunk in content.as_bytes().chunks(8192) {
        operation.check()?;
        file.write_all(chunk)
            .map_err(|e| io_error("write temporary file", e))?;
    }
    if let Some(previous) = previous {
        file.set_permissions(previous.permissions())
            .map_err(|e| io_error("preserve permissions", e))?;
    }
    file.sync_all()
        .map_err(|e| io_error("sync temporary file", e))?;
    let staged = version(
        &file
            .metadata()
            .map_err(|e| io_error("inspect temporary file", e))?,
    );
    drop(file);
    verify(&directory, Path::new(name), previous)?;
    operation.check()?;
    if previous.is_none() {
        staging
            .hard_link("content", &directory, name)
            .map_err(|e| io_error("create file without overwriting an existing destination", e))?;
    } else {
        staging
            .rename("content", &directory, name)
            .map_err(|e| io_error("replace file", e))?;
    }
    // Link removal changes ctime too: complete staging cleanup before observing.
    drop(temporary);
    let committed = inspect(&directory, Path::new(name))
        .map_err(|_| {
            ToolError::Uncertain("file was published but its result could not be inspected".into())
        })?
        .ok_or_else(|| ToolError::Uncertain("file disappeared after publication".into()))?;
    let published = version(&committed);
    // Publication changes ctime; inode, size and mtime must still identify our staged content.
    let same_content = staged.len == published.len && staged.modified == published.modified;
    #[cfg(unix)]
    let same_content = same_content
        && staged.identity.0 == published.identity.0
        && staged.identity.1 == published.identity.1;
    if !same_content {
        return Err(ToolError::Uncertain(
            "file changed immediately after publication; read and verify it".into(),
        ));
    }
    #[cfg(unix)]
    directory
        .open(".")
        .and_then(|file| file.sync_all())
        .map_err(|_| {
            ToolError::Uncertain(
                "file was replaced but directory sync failed; verify its result".into(),
            )
        })?;
    Ok(published)
}

struct Temporary<'a> {
    parent: &'a Dir,
    name: PathBuf,
    directory: Option<Dir>,
}
impl Drop for Temporary<'_> {
    fn drop(&mut self) {
        if let Some(directory) = self.directory.take() {
            let _ = directory.remove_file("content");
        }
        let _ = self.parent.remove_dir(&self.name);
    }
}

pub(super) fn failed(message: impl Into<String>) -> ToolError {
    ToolError::Failed(message.into())
}
pub(super) fn io_error(action: &str, error: std::io::Error) -> ToolError {
    failed(format!("cannot {action}: {error}"))
}
pub(super) fn parse<T: serde::de::DeserializeOwned>(value: &serde_json::Value) -> ToolResult<T> {
    serde_json::from_value(value.clone())
        .map_err(|error| failed(format!("invalid tool arguments: {error}")))
}
