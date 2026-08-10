//! Local build-context handling for native builds: `.dockerignore`
//! filtering, source selection with Docker-style glob patterns, destination
//! planning, and tar packaging of the content streamed into the build VM.
//!
//! All selection happens on the client against the local filesystem; only
//! regular files and directories are allowed, and every path is validated to
//! stay inside the context directory. The produced tar contains only clean
//! relative paths, so guest-side extraction with `tar -x -C <root>` cannot
//! escape the destination.

use anyhow::{anyhow, bail, Context, Result};
use cap_fs_ext::{
    FollowSymlinks, OpenOptionsFollowExt, OpenOptionsMaybeDirExt, OpenOptionsSyncExt, OsMetadataExt,
};
use cap_std::ambient_authority;
use cap_std::fs::{Dir, File as CapabilityFile, Metadata as CapabilityMetadata, OpenOptions};
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

#[cfg(windows)]
use std::os::windows::ffi::OsStringExt;
#[cfg(windows)]
use std::os::windows::io::AsRawHandle;
#[cfg(windows)]
use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_NO_MORE_FILES};
#[cfg(windows)]
use windows_sys::Win32::Storage::FileSystem::{
    FileFullDirectoryInfo, FileFullDirectoryRestartInfo, GetFileInformationByHandleEx,
    FILE_FULL_DIR_INFO,
};

const DOCKERIGNORE_FILE: &str = ".dockerignore";

pub(crate) struct BuildContext {
    root: Arc<ContextRoot>,
    ignore: IgnoreRules,
}

impl BuildContext {
    pub(crate) fn load(root: &Path) -> Result<Self> {
        let root = Arc::new(ContextRoot::open(root)?);
        let dockerignore = ContextPath::new(root.clone(), PathBuf::from(DOCKERIGNORE_FILE))?;
        let ignore = match dockerignore.read_to_string() {
            Ok(text) => IgnoreRules::parse(&text)
                .with_context(|| format!("parsing {}", dockerignore.display()))?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => IgnoreRules::default(),
            Err(err) => {
                return Err(err).with_context(|| format!("reading {}", dockerignore.display()))
            }
        };
        Ok(Self { root, ignore })
    }

    #[cfg(test)]
    pub(crate) fn root(&self) -> &Path {
        &self.root.display
    }

    /// Selects the local sources for one COPY/ADD instruction. Globs are
    /// expanded, `.dockerignore` is honored, and every selected path must be
    /// a regular file or a directory reachable without leaving the context.
    pub(crate) fn select_sources(&self, patterns: &[String]) -> Result<Vec<SelectedSource>> {
        let mut selected = Vec::new();
        for pattern in patterns {
            let normalized = normalize_context_pattern(pattern)?;
            if has_glob_meta(&normalized) {
                let matches = self.select_glob(&normalized)?;
                if matches.is_empty() {
                    bail!("source {pattern:?} matched no files in the build context");
                }
                selected.extend(matches);
            } else {
                selected.push(self.select_exact(pattern, &normalized)?);
            }
        }
        Ok(selected)
    }

    fn select_exact(&self, original: &str, normalized: &str) -> Result<SelectedSource> {
        let relative = if normalized == "." {
            PathBuf::new()
        } else {
            PathBuf::from(normalized)
        };
        let path = ContextPath::new(self.root.clone(), relative.clone())?;
        let opened = match path.open_node_io() {
            Ok(opened) => opened,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                bail!("source {original:?} was not found in the build context");
            }
            Err(err) => {
                bail!(
                    "opening source {original:?} without following symbolic links or reparse \
                     points: {err}"
                );
            }
        };
        let is_dir = opened.is_dir();
        if normalized != "." && self.ignore.is_ignored(&relative, is_dir) {
            bail!("source {original:?} is excluded by {DOCKERIGNORE_FILE}");
        }
        Ok(SelectedSource {
            path,
            relative,
            is_dir,
        })
    }

    fn select_glob(&self, pattern: &str) -> Result<Vec<SelectedSource>> {
        let segments = pattern_segments(pattern)?;
        let mut matches = Vec::new();
        let root = ContextPath::new(self.root.clone(), PathBuf::new())?;
        for entry in self.walk_filtered(&root)? {
            let relative_segments: Vec<&str> = entry
                .relative
                .iter()
                .map(|part| part.to_str().expect("selection enforces UTF-8 names"))
                .collect();
            if segments_match(&segments, &relative_segments) {
                matches.push(entry);
            }
        }
        matches.sort_by(|left, right| left.relative.cmp(&right.relative));
        // A glob may match both a directory and paths inside it; keep only
        // the outermost matches so content is not packaged twice.
        let mut outermost: Vec<SelectedSource> = Vec::new();
        for candidate in matches {
            let nested = outermost
                .iter()
                .any(|kept| kept.is_dir && candidate.relative.starts_with(&kept.relative));
            if !nested {
                outermost.push(candidate);
            }
        }
        Ok(outermost)
    }

    /// Walks a directory, skipping ignored paths, rejecting symlinks and
    /// special files, and enforcing UTF-8 names. Returned paths are relative
    /// to the context root.
    fn walk_filtered(&self, dir: &ContextPath) -> Result<Vec<SelectedSource>> {
        self.walk_filtered_impl(dir, |_, _| Ok(()))
    }

    fn walk_filtered_impl<F>(
        &self,
        dir: &ContextPath,
        mut after_open: F,
    ) -> Result<Vec<SelectedSource>>
    where
        F: FnMut(&ContextPath, bool) -> Result<()>,
    {
        let opened = dir.open_node().with_context(|| {
            format!(
                "opening directory {} without following symbolic links or reparse points",
                dir.display()
            )
        })?;
        let OpenedNode::Directory(directory) = opened else {
            bail!("source is no longer a directory: {}", dir.display());
        };
        let mut entries = Vec::new();
        self.walk_open_dir(directory, &dir.relative, &mut entries, &mut after_open)?;
        Ok(entries)
    }

    fn walk_open_dir<F>(
        &self,
        directory: Dir,
        relative_dir: &Path,
        entries: &mut Vec<SelectedSource>,
        after_open: &mut F,
    ) -> Result<()>
    where
        F: FnMut(&ContextPath, bool) -> Result<()>,
    {
        let mut names = directory_entry_names(&directory).with_context(|| {
            format!("walking {}", self.root.display_path(relative_dir).display())
        })?;
        names.sort();

        for name in names {
            validate_context_name(&name)?;
            let relative = relative_dir.join(&name);
            let path = ContextPath::new(self.root.clone(), relative.clone())?;
            let opened = open_child_nofollow(&directory, &name).with_context(|| {
                format!(
                    "opening context entry {} without following symbolic links or reparse points",
                    path.display()
                )
            })?;
            let is_dir = opened.is_dir();
            after_open(&path, is_dir)?;

            let ignored = self.ignore.is_ignored(&relative, is_dir);
            match opened {
                OpenedNode::Directory(child) => {
                    if !ignored {
                        entries.push(SelectedSource {
                            path: path.clone(),
                            relative: relative.clone(),
                            is_dir: true,
                        });
                    }
                    if !ignored || self.ignore.has_negations() {
                        self.walk_open_dir(child, &relative, entries, after_open)?;
                    }
                }
                OpenedNode::File(_) => {
                    if !ignored {
                        entries.push(SelectedSource {
                            path,
                            relative,
                            is_dir: false,
                        });
                    }
                }
            }
        }
        Ok(())
    }

    /// Walks a directory source purely for validation so symlinks, special
    /// files, and non-UTF-8 names fail before the build VM is created.
    pub(crate) fn validate_directory_source(&self, source: &SelectedSource) -> Result<()> {
        self.walk_filtered(&source.path).map(|_| ())
    }

    /// Expands one selected directory source into the relative file/dir
    /// entries beneath it, honoring `.dockerignore`.
    fn directory_contents(
        &self,
        source: &SelectedSource,
    ) -> Result<Vec<(ContextPath, PathBuf, bool)>> {
        let entries = self.walk_filtered(&source.path)?;
        Ok(entries
            .into_iter()
            .map(|entry| {
                let below = entry
                    .relative
                    .strip_prefix(&source.relative)
                    .unwrap_or(&entry.relative)
                    .to_path_buf();
                (entry.path, below, entry.is_dir)
            })
            .collect())
    }
}

