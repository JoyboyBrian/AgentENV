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
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use walkdir::WalkDir;

const DOCKERIGNORE_FILE: &str = ".dockerignore";

pub(crate) struct BuildContext {
    root: PathBuf,
    ignore: IgnoreRules,
}

impl BuildContext {
    pub(crate) fn load(root: &Path) -> Result<Self> {
        let metadata = std::fs::metadata(root)
            .with_context(|| format!("reading build context {}", root.display()))?;
        if !metadata.is_dir() {
            bail!(
                "build context must be a directory: {} (pass the Dockerfile with -f and the \
                 context directory as the positional argument)",
                root.display()
            );
        }
        let root = root
            .canonicalize()
            .with_context(|| format!("resolving build context {}", root.display()))?;
        let ignore = match std::fs::read_to_string(root.join(DOCKERIGNORE_FILE)) {
            Ok(text) => IgnoreRules::parse(&text)
                .with_context(|| format!("parsing {}", root.join(DOCKERIGNORE_FILE).display()))?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => IgnoreRules::default(),
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("reading {}", root.join(DOCKERIGNORE_FILE).display()))
            }
        };
        Ok(Self { root, ignore })
    }

    #[cfg(test)]
    pub(crate) fn root(&self) -> &Path {
        &self.root
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
        let relative = PathBuf::from(normalized);
        let path = if normalized == "." {
            self.root.clone()
        } else {
            self.root.join(&relative)
        };
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                bail!("source {original:?} was not found in the build context");
            }
            Err(err) => {
                return Err(err).with_context(|| format!("reading source {original:?}"));
            }
        };
        if normalized != "." && self.ignore.is_ignored(&relative, metadata.is_dir()) {
            bail!("source {original:?} is excluded by {DOCKERIGNORE_FILE}");
        }
        validate_source_file_type(&path, &metadata)?;
        Ok(SelectedSource {
            path,
            relative,
            is_dir: metadata.is_dir(),
        })
    }

    fn select_glob(&self, pattern: &str) -> Result<Vec<SelectedSource>> {
        let segments = pattern_segments(pattern)?;
        let mut matches = Vec::new();
        for entry in self.walk_filtered(&self.root)? {
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
    fn walk_filtered(&self, dir: &Path) -> Result<Vec<SelectedSource>> {
        let mut entries = Vec::new();
        let mut walker = WalkDir::new(dir)
            .follow_links(false)
            .min_depth(1)
            .sort_by_file_name()
            .into_iter();
        while let Some(entry) = walker.next() {
            let entry = entry.with_context(|| format!("walking {}", dir.display()))?;
            let file_type = entry.file_type();
            let relative = entry
                .path()
                .strip_prefix(&self.root)
                .expect("walked entries stay under the context root")
                .to_path_buf();
            for component in relative.components() {
                let Component::Normal(part) = component else {
                    bail!("invalid context path: {}", relative.display());
                };
                if part.to_str().is_none() {
                    bail!(
                        "context entries must have valid UTF-8 names: {}",
                        entry.path().display()
                    );
                }
            }
            if self.ignore.is_ignored(&relative, file_type.is_dir()) {
                // A fully ignored directory can be pruned unless a negation
                // pattern could re-include something beneath it.
                if file_type.is_dir() && !self.ignore.has_negations() {
                    walker.skip_current_dir();
                }
                continue;
            }
            if file_type.is_symlink() {
                bail!(
                    "symbolic links are not supported in the build context: {}",
                    entry.path().display()
                );
            }
            if !file_type.is_dir() && !file_type.is_file() {
                bail!(
                    "special files are not supported in the build context: {}",
                    entry.path().display()
                );
            }
            entries.push(SelectedSource {
                path: entry.path().to_path_buf(),
                relative,
                is_dir: file_type.is_dir(),
            });
        }
        Ok(entries)
    }

    /// Walks a directory source purely for validation so symlinks, special
    /// files, and non-UTF-8 names fail before the build VM is created.
    pub(crate) fn validate_directory_source(&self, source: &SelectedSource) -> Result<()> {
        self.walk_filtered(&source.path).map(|_| ())
    }

    /// Expands one selected directory source into the relative file/dir
    /// entries beneath it, honoring `.dockerignore`.
    fn directory_contents(&self, source: &SelectedSource) -> Result<Vec<(PathBuf, PathBuf, bool)>> {
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

fn validate_source_file_type(path: &Path, metadata: &std::fs::Metadata) -> Result<()> {
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        bail!(
            "symbolic links are not supported in the build context: {}",
            path.display()
        );
    }
    if !file_type.is_file() && !file_type.is_dir() {
        bail!(
            "special files are not supported in the build context: {}",
            path.display()
        );
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub(crate) struct SelectedSource {
    /// Absolute local path.
    pub path: PathBuf,
    /// Path relative to the context root ("." source keeps an empty path).
    pub relative: PathBuf,
    pub is_dir: bool,
}

impl SelectedSource {
    fn base_name(&self) -> Result<&str> {
        self.path
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
    Ok(segments.join("/"))
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
#[derive(Debug, PartialEq)]
pub(crate) struct TarEntryPlan {
    pub local: PathBuf,
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
    for component in path.components() {
        match component {
            Component::Normal(part) => {
                if part.to_str().is_none() {
                    bail!(
                        "tar entries must have valid UTF-8 names: {}",
                        path.display()
                    );
                }
            }
            _ => bail!("tar entries must stay relative: {}", path.display()),
        }
    }
    Ok(())
}

/// Packages the planned entries into an uncompressed tar stream. Entries are
/// written root-owned (uid/gid 0) with their local permission bits and
/// mtimes, matching Docker's `--chown`-less COPY behavior once extracted as
/// root inside the guest.
pub(crate) fn pack_transfer(entries: &[TarEntryPlan], out: std::fs::File) -> Result<u64> {
    let mut builder = tar::Builder::new(out);
    for entry in entries {
        let metadata = std::fs::symlink_metadata(&entry.local)
            .with_context(|| format!("reading {}", entry.local.display()))?;
        let file_type = metadata.file_type();
        if file_type.is_symlink() || (!file_type.is_dir() && !file_type.is_file()) {
            bail!(
                "context entry changed while packaging (symlink or special file): {}",
                entry.local.display()
            );
        }
        if file_type.is_dir() != entry.is_dir {
            bail!(
                "context entry changed while packaging: {}",
                entry.local.display()
            );
        }
        let mut header = tar::Header::new_gnu();
        header.set_uid(0);
        header.set_gid(0);
        header.set_mode(unix_mode(&metadata));
        header.set_mtime(
            metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|duration| duration.as_secs())
                .unwrap_or(0),
        );
        if entry.is_dir {
            header.set_entry_type(tar::EntryType::Directory);
            header.set_size(0);
            builder
                .append_data(&mut header, &entry.guest_relative, std::io::empty())
                .with_context(|| format!("packaging {}", entry.local.display()))?;
        } else {
            header.set_entry_type(tar::EntryType::Regular);
            header.set_size(metadata.len());
            let file = std::fs::File::open(&entry.local)
                .with_context(|| format!("opening {}", entry.local.display()))?;
            builder
                .append_data(
                    &mut header,
                    &entry.guest_relative,
                    SizedReader::new(file, metadata.len()),
                )
                .with_context(|| format!("packaging {}", entry.local.display()))?;
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
fn unix_mode(metadata: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::MetadataExt;
    metadata.mode() & 0o7777
}

#[cfg(not(unix))]
fn unix_mode(_metadata: &std::fs::Metadata) -> u32 {
    0o644
}

/// Sniffs whether a local file is an archive Docker's ADD would
/// auto-extract: gzip/bzip2/xz/zstd compressed streams or an uncompressed
/// ustar tar.
pub(crate) fn looks_like_add_archive(path: &Path) -> Result<bool> {
    let mut file =
        std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
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
        assert_eq!(sources[0].path, context.root());
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
    fn select_rejects_special_files() {
        use std::os::unix::net::UnixListener;

        let temp = TempDir::new().unwrap();
        UnixListener::bind(temp.path().join("socket")).unwrap();
        let context = context_with(&temp, None);

        let err = context
            .select_sources(&["socket".into()])
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
        SelectedSource {
            path: PathBuf::from(format!("/ctx/{name}")),
            relative: PathBuf::from(name),
            is_dir: false,
        }
    }

    fn dir_source(name: &str) -> SelectedSource {
        SelectedSource {
            path: PathBuf::from(format!("/ctx/{name}")),
            relative: PathBuf::from(name),
            is_dir: true,
        }
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

    #[test]
    fn add_archive_sniffing_detects_tars_and_compression() {
        let temp = TempDir::new().unwrap();

        let plain = temp.path().join("plain.txt");
        fs::write(&plain, "not an archive").unwrap();
        assert!(!looks_like_add_archive(&plain).unwrap());

        let gzip = temp.path().join("data.gz");
        fs::write(&gzip, [0x1f, 0x8b, 0x08, 0x00]).unwrap();
        assert!(looks_like_add_archive(&gzip).unwrap());

        let zstd = temp.path().join("data.zst");
        fs::write(&zstd, [0x28, 0xb5, 0x2f, 0xfd, 0x00]).unwrap();
        assert!(looks_like_add_archive(&zstd).unwrap());

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
        assert!(looks_like_add_archive(&tar_path).unwrap());
    }
}