struct ContextRoot {
    display: PathBuf,
    directory: Dir,
}

impl ContextRoot {
    fn open(path: &Path) -> Result<Self> {
        let options = nofollow_open_options();
        let file = CapabilityFile::open_ambient_with(path, &options, ambient_authority())
            .with_context(|| format!("opening build context {}", path.display()))?;
        let metadata = file
            .metadata()
            .with_context(|| format!("reading build context {}", path.display()))?;
        reject_symlink_or_reparse(&metadata).with_context(|| {
            format!(
                "build context cannot be a symbolic link or reparse point: {}",
                path.display()
            )
        })?;
        if !metadata.is_dir() {
            bail!(
                "build context must be a directory: {} (pass the Dockerfile with -f and the \
                 context directory as the positional argument)",
                path.display()
            );
        }
        Ok(Self {
            display: path.to_path_buf(),
            directory: Dir::from_std_file(file.into_std()),
        })
    }

    fn open_node(&self, relative: &Path) -> std::io::Result<OpenedNode> {
        validate_context_relative_path_io(relative)?;
        if relative.as_os_str().is_empty() {
            return self.directory.try_clone().map(OpenedNode::Directory);
        }

        let mut directory = self.directory.try_clone()?;
        let mut components = relative.components().peekable();
        while let Some(component) = components.next() {
            let Component::Normal(name) = component else {
                return Err(invalid_context_path(relative));
            };
            let opened = open_child_nofollow(&directory, name)?;
            if components.peek().is_none() {
                return Ok(opened);
            }
            match opened {
                OpenedNode::Directory(child) => directory = child,
                OpenedNode::File(_) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::NotADirectory,
                        format!(
                            "context path ancestor is not a directory: {}",
                            relative.display()
                        ),
                    ));
                }
            }
        }
        unreachable!("non-empty relative paths have at least one component")
    }

    fn display_path(&self, relative: &Path) -> PathBuf {
        if relative.as_os_str().is_empty() {
            self.display.clone()
        } else {
            self.display.join(relative)
        }
    }
}

#[derive(Clone)]
pub(crate) struct ContextPath {
    root: Arc<ContextRoot>,
    relative: PathBuf,
    display: PathBuf,
}

impl ContextPath {
    fn new(root: Arc<ContextRoot>, relative: PathBuf) -> Result<Self> {
        validate_context_relative_path(&relative)?;
        let display = root.display_path(&relative);
        Ok(Self {
            root,
            relative,
            display,
        })
    }

    pub(crate) fn display(&self) -> std::path::Display<'_> {
        self.display.display()
    }

    fn open_node(&self) -> Result<OpenedNode> {
        self.open_node_io().map_err(Into::into)
    }

    fn open_node_io(&self) -> std::io::Result<OpenedNode> {
        self.root.open_node(&self.relative)
    }

    fn read_to_string(&self) -> std::io::Result<String> {
        let OpenedNode::File(mut file) = self.open_node_io()? else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::IsADirectory,
                format!("context path is a directory: {}", self.display()),
            ));
        };
        let mut text = String::new();
        file.read_to_string(&mut text)?;
        Ok(text)
    }

    #[cfg(test)]
    fn display_path(&self) -> &Path {
        &self.display
    }
}

impl fmt::Debug for ContextPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ContextPath")
            .field("relative", &self.relative)
            .field("display", &self.display)
            .finish_non_exhaustive()
    }
}

enum OpenedNode {
    File(CapabilityFile),
    Directory(Dir),
}

impl OpenedNode {
    fn is_dir(&self) -> bool {
        matches!(self, Self::Directory(_))
    }

    fn metadata(&self) -> std::io::Result<CapabilityMetadata> {
        match self {
            Self::File(file) => file.metadata(),
            Self::Directory(directory) => directory.dir_metadata(),
        }
    }
}

fn nofollow_open_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .follow(FollowSymlinks::No)
        .maybe_dir(true)
        .nonblock(true);
    options
}

fn open_child_nofollow(directory: &Dir, name: &OsStr) -> std::io::Result<OpenedNode> {
    validate_context_name_io(name)?;
    open_child_nofollow_raw(directory, name)
}

fn open_child_nofollow_raw(directory: &Dir, name: &OsStr) -> std::io::Result<OpenedNode> {
    let file = directory.open_with(Path::new(name), &nofollow_open_options())?;
    let metadata = file.metadata()?;
    reject_symlink_or_reparse(&metadata)?;
    if metadata.is_dir() {
        Ok(OpenedNode::Directory(Dir::from_std_file(file.into_std())))
    } else if metadata.is_file() {
        Ok(OpenedNode::File(file))
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "special files are not supported in the build context",
        ))
    }
}

#[cfg(not(windows))]
fn directory_entry_names(directory: &Dir) -> std::io::Result<Vec<OsString>> {
    directory
        .entries()?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect()
}

#[cfg(windows)]
fn directory_entry_names(directory: &Dir) -> std::io::Result<Vec<OsString>> {
    const BUFFER_SIZE: usize = 64 * 1024;

    let mut buffer = vec![0_u64; BUFFER_SIZE / std::mem::size_of::<u64>()];
    let buffer_size = u32::try_from(BUFFER_SIZE).expect("64 KiB fits in u32");
    let mut names = Vec::new();
    let mut information_class = FileFullDirectoryRestartInfo;

    loop {
        buffer.fill(0);
        let succeeded = unsafe {
            GetFileInformationByHandleEx(
                directory.as_raw_handle(),
                information_class,
                buffer.as_mut_ptr().cast(),
                buffer_size,
            )
        };
        if succeeded == 0 {
            let err = std::io::Error::last_os_error();
            let code = err.raw_os_error();
            if code == Some(ERROR_NO_MORE_FILES as i32)
                || (information_class == FileFullDirectoryRestartInfo
                    && code == Some(ERROR_FILE_NOT_FOUND as i32))
            {
                break;
            }
            return Err(err);
        }
        parse_windows_directory_buffer(&buffer, &mut names)?;
        information_class = FileFullDirectoryInfo;
    }
    Ok(names)
}

#[cfg(windows)]
fn parse_windows_directory_buffer(
    buffer: &[u64],
    names: &mut Vec<OsString>,
) -> std::io::Result<()> {
    let bytes = unsafe {
        std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), std::mem::size_of_val(buffer))
    };
    let record_size = std::mem::size_of::<FILE_FULL_DIR_INFO>();
    let name_offset = std::mem::offset_of!(FILE_FULL_DIR_INFO, FileName);
    let mut offset = 0_usize;

    loop {
        let record_end = offset.checked_add(record_size).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "directory entry offset overflowed",
            )
        })?;
        if record_end > bytes.len() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "directory enumeration returned a truncated record",
            ));
        }
        let record = unsafe {
            std::ptr::read_unaligned(bytes.as_ptr().add(offset).cast::<FILE_FULL_DIR_INFO>())
        };
        let name_len = usize::try_from(record.FileNameLength).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "directory entry name length does not fit in memory",
            )
        })?;
        if name_len % std::mem::size_of::<u16>() != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "directory enumeration returned an invalid UTF-16 byte length",
            ));
        }
        let name_start = offset.checked_add(name_offset).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "directory entry name offset overflowed",
            )
        })?;
        let name_end = name_start.checked_add(name_len).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "directory entry name length overflowed",
            )
        })?;
        let name_bytes = bytes.get(name_start..name_end).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "directory enumeration returned a truncated name",
            )
        })?;
        let wide_name: Vec<u16> = name_bytes
            .chunks_exact(2)
            .map(|bytes| u16::from_ne_bytes([bytes[0], bytes[1]]))
            .collect();
        let name = OsString::from_wide(&wide_name);
        if name != OsStr::new(".") && name != OsStr::new("..") {
            names.push(name);
        }

        if record.NextEntryOffset == 0 {
            break;
        }
        let next = usize::try_from(record.NextEntryOffset).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "directory entry offset does not fit in memory",
            )
        })?;
        if next < record_size {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "directory enumeration returned a non-progressing entry offset",
            ));
        }
        offset = offset.checked_add(next).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "directory entry offset overflowed",
            )
        })?;
    }
    Ok(())
}

fn reject_symlink_or_reparse(metadata: &CapabilityMetadata) -> std::io::Result<()> {
    if metadata.file_type().is_symlink() || metadata_is_reparse_point(metadata) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "symbolic links and reparse points are not supported in the build context",
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn metadata_is_reparse_point(metadata: &CapabilityMetadata) -> bool {
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn metadata_is_reparse_point(_metadata: &CapabilityMetadata) -> bool {
    false
}

fn validate_context_name(name: &OsStr) -> Result<()> {
    validate_context_name_io(name).map_err(Into::into)
}

fn validate_context_name_io(name: &OsStr) -> std::io::Result<()> {
    let Some(name) = name.to_str() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "context entries must have valid UTF-8 names",
        ));
    };
    if name.is_empty() || name == "." || name == ".." || name.contains('\\') || name.contains(':') {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("invalid or non-portable context path component: {name:?}"),
        ));
    }
    Ok(())
}

fn validate_context_relative_path(path: &Path) -> Result<()> {
    validate_context_relative_path_io(path).map_err(Into::into)
}

fn validate_context_relative_path_io(path: &Path) -> std::io::Result<()> {
    if path
        .as_os_str()
        .to_str()
        .is_some_and(|path| path.contains('\\'))
    {
        return Err(invalid_context_path(path));
    }
    for component in path.components() {
        match component {
            Component::Normal(name) => validate_context_name_io(name)?,
            _ => return Err(invalid_context_path(path)),
        }
    }
    Ok(())
}

fn invalid_context_path(path: &Path) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        format!(
            "context paths must be relative and cannot contain prefixes, roots, parent \
             components, or backslashes: {}",
            path.display()
        ),
    )
}

fn looks_like_windows_prefix(component: &str) -> bool {
    let bytes = component.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

#[derive(Clone, Debug)]
pub(crate) struct SelectedSource {
    /// Context-relative path backed by the fixed context directory handle.
    pub path: ContextPath,
    /// Path relative to the context root ("." source keeps an empty path).
    pub relative: PathBuf,
    pub is_dir: bool,
}

impl SelectedSource {
    fn base_name(&self) -> Result<&str> {
        self.relative
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| anyhow!("source has no usable file name: {}", self.path.display()))
    }
}

/// Normalizes a COPY/ADD source pattern: leading slashes are context-root
/// relative (Docker semantics), `.` segments collapse, and `..` may not
/// escape the context.
fn normalize_context_pattern(pattern: &str) -> Result<String> {
    let trimmed = pattern.trim();
    if trimmed.is_empty() {
        bail!("source pattern cannot be empty");
    }
    if trimmed.contains('\\') {
        bail!("source {pattern:?} contains a backslash and is not a portable context path");
    }
    if trimmed.starts_with("//") {
        bail!("source {pattern:?} uses a Windows/UNC path prefix");
    }
    let mut segments: Vec<&str> = Vec::new();
    for segment in trimmed.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                if segments.pop().is_none() {
                    bail!("source {pattern:?} escapes the build context");
                }
            }
            segment => segments.push(segment),
        }
    }
    if segments.is_empty() {
        return Ok(".".to_string());
    }
    if looks_like_windows_prefix(segments[0]) {
        bail!("source {pattern:?} uses a Windows path prefix");
    }
    let normalized = segments.join("/");
    validate_context_relative_path(Path::new(&normalized))
        .with_context(|| format!("invalid source pattern {pattern:?}"))?;
    Ok(normalized)
}

fn has_glob_meta(pattern: &str) -> bool {
    pattern.contains(['*', '?', '['])
}

fn pattern_segments(pattern: &str) -> Result<Vec<String>> {
    let segments: Vec<String> = pattern
        .split('/')
        .filter(|segment| !segment.is_empty() && *segment != ".")
        .map(str::to_string)
        .collect();
    for segment in &segments {
        validate_segment_pattern(segment)
            .with_context(|| format!("invalid pattern {pattern:?}"))?;
    }
    Ok(segments)
}

// ---------------------------------------------------------------------------
// .dockerignore
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
pub(crate) struct IgnoreRules {
    patterns: Vec<IgnorePattern>,
    negations: bool,
}

#[derive(Debug)]
struct IgnorePattern {
    segments: Vec<String>,
    negated: bool,
}

impl IgnoreRules {
    pub(crate) fn parse(text: &str) -> Result<Self> {
        let mut patterns = Vec::new();
        let mut negations = false;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (negated, body) = match line.strip_prefix('!') {
                Some(rest) => (true, rest.trim()),
                None => (false, line),
            };
            let normalized = match normalize_ignore_pattern(body) {
                Some(normalized) => normalized,
                None => continue,
            };
            let segments = pattern_segments(&normalized)
                .with_context(|| format!("invalid {DOCKERIGNORE_FILE} pattern {line:?}"))?;
            negations |= negated;
            patterns.push(IgnorePattern { segments, negated });
        }
        Ok(Self {
            patterns,
            negations,
        })
    }

    pub(crate) fn has_negations(&self) -> bool {
        self.negations
    }

    /// Docker-style matching: the last pattern that matches the path or one
    /// of its parent directories decides, and `!` patterns re-include.
    pub(crate) fn is_ignored(&self, relative: &Path, _is_dir: bool) -> bool {
        if self.patterns.is_empty() {
            return false;
        }
        let segments: Vec<&str> = relative.iter().filter_map(|part| part.to_str()).collect();
        if segments.is_empty() {
            return false;
        }
        let mut ignored = false;
        for pattern in &self.patterns {
            if pattern_matches_path_or_parent(&pattern.segments, &segments) {
                ignored = !pattern.negated;
            }
        }
        ignored
    }
}

/// Normalizes a `.dockerignore` pattern body: strips leading/trailing
/// slashes, resolves `.`/`..` lexically, and drops patterns that reduce to
/// the whole context.
fn normalize_ignore_pattern(body: &str) -> Option<String> {
    let mut segments: Vec<&str> = Vec::new();
    for segment in body.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            segment => segments.push(segment),
        }
    }
    if segments.is_empty() {
        return None;
    }
    Some(segments.join("/"))
}

fn pattern_matches_path_or_parent(pattern: &[String], path: &[&str]) -> bool {
    for prefix_len in 1..=path.len() {
        if segments_match(pattern, &path[..prefix_len]) {
            return true;
        }
    }
    false
}

/// Matches pattern segments against path segments. `**` spans zero or more
/// segments except in the final position, where (like Docker) it requires at
/// least one segment so `a/**` matches contents of `a` but not `a` itself.
fn segments_match(pattern: &[String], path: &[&str]) -> bool {
    match pattern.first() {
        None => path.is_empty(),
        Some(segment) if segment == "**" => {
            if pattern.len() == 1 {
                return !path.is_empty();
            }
            if segments_match(&pattern[1..], path) {
                return true;
            }
            !path.is_empty() && segments_match(pattern, &path[1..])
        }
        Some(segment) => {
            !path.is_empty()
                && segment_match(segment, path[0])
                && segments_match(&pattern[1..], &path[1..])
        }
    }
}

/// Single-segment glob matching with Go `filepath.Match` semantics: `*`
/// (any run within the segment), `?` (one character), `[...]` classes with
/// `^`/`!` negation and ranges, and `\` escaping the next character.
fn segment_match(pattern: &str, name: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let name: Vec<char> = name.chars().collect();
    match_chars(&pattern, &name)
}

fn match_chars(pattern: &[char], name: &[char]) -> bool {
    let Some(&first) = pattern.first() else {
        return name.is_empty();
    };
    match first {
        '*' => {
            match_chars(&pattern[1..], name)
                || (!name.is_empty() && match_chars(pattern, &name[1..]))
        }
        '?' => !name.is_empty() && match_chars(&pattern[1..], &name[1..]),
        '[' => {
            let Some((matched_len, matches)) = match_class(&pattern[1..], name.first().copied())
            else {
                return false;
            };
            matches && match_chars(&pattern[1 + matched_len..], &name[1..])
        }
        '\\' => {
            pattern.len() > 1
                && !name.is_empty()
                && pattern[1] == name[0]
                && match_chars(&pattern[2..], &name[1..])
        }
        ch => !name.is_empty() && ch == name[0] && match_chars(&pattern[1..], &name[1..]),
    }
}

/// Matches a character class body (after `[`). Returns the consumed pattern
/// length including the closing `]` and whether the candidate matched.
fn match_class(body: &[char], candidate: Option<char>) -> Option<(usize, bool)> {
    let candidate = candidate?;
    let mut index = 0;
    let negated = matches!(body.first(), Some('^' | '!'));
    if negated {
        index += 1;
    }
    let mut matched = false;
    let mut first = true;
    loop {
        let &ch = body.get(index)?;
        if ch == ']' && !first {
            index += 1;
            break;
        }
        first = false;
        let low = if ch == '\\' {
            index += 1;
            *body.get(index)?
        } else {
            ch
        };
        index += 1;
        let high = if body.get(index) == Some(&'-') && body.get(index + 1) != Some(&']') {
            index += 1;
            let &next = body.get(index)?;
            let next = if next == '\\' {
                index += 1;
                *body.get(index)?
            } else {
                next
            };
            index += 1;
            next
        } else {
            low
        };
        if low <= candidate && candidate <= high {
            matched = true;
        }
    }
    Some((index, matched != negated))
}

fn validate_segment_pattern(segment: &str) -> Result<()> {
    let chars: Vec<char> = segment.chars().collect();
    let mut index = 0;
    while index < chars.len() {
        match chars[index] {
            '\\' => {
                if index + 1 >= chars.len() {
                    bail!("pattern segment ends with a bare escape: {segment:?}");
                }
                index += 2;
            }
            '[' => {
                let Some((consumed, _)) = match_class(&chars[index + 1..], Some('\0')) else {
                    bail!("pattern segment has an unterminated character class: {segment:?}");
                };
                index += 1 + consumed;
            }
            _ => index += 1,
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Destination planning
// ---------------------------------------------------------------------------

/// What the guest reported about the destination path before transfer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GuestPathKind {
    Directory,
    File,
    Missing,
}

/// How the transferred content lands in the guest.
#[derive(Debug, PartialEq)]
pub(crate) enum DestPlan {
    /// Copy sources into `root`: file sources keep their base name, and
    /// directory sources contribute their contents (Docker semantics).
    Directory { root: String },
    /// Replace or create the single file `root/file_name`.
    SingleFile { root: String, file_name: String },
}

impl DestPlan {
    pub(crate) fn extraction_root(&self) -> &str {
        match self {
            DestPlan::Directory { root } => root,
            DestPlan::SingleFile { root, .. } => root,
        }
    }
}

/// Resolves a Dockerfile destination against the effective working
/// directory. Returns the absolute path (normalized lexically) and whether
/// the raw destination explicitly named a directory with a trailing slash
/// (or was `.`/relative-dir shaped).
pub(crate) fn resolve_guest_dest(dest: &str, workdir: &str) -> Result<(String, bool)> {
    let trimmed = dest.trim();
    if trimmed.is_empty() {
        bail!("destination cannot be empty");
    }
    let wants_directory =
        trimmed.ends_with('/') || trimmed == "." || trimmed.ends_with("/.") || trimmed == "..";
    let joined = if trimmed.starts_with('/') {
        trimmed.to_string()
    } else {
        let base = if workdir.is_empty() { "/" } else { workdir };
        format!("{}/{}", base.trim_end_matches('/'), trimmed)
    };
    let mut segments: Vec<&str> = Vec::new();
    for segment in joined.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            segment => segments.push(segment),
        }
    }
    let absolute = if segments.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", segments.join("/"))
    };
    Ok((absolute, wants_directory))
}

/// Decides how one COPY/ADD instruction lands in the guest, given the
/// stat of the resolved destination. Ambiguous forms are rejected before any
/// transfer happens.
pub(crate) fn plan_destination(
    instruction: &str,
    sources: &[SelectedSource],
    dest: &str,
    dest_wants_directory: bool,
    stat: GuestPathKind,
) -> Result<DestPlan> {
    if sources.is_empty() {
        bail!("{instruction} requires at least one source");
    }
    if dest_wants_directory || stat == GuestPathKind::Directory || dest == "/" {
        return Ok(DestPlan::Directory {
            root: dest.to_string(),
        });
    }

    let (parent, file_name) = split_guest_path(dest)?;
    match stat {
        GuestPathKind::Directory => unreachable!("handled above"),
        GuestPathKind::File => {
            if let [source] = sources {
                if !source.is_dir {
                    return Ok(DestPlan::SingleFile {
                        root: parent,
                        file_name,
                    });
                }
                bail!(
                    "{instruction} cannot copy directory {} over existing file {dest}",
                    source.path.display()
                );
            }
            bail!(
                "{instruction} destination {dest} exists as a file; copying multiple sources \
                 requires a directory destination"
            );
        }
        GuestPathKind::Missing => match sources {
            [source] if !source.is_dir => Ok(DestPlan::SingleFile {
                root: parent,
                file_name,
            }),
            [_] => Ok(DestPlan::Directory {
                root: dest.to_string(),
            }),
            _ => bail!(
                "{instruction} with multiple sources requires the destination to be an \
                 existing directory or to end with '/': {dest}"
            ),
        },
    }
}

fn split_guest_path(path: &str) -> Result<(String, String)> {
    let trimmed = path.trim_end_matches('/');
    let Some((parent, name)) = trimmed.rsplit_once('/') else {
        bail!("destination must be absolute: {path}");
    };
    if name.is_empty() {
        bail!("destination has no file name: {path}");
    }
    let parent = if parent.is_empty() {
        "/".to_string()
    } else {
        parent.to_string()
    };
    Ok((parent, name.to_string()))
}

// ---------------------------------------------------------------------------
// Tar packaging
// ---------------------------------------------------------------------------

/// One tar entry: a local path mapped to its guest path relative to the
/// extraction root.
#[derive(Debug)]
pub(crate) struct TarEntryPlan {
    pub local: ContextPath,
    pub guest_relative: PathBuf,
    pub is_dir: bool,
}

/// Maps the selected sources onto tar entries relative to the destination
/// plan's extraction root. Later sources overwrite earlier ones on
/// extraction, matching Docker's in-order copy semantics.
pub(crate) fn transfer_entries(
    context: &BuildContext,
    sources: &[SelectedSource],
    plan: &DestPlan,
) -> Result<Vec<TarEntryPlan>> {
    let mut entries = Vec::new();
    match plan {
        DestPlan::SingleFile { file_name, .. } => {
            let [source] = sources else {
                bail!("single-file destinations take exactly one source");
            };
            entries.push(TarEntryPlan {
                local: source.path.clone(),
                guest_relative: PathBuf::from(file_name),
                is_dir: false,
            });
        }
        DestPlan::Directory { .. } => {
            for source in sources {
                if source.is_dir {
                    for (local, below, is_dir) in context.directory_contents(source)? {
                        entries.push(TarEntryPlan {
                            local,
                            guest_relative: below,
                            is_dir,
                        });
                    }
                } else {
                    entries.push(TarEntryPlan {
                        local: source.path.clone(),
                        guest_relative: PathBuf::from(source.base_name()?),
                        is_dir: false,
                    });
                }
            }
        }
    }
    for entry in &entries {
        validate_tar_relative_path(&entry.guest_relative)?;
    }
    Ok(entries)
}

fn validate_tar_relative_path(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty() {
        bail!("tar entries cannot have an empty path");
    }
    validate_context_relative_path(path).with_context(|| {
        format!(
            "tar entries must use portable relative paths: {}",
            path.display()
        )
    })
}

/// Packages the planned entries into an uncompressed tar stream. Entries are
/// written root-owned (uid/gid 0) with their local permission bits and
/// mtimes, matching Docker's `--chown`-less COPY behavior once extracted as
/// root inside the guest.
pub(crate) fn pack_transfer(entries: &[TarEntryPlan], out: std::fs::File) -> Result<u64> {
    pack_transfer_impl(entries, out, |_| Ok(()))
}

fn pack_transfer_impl<F>(
    entries: &[TarEntryPlan],
    out: std::fs::File,
    mut after_open: F,
) -> Result<u64>
where
    F: FnMut(&TarEntryPlan) -> Result<()>,
{
    let mut builder = tar::Builder::new(out);
    for entry in entries {
        let opened = entry.local.open_node().with_context(|| {
            format!(
                "opening {} for packaging without following symbolic links or reparse points",
                entry.local.display()
            )
        })?;
        let metadata = opened
            .metadata()
            .with_context(|| format!("reading {}", entry.local.display()))?;
        if opened.is_dir() != entry.is_dir {
            bail!(
                "context entry changed while packaging: {}",
                entry.local.display()
            );
        }
        after_open(entry)?;

        let mut header = tar::Header::new_gnu();
        header.set_uid(0);
        header.set_gid(0);
        header.set_mode(unix_mode(&metadata));
        header.set_mtime(
            metadata
                .modified()
                .ok()
                .and_then(|time| time.into_std().duration_since(std::time::UNIX_EPOCH).ok())
                .map(|duration| duration.as_secs())
                .unwrap_or(0),
        );
        match opened {
            OpenedNode::Directory(_) => {
                header.set_entry_type(tar::EntryType::Directory);
                header.set_size(0);
                builder
                    .append_data(&mut header, &entry.guest_relative, std::io::empty())
                    .with_context(|| format!("packaging {}", entry.local.display()))?;
            }
            OpenedNode::File(file) => {
                header.set_entry_type(tar::EntryType::Regular);
                header.set_size(metadata.len());
                builder
                    .append_data(
                        &mut header,
                        &entry.guest_relative,
                        SizedReader::new(file, metadata.len()),
                    )
                    .with_context(|| format!("packaging {}", entry.local.display()))?;
            }
        }
    }
    let out = builder
        .into_inner()
        .context("finalizing build context archive")?;
    let size = out
        .metadata()
        .context("reading build context archive size")?
        .len();
    Ok(size)
}

/// Guards against files growing while they are packaged: the tar header has
/// already committed to a size, so exactly that many bytes must be written.
struct SizedReader<R> {
    inner: R,
    remaining: u64,
}

impl<R> SizedReader<R> {
    fn new(inner: R, len: u64) -> Self {
        Self {
            inner,
            remaining: len,
        }
    }
}

impl<R: Read> Read for SizedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.remaining == 0 {
            return Ok(0);
        }
        let cap = buf.len().min(self.remaining as usize);
        let read = self.inner.read(&mut buf[..cap])?;
        if read == 0 && self.remaining > 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "file shrank while it was being packaged",
            ));
        }
        self.remaining -= read as u64;
        Ok(read)
    }
}

#[cfg(unix)]
fn unix_mode(metadata: &CapabilityMetadata) -> u32 {
    metadata.mode() & 0o7777
}

#[cfg(not(unix))]
fn unix_mode(_metadata: &CapabilityMetadata) -> u32 {
    0o644
}

/// Sniffs whether a local file is an archive Docker's ADD would
/// auto-extract: gzip/bzip2/xz/zstd compressed streams or an uncompressed
/// ustar tar.
pub(crate) fn looks_like_add_archive(path: &ContextPath) -> Result<bool> {
    let OpenedNode::File(mut file) = path.open_node().with_context(|| {
        format!(
            "opening {} without following symbolic links or reparse points",
            path.display()
        )
    })?
    else {
        bail!("ADD source is no longer a regular file: {}", path.display());
    };
    let mut header = [0_u8; 512];
    let mut filled = 0;
    while filled < header.len() {
        let read = file
            .read(&mut header[filled..])
            .with_context(|| format!("reading {}", path.display()))?;
        if read == 0 {
            break;
        }
        filled += read;
    }
    let header = &header[..filled];
    let compressed = header.starts_with(&[0x1f, 0x8b])
        || header.starts_with(b"BZh")
        || header.starts_with(&[0xfd, 0x37, 0x7a, 0x58, 0x5a, 0x00])
        || header.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]);
    let ustar = filled >= 262 && &header[257..262] == b"ustar";
    Ok(compressed || ustar)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn context_with(temp: &TempDir, dockerignore: Option<&str>) -> BuildContext {
        if let Some(text) = dockerignore {
            fs::write(temp.path().join(".dockerignore"), text).unwrap();
        }
        BuildContext::load(temp.path()).unwrap()
    }

    fn touch(temp: &TempDir, path: &str) {
        let full = temp.path().join(path);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, path).unwrap();
    }

    fn relatives(sources: &[SelectedSource]) -> Vec<String> {
        sources
            .iter()
            .map(|source| source.relative.to_string_lossy().into_owned())
            .collect()
    }

    fn source_for_destination_test(name: &str, is_dir: bool) -> SelectedSource {
        let root = Arc::new(ContextRoot::open(Path::new(".")).unwrap());
        let relative = PathBuf::from(name);
        SelectedSource {
            path: ContextPath::new(root, relative.clone()).unwrap(),
            relative,
            is_dir,
        }
    }

    fn packed_member(entries: &[TarEntryPlan], member: &str) -> Vec<u8> {
        let archive_file = tempfile::NamedTempFile::new().unwrap();
        pack_transfer(entries, archive_file.reopen().unwrap()).unwrap();
        let mut archive = tar::Archive::new(archive_file.reopen().unwrap());
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            if entry.path().unwrap() == Path::new(member) {
                let mut bytes = Vec::new();
                entry.read_to_end(&mut bytes).unwrap();
                return bytes;
            }
        }
        panic!("tar member {member:?} was not found");
    }

    #[test]
    fn ignore_patterns_are_root_anchored() {
        let rules = IgnoreRules::parse("node_modules\n*.md\n").unwrap();
        assert!(rules.is_ignored(Path::new("node_modules"), true));
        assert!(rules.is_ignored(Path::new("node_modules/pkg/index.js"), false));
        assert!(rules.is_ignored(Path::new("README.md"), false));
        // Unlike gitignore, patterns do not float: nested paths that only
        // match at a deeper level stay included.
        assert!(!rules.is_ignored(Path::new("src/node_modules"), true));
        assert!(!rules.is_ignored(Path::new("docs/README.md"), false));
    }

    #[test]
    fn ignore_double_star_crosses_directories() {
        let rules = IgnoreRules::parse("**/temp?\n**/*.log\n").unwrap();
        assert!(rules.is_ignored(Path::new("temp1"), false));
        assert!(rules.is_ignored(Path::new("a/b/temp2"), false));
        assert!(rules.is_ignored(Path::new("deep/build.log"), false));
        assert!(!rules.is_ignored(Path::new("temp12"), false));
    }

    #[test]
    fn ignore_negation_reincludes_last_match_wins() {
        let rules = IgnoreRules::parse("*.md\n!README.md\n").unwrap();
        assert!(rules.is_ignored(Path::new("notes.md"), false));
        assert!(!rules.is_ignored(Path::new("README.md"), false));

        let rules = IgnoreRules::parse("!README.md\n*.md\n").unwrap();
        assert!(rules.is_ignored(Path::new("README.md"), false));
    }

    #[test]
    fn ignore_negation_reincludes_children_of_excluded_dir() {
        let rules = IgnoreRules::parse("build\n!build/keep.txt\n").unwrap();
        assert!(rules.is_ignored(Path::new("build"), true));
        assert!(rules.is_ignored(Path::new("build/out.bin"), false));
        assert!(!rules.is_ignored(Path::new("build/keep.txt"), false));
    }

    #[test]
    fn ignore_comments_and_blank_lines_are_skipped() {
        let rules = IgnoreRules::parse("# comment\n\n  \ntarget\n").unwrap();
        assert!(rules.is_ignored(Path::new("target"), true));
        assert!(!rules.is_ignored(Path::new("comment"), false));
    }

    #[test]
    fn ignore_trailing_and_leading_slashes_normalize() {
        let rules = IgnoreRules::parse("/dist/\n").unwrap();
        assert!(rules.is_ignored(Path::new("dist"), true));
        assert!(rules.is_ignored(Path::new("dist/app.js"), false));
    }

    #[test]
    fn ignore_character_classes_follow_go_match() {
        let rules = IgnoreRules::parse("file[0-9].txt\nother[^a].txt\n").unwrap();
        assert!(rules.is_ignored(Path::new("file1.txt"), false));
        assert!(!rules.is_ignored(Path::new("filex.txt"), false));
        assert!(rules.is_ignored(Path::new("otherb.txt"), false));
        assert!(!rules.is_ignored(Path::new("othera.txt"), false));
    }

    #[test]
    fn ignore_unterminated_class_is_rejected() {
        assert!(IgnoreRules::parse("bad[pattern\n").is_err());
    }

    #[test]
    fn select_exact_file_and_directory() {
        let temp = TempDir::new().unwrap();
        touch(&temp, "app/main.py");
        touch(&temp, "config.json");
        let context = context_with(&temp, None);

        let sources = context
            .select_sources(&["config.json".into(), "app".into()])
            .unwrap();
        assert_eq!(relatives(&sources), ["config.json", "app"]);
        assert!(!sources[0].is_dir);
        assert!(sources[1].is_dir);
    }

    #[test]
    fn select_dot_source_is_context_root() {
        let temp = TempDir::new().unwrap();
        touch(&temp, "file.txt");
        let context = context_with(&temp, None);

        let sources = context.select_sources(&[".".into()]).unwrap();
        assert_eq!(sources.len(), 1);
        assert!(sources[0].is_dir);
        assert_eq!(sources[0].path.display_path(), context.root());
    }

    #[test]
    fn select_absolute_source_is_context_relative() {
        let temp = TempDir::new().unwrap();
        touch(&temp, "etc/config");
        let context = context_with(&temp, None);

        let sources = context.select_sources(&["/etc/config".into()]).unwrap();
        assert_eq!(relatives(&sources), ["etc/config"]);
    }

    #[test]
    fn select_rejects_context_escape() {
        let temp = TempDir::new().unwrap();
        let context = context_with(&temp, None);

        let err = context
            .select_sources(&["../secrets".into()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("escapes the build context"), "{err}");

        let err = context
            .select_sources(&["a/../../b".into()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("escapes the build context"), "{err}");
    }

    #[test]
    fn source_paths_reject_windows_escape_forms_before_access() {
        for pattern in [
            r"..\secrets",
            r"safe\..\secrets",
            r"C:\secrets",
            r"\\server\share",
            "C:secrets",
            "file:stream",
            "//server/share",
        ] {
            let err = normalize_context_pattern(pattern).unwrap_err().to_string();
            assert!(
                err.contains("backslash") || err.contains("prefix") || err.contains("invalid"),
                "{pattern:?}: {err}"
            );
        }
    }

    #[test]
    fn context_relative_paths_reject_roots_parents_prefixes_and_backslashes() {
        for path in [
            Path::new("/absolute"),
            Path::new("../escape"),
            Path::new("safe/../escape"),
            Path::new("C:escape"),
            Path::new("file:stream"),
            Path::new(r"safe\escape"),
            Path::new(r"\\server\share"),
        ] {
            assert!(
                validate_context_relative_path(path).is_err(),
                "{}",
                path.display()
            );
        }
        assert!(validate_context_relative_path(Path::new("safe/nested/file")).is_ok());
    }

    #[test]
    fn select_missing_source_reports_not_found() {
        let temp = TempDir::new().unwrap();
        let context = context_with(&temp, None);

        let err = context
            .select_sources(&["missing.txt".into()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("not found"), "{err}");
    }

    #[test]
    fn select_ignored_source_is_reported() {
        let temp = TempDir::new().unwrap();
        touch(&temp, "secret.env");
        let context = context_with(&temp, Some("*.env\n"));

        let err = context
            .select_sources(&["secret.env".into()])
            .unwrap_err()
            .to_string();
        assert!(err.contains(".dockerignore"), "{err}");
    }

    #[test]
    fn select_glob_expands_and_filters_ignored() {
        let temp = TempDir::new().unwrap();
        touch(&temp, "a.txt");
        touch(&temp, "b.txt");
        touch(&temp, "ignored.txt");
        touch(&temp, "c.rs");
        let context = context_with(&temp, Some("ignored.txt\n"));

        let sources = context.select_sources(&["*.txt".into()]).unwrap();
        assert_eq!(relatives(&sources), ["a.txt", "b.txt"]);
    }

    #[test]
    fn select_glob_without_matches_errors() {
        let temp = TempDir::new().unwrap();
        touch(&temp, "a.txt");
        let context = context_with(&temp, None);

        let err = context
            .select_sources(&["*.rs".into()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("matched no files"), "{err}");
    }

    #[test]
    fn select_glob_handles_an_empty_context() {
        let temp = TempDir::new().unwrap();
        let context = context_with(&temp, None);

        let err = context
            .select_sources(&["*".into()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("matched no files"), "{err}");
    }

    #[test]
    fn select_glob_keeps_only_outermost_directory_matches() {
        let temp = TempDir::new().unwrap();
        touch(&temp, "pkg/sub/file.txt");
        let context = context_with(&temp, None);

        // "pkg*" style globs match the directory; nested content must not be
        // selected twice.
        let sources = context.select_sources(&["pkg*".into()]).unwrap();
        assert_eq!(relatives(&sources), ["pkg"]);
    }

    #[cfg(unix)]
    #[test]
    fn select_rejects_symlink_sources_and_walked_symlinks() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().unwrap();
        touch(&temp, "real.txt");
        symlink("real.txt", temp.path().join("link.txt")).unwrap();
        let context = context_with(&temp, None);

        let err = context
            .select_sources(&["link.txt".into()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("symbolic links"), "{err}");

        let err = context.select_sources(&[".".into()]).map(|sources| {
            transfer_entries(
                &context,
                &sources,
                &DestPlan::Directory {
                    root: "/app".into(),
                },
            )
        });
        let err = err.unwrap().unwrap_err().to_string();
        assert!(err.contains("symbolic links"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn select_exact_rejects_symlink_ancestor() {
        use std::os::unix::fs::symlink;

        let parent = TempDir::new().unwrap();
        let context_dir = parent.path().join("context");
        let outside = parent.path().join("outside");
        fs::create_dir_all(&context_dir).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("secret.txt"), b"host secret").unwrap();
        symlink(&outside, context_dir.join("link")).unwrap();
        let context = BuildContext::load(&context_dir).unwrap();

        let err = context
            .select_sources(&["link/secret.txt".into()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("symbolic links"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn context_root_handle_survives_path_replacement() {
        use std::os::unix::fs::symlink;

        let parent = TempDir::new().unwrap();
        let context_dir = parent.path().join("context");
        let original_dir = parent.path().join("original-context");
        let outside = parent.path().join("outside");
        fs::create_dir_all(&context_dir).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(context_dir.join("payload.txt"), b"original").unwrap();
        fs::write(outside.join("payload.txt"), b"hostile!").unwrap();
        let context = BuildContext::load(&context_dir).unwrap();

        fs::rename(&context_dir, &original_dir).unwrap();
        symlink(&outside, &context_dir).unwrap();

        let sources = context.select_sources(&["payload.txt".into()]).unwrap();
        let entries = transfer_entries(
            &context,
            &sources,
            &DestPlan::Directory {
                root: "/app".into(),
            },
        )
        .unwrap();
        assert_eq!(packed_member(&entries, "payload.txt"), b"original");
    }

    #[cfg(unix)]
    #[test]
    fn select_rejects_special_files() {
        use std::process::Command;

        let temp = TempDir::new().unwrap();
        let status = Command::new("mkfifo")
            .arg(temp.path().join("fifo"))
            .status()
            .unwrap();
        assert!(status.success());
        let context = context_with(&temp, None);

        let err = context
            .select_sources(&["fifo".into()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("special files"), "{err}");
    }

    #[test]
    fn resolve_guest_dest_forms() {
        assert_eq!(
            resolve_guest_dest("/app/", "/").unwrap(),
            ("/app".to_string(), true)
        );
        assert_eq!(
            resolve_guest_dest("/app/file.txt", "/").unwrap(),
            ("/app/file.txt".to_string(), false)
        );
        assert_eq!(
            resolve_guest_dest("relative.txt", "/work").unwrap(),
            ("/work/relative.txt".to_string(), false)
        );
        assert_eq!(
            resolve_guest_dest(".", "/work").unwrap(),
            ("/work".to_string(), true)
        );
        assert_eq!(
            resolve_guest_dest("sub/", "/work").unwrap(),
            ("/work/sub".to_string(), true)
        );
        assert_eq!(
            resolve_guest_dest("/a/../b", "/").unwrap(),
            ("/b".to_string(), false)
        );
        assert_eq!(
            resolve_guest_dest("/../x", "/").unwrap(),
            ("/x".to_string(), false)
        );
    }

    fn file_source(name: &str) -> SelectedSource {
        source_for_destination_test(name, false)
    }

    fn dir_source(name: &str) -> SelectedSource {
        source_for_destination_test(name, true)
    }

    #[test]
    fn plan_destination_existing_directory_copies_into() {
        let plan = plan_destination(
            "COPY",
            &[file_source("a.txt")],
            "/existing",
            false,
            GuestPathKind::Directory,
        )
        .unwrap();
        assert_eq!(
            plan,
            DestPlan::Directory {
                root: "/existing".into()
            }
        );
    }

    #[test]
    fn plan_destination_file_or_missing_is_file_target() {
        for stat in [GuestPathKind::File, GuestPathKind::Missing] {
            let plan = plan_destination(
                "COPY",
                &[file_source("a.txt")],
                "/app/renamed.txt",
                false,
                stat,
            )
            .unwrap();
            assert_eq!(
                plan,
                DestPlan::SingleFile {
                    root: "/app".into(),
                    file_name: "renamed.txt".into()
                }
            );
        }
    }

    #[test]
    fn plan_destination_single_dir_to_missing_creates_directory() {
        let plan = plan_destination(
            "COPY",
            &[dir_source("src")],
            "/newdir",
            false,
            GuestPathKind::Missing,
        )
        .unwrap();
        assert_eq!(
            plan,
            DestPlan::Directory {
                root: "/newdir".into()
            }
        );
    }

    #[test]
    fn plan_destination_dir_over_file_is_rejected() {
        let err = plan_destination(
            "COPY",
            &[dir_source("src")],
            "/existing-file",
            false,
            GuestPathKind::File,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("existing file"), "{err}");
    }

    #[test]
    fn plan_destination_multiple_sources_need_directory() {
        let sources = [file_source("a.txt"), file_source("b.txt")];
        let err = plan_destination(
            "COPY",
            &sources,
            "/app/target",
            false,
            GuestPathKind::Missing,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("end with '/'"), "{err}");

        let err = plan_destination("COPY", &sources, "/app/file", false, GuestPathKind::File)
            .unwrap_err()
            .to_string();
        assert!(err.contains("exists as a file"), "{err}");

        // Existing directory (stat-resolved) accepts multiple sources.
        assert!(matches!(
            plan_destination("COPY", &sources, "/app", false, GuestPathKind::Directory),
            Ok(DestPlan::Directory { .. })
        ));
    }

    #[test]
    fn transfer_entries_directory_sources_contribute_contents() {
        let temp = TempDir::new().unwrap();
        touch(&temp, "src/lib/util.py");
        touch(&temp, "src/main.py");
        touch(&temp, "single.txt");
        let context = context_with(&temp, None);

        let sources = context
            .select_sources(&["src".into(), "single.txt".into()])
            .unwrap();
        let entries = transfer_entries(
            &context,
            &sources,
            &DestPlan::Directory {
                root: "/app".into(),
            },
        )
        .unwrap();

        let guest: Vec<String> = entries
            .iter()
            .map(|entry| entry.guest_relative.to_string_lossy().into_owned())
            .collect();
        assert_eq!(guest, ["lib", "lib/util.py", "main.py", "single.txt"]);
    }

    #[test]
    fn transfer_entries_single_file_renames_to_dest() {
        let temp = TempDir::new().unwrap();
        touch(&temp, "config.json");
        let context = context_with(&temp, None);
        let sources = context.select_sources(&["config.json".into()]).unwrap();

        let entries = transfer_entries(
            &context,
            &sources,
            &DestPlan::SingleFile {
                root: "/etc/app".into(),
                file_name: "settings.json".into(),
            },
        )
        .unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].guest_relative, PathBuf::from("settings.json"));
    }

    #[test]
    fn transfer_entries_ignore_applies_inside_directories() {
        let temp = TempDir::new().unwrap();
        touch(&temp, "app/keep.py");
        touch(&temp, "app/skip.pyc");
        let context = context_with(&temp, Some("**/*.pyc\n"));
        let sources = context.select_sources(&["app".into()]).unwrap();

        let entries = transfer_entries(
            &context,
            &sources,
            &DestPlan::Directory {
                root: "/app".into(),
            },
        )
        .unwrap();
        let guest: Vec<String> = entries
            .iter()
            .map(|entry| entry.guest_relative.to_string_lossy().into_owned())
            .collect();
        assert_eq!(guest, ["keep.py"]);
    }

    #[cfg(unix)]
    #[test]
    fn transfer_rejects_selected_directory_replaced_by_symlink() {
        use std::os::unix::fs::symlink;

        let parent = TempDir::new().unwrap();
        let context_dir = parent.path().join("context");
        let outside = parent.path().join("outside");
        fs::create_dir_all(context_dir.join("src")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(context_dir.join("src/original.txt"), b"original").unwrap();
        fs::write(outside.join("host.txt"), b"host").unwrap();
        let context = BuildContext::load(&context_dir).unwrap();
        let sources = context.select_sources(&["src".into()]).unwrap();

        fs::rename(context_dir.join("src"), context_dir.join("src-original")).unwrap();
        symlink(&outside, context_dir.join("src")).unwrap();

        let err = transfer_entries(
            &context,
            &sources,
            &DestPlan::Directory {
                root: "/app".into(),
            },
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("symbolic links"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn traversal_uses_open_directory_handle_after_path_swap() {
        use std::os::unix::fs::symlink;

        let parent = TempDir::new().unwrap();
        let context_dir = parent.path().join("context");
        let outside = parent.path().join("outside");
        fs::create_dir_all(context_dir.join("src/nested")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(context_dir.join("src/nested/original.txt"), b"original").unwrap();
        fs::write(outside.join("host.txt"), b"host").unwrap();
        let context = BuildContext::load(&context_dir).unwrap();
        let source = context.select_sources(&["src".into()]).unwrap().remove(0);
        let mut swapped = false;

        let walked = context
            .walk_filtered_impl(&source.path, |path, is_dir| {
                if !swapped && is_dir && path.relative == Path::new("src/nested") {
                    fs::rename(
                        context_dir.join("src/nested"),
                        context_dir.join("src/nested-original"),
                    )?;
                    symlink(&outside, context_dir.join("src/nested"))?;
                    swapped = true;
                }
                Ok(())
            })
            .unwrap();

        assert!(swapped);
        assert!(relatives(&walked).contains(&"src/nested/original.txt".to_string()));
        assert!(!relatives(&walked).contains(&"src/nested/host.txt".to_string()));
    }

    #[test]
    fn pack_transfer_writes_root_owned_entries() {
        let temp = TempDir::new().unwrap();
        touch(&temp, "dir/file.txt");
        let context = context_with(&temp, None);
        let sources = context.select_sources(&["dir".into()]).unwrap();
        let entries = transfer_entries(
            &context,
            &sources,
            &DestPlan::Directory {
                root: "/app".into(),
            },
        )
        .unwrap();

        let out_path = temp.path().join("out.tar");
        let out = fs::File::create(&out_path).unwrap();
        let size = pack_transfer(&entries, out).unwrap();
        assert!(size > 0);

        let mut archive = tar::Archive::new(fs::File::open(&out_path).unwrap());
        let mut seen = Vec::new();
        for entry in archive.entries().unwrap() {
            let entry = entry.unwrap();
            let header = entry.header();
            assert_eq!(header.uid().unwrap(), 0);
            assert_eq!(header.gid().unwrap(), 0);
            seen.push(entry.path().unwrap().to_string_lossy().into_owned());
        }
        assert_eq!(seen, ["file.txt"]);
    }

    #[cfg(unix)]
    #[test]
    fn pack_rejects_file_replaced_by_symlink_before_open() {
        use std::os::unix::fs::symlink;

        let parent = TempDir::new().unwrap();
        let context_dir = parent.path().join("context");
        let outside = parent.path().join("outside");
        fs::create_dir_all(&context_dir).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(context_dir.join("payload.txt"), b"original").unwrap();
        fs::write(outside.join("payload.txt"), b"hostile!").unwrap();
        let context = BuildContext::load(&context_dir).unwrap();
        let sources = context.select_sources(&["payload.txt".into()]).unwrap();
        let entries = transfer_entries(
            &context,
            &sources,
            &DestPlan::Directory {
                root: "/app".into(),
            },
        )
        .unwrap();

        fs::rename(
            context_dir.join("payload.txt"),
            context_dir.join("payload-original.txt"),
        )
        .unwrap();
        symlink(outside.join("payload.txt"), context_dir.join("payload.txt")).unwrap();

        let out = tempfile::NamedTempFile::new().unwrap();
        let err = pack_transfer(&entries, out.reopen().unwrap())
            .unwrap_err()
            .to_string();
        assert!(err.contains("symbolic links"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn pack_reads_open_file_handle_after_path_swap() {
        use std::os::unix::fs::symlink;

        let parent = TempDir::new().unwrap();
        let context_dir = parent.path().join("context");
        let outside = parent.path().join("outside");
        fs::create_dir_all(&context_dir).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(context_dir.join("payload.txt"), b"original").unwrap();
        fs::write(outside.join("payload.txt"), b"hostile!").unwrap();
        let context = BuildContext::load(&context_dir).unwrap();
        let sources = context.select_sources(&["payload.txt".into()]).unwrap();
        let entries = transfer_entries(
            &context,
            &sources,
            &DestPlan::Directory {
                root: "/app".into(),
            },
        )
        .unwrap();
        let archive_file = tempfile::NamedTempFile::new().unwrap();
        let mut swapped = false;

        pack_transfer_impl(&entries, archive_file.reopen().unwrap(), |entry| {
            if !swapped {
                fs::rename(
                    entry.local.display_path(),
                    context_dir.join("payload-original.txt"),
                )?;
                symlink(outside.join("payload.txt"), entry.local.display_path())?;
                swapped = true;
            }
            Ok(())
        })
        .unwrap();

        assert!(swapped);
        let mut archive = tar::Archive::new(archive_file.reopen().unwrap());
        let mut entry = archive.entries().unwrap().next().unwrap().unwrap();
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"original");
    }

    #[test]
    fn add_archive_sniffing_detects_tars_and_compression() {
        let temp = TempDir::new().unwrap();

        let plain = temp.path().join("plain.txt");
        fs::write(&plain, "not an archive").unwrap();

        let gzip = temp.path().join("data.gz");
        fs::write(&gzip, [0x1f, 0x8b, 0x08, 0x00]).unwrap();

        let zstd = temp.path().join("data.zst");
        fs::write(&zstd, [0x28, 0xb5, 0x2f, 0xfd, 0x00]).unwrap();

        // Build a real ustar archive and confirm detection.
        let tar_path = temp.path().join("real.tar");
        {
            let mut builder = tar::Builder::new(fs::File::create(&tar_path).unwrap());
            let mut header = tar::Header::new_gnu();
            header.set_size(4);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, "member", &b"data"[..])
                .unwrap();
            builder.finish().unwrap();
        }

        let context = context_with(&temp, None);
        let sources = context
            .select_sources(&[
                "plain.txt".into(),
                "data.gz".into(),
                "data.zst".into(),
                "real.tar".into(),
            ])
            .unwrap();
        assert!(!looks_like_add_archive(&sources[0].path).unwrap());
        assert!(looks_like_add_archive(&sources[1].path).unwrap());
        assert!(looks_like_add_archive(&sources[2].path).unwrap());
        assert!(looks_like_add_archive(&sources[3].path).unwrap());
    }
}
