//! Host-side planning for template `COPY` steps.
//!
//! The E2B SDK uploads one tar archive per `COPY` instruction whose entry
//! paths are relative to the build context (the glob in `src` is already
//! resolved by the SDK). This module rewrites that archive so every entry
//! carries its final absolute guest path per Docker `COPY` semantics; the
//! build sandbox then only needs a single `tar -xpf archive -C /`.
//!
//! The rewrite runs in two passes so an archive is never held in memory: the
//! first pass indexes entry paths to compute the mapping, the second streams
//! each entry's bytes straight into the rewritten archive.
//!
//! Ownership is written into the rewritten headers rather than applied with a
//! post-extract `chown`, so a copy can only ever change the files it creates.

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::Path;

use anyhow::{bail, Context, Result};

/// Upper bound on entries in one build-context archive. The indexing pass
/// keeps one normalized path per entry, so this bounds that allocation
/// independently of the byte budget: an archive of a million empty files is
/// tiny but path-heavy. Real build contexts are orders of magnitude smaller.
const MAX_ARCHIVE_ENTRIES: usize = 200_000;

/// Numeric ownership applied to every entry of one copy.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CopyOwnership {
    pub(crate) uid: u64,
    pub(crate) gid: u64,
}

/// Inputs for rewriting one `COPY` step's archive.
pub(crate) struct CopyRequest<'a> {
    pub(crate) source_tar: &'a Path,
    pub(crate) src: &'a str,
    pub(crate) dest: &'a str,
    pub(crate) workdir: &'a str,
    pub(crate) mode: Option<u32>,
    /// Ownership requested by `--chown`, already resolved to numeric ids
    /// inside the build sandbox. `None` keeps Docker's root:root default.
    pub(crate) ownership: Option<CopyOwnership>,
    /// Budget for the decompressed archive, bounding both the rewritten file
    /// on the host and what a single upload can expand to.
    pub(crate) max_total_bytes: u64,
}

/// Summary of a rewritten copy archive.
#[derive(Debug)]
pub(crate) struct CopyPlan {
    /// Number of file/dir/symlink entries written to the rewritten archive.
    pub(crate) entry_count: usize,
    /// Total file bytes written to the rewritten archive.
    pub(crate) total_bytes: u64,
}

/// One archive entry as seen by the indexing pass.
struct EntryIndex {
    /// Normalized context-relative path ("dir/file.txt").
    path: String,
    is_dir: bool,
}

fn is_glob_pattern(src: &str) -> bool {
    src.contains(['*', '?', '['])
}

/// Minimal fnmatch-style matcher covering `*`, `?` and `[...]` (no `**`),
/// mirroring the Python `glob` patterns the SDK resolves client-side.
fn glob_match(pattern: &str, value: &str) -> bool {
    fn inner(pattern: &[char], value: &[char]) -> bool {
        match pattern.split_first() {
            None => value.is_empty(),
            Some(('*', rest)) => (0..=value.len()).any(|skip| inner(rest, &value[skip..])),
            Some(('?', rest)) => !value.is_empty() && inner(rest, &value[1..]),
            Some(('[', rest)) => {
                let Some(end) = rest.iter().position(|&c| c == ']') else {
                    // No closing bracket: treat '[' as a literal character.
                    return !value.is_empty() && value[0] == '[' && inner(rest, &value[1..]);
                };
                let (class, after) = rest.split_at(end);
                let after = &after[1..];
                let Some(&first) = value.first() else {
                    return false;
                };
                let (negated, class) = match class.first() {
                    Some('!') | Some('^') => (true, &class[1..]),
                    _ => (false, class),
                };
                let mut matched = false;
                let mut i = 0;
                while i < class.len() {
                    if i + 2 < class.len() && class[i + 1] == '-' {
                        if class[i] <= first && first <= class[i + 2] {
                            matched = true;
                        }
                        i += 3;
                    } else {
                        if class[i] == first {
                            matched = true;
                        }
                        i += 1;
                    }
                }
                if matched != negated {
                    inner(after, &value[1..])
                } else {
                    false
                }
            }
            Some((&c, rest)) => !value.is_empty() && value[0] == c && inner(rest, &value[1..]),
        }
    }
    let pattern: Vec<char> = pattern.chars().collect();
    let value: Vec<char> = value.chars().collect();
    inner(&pattern, &value)
}

/// Normalizes a context-relative source pattern ("./a/b/" -> "a/b").
fn normalize_src(src: &str) -> String {
    let mut src = src.trim();
    while let Some(stripped) = src.strip_prefix("./") {
        src = stripped;
    }
    src.trim_end_matches('/').to_string()
}

/// Joins `path` onto `base` and lexically normalizes the result into an
/// absolute guest path. Absolute `path` values replace `base` entirely, which
/// is how Docker resolves both `WORKDIR` and `COPY` destinations.
pub(crate) fn resolve_guest_path(base: &str, path: &str) -> Result<String> {
    let joined = if path.starts_with('/') {
        path.to_string()
    } else {
        let base = if base.trim().is_empty() { "/" } else { base };
        if !base.starts_with('/') {
            bail!("cannot resolve '{path}' against non-absolute base '{base}'");
        }
        format!("{}/{}", base.trim_end_matches('/'), path)
    };

    let mut parts: Vec<&str> = Vec::new();
    for part in joined.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.pop().is_none() {
                    bail!("path '{path}' escapes the filesystem root");
                }
            }
            part => parts.push(part),
        }
    }
    Ok(format!("/{}", parts.join("/")))
}

fn join_abs(base: &str, rel: &str) -> String {
    if rel.is_empty() {
        base.to_string()
    } else if base == "/" {
        format!("/{rel}")
    } else {
        format!("{base}/{rel}")
    }
}

fn base_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Normalizes one archive entry path and rejects escapes.
fn normalize_entry_path(raw: &Path) -> Result<String> {
    let mut parts: Vec<String> = Vec::new();
    for component in raw.components() {
        match component {
            std::path::Component::Normal(part) => {
                parts.push(part.to_string_lossy().into_owned());
            }
            std::path::Component::CurDir => {}
            other => bail!(
                "unsupported path component {:?} in archive entry '{}'",
                other,
                raw.display()
            ),
        }
    }
    if parts.is_empty() {
        bail!("empty path in archive entry");
    }
    Ok(parts.join("/"))
}

/// Opens the uploaded archive, transparently decompressing gzip.
fn open_archive(source_tar: &Path) -> Result<tar::Archive<Box<dyn Read>>> {
    let mut file = File::open(source_tar)
        .with_context(|| format!("open build context archive '{}'", source_tar.display()))?;
    let mut magic = [0u8; 2];
    let gzip = match file.read(&mut magic) {
        Ok(2) => magic == [0x1f, 0x8b],
        _ => false,
    };
    file.seek(SeekFrom::Start(0))
        .context("rewind build context archive")?;

    let reader: Box<dyn Read> = if gzip {
        Box::new(flate2::read::GzDecoder::new(BufReader::new(file)))
    } else {
        Box::new(BufReader::new(file))
    };
    Ok(tar::Archive::new(reader))
}

fn check_entry_type(entry_type: tar::EntryType) -> Result<()> {
    match entry_type {
        tar::EntryType::Regular
        | tar::EntryType::Directory
        | tar::EntryType::Symlink
        | tar::EntryType::GNUSparse => Ok(()),
        // Metadata-only companion entries (long names, pax headers) are
        // consumed by the tar crate itself and never surface here.
        other => bail!("unsupported entry type {other:?} in build context archive"),
    }
}

/// First pass: index entry paths and enforce the archive budgets without
/// reading any file contents.
fn read_entry_index(source_tar: &Path, max_total_bytes: u64) -> Result<Vec<EntryIndex>> {
    let mut archive = open_archive(source_tar)?;
    let mut index = Vec::new();
    let mut total_bytes = 0u64;

    for entry in archive
        .entries()
        .context("read build context archive entries")?
    {
        let entry = entry.context("read build context archive entry")?;
        let entry_type = entry.header().entry_type();
        check_entry_type(entry_type)?;

        total_bytes = total_bytes.saturating_add(entry.size());
        if total_bytes > max_total_bytes {
            bail!(
                "build context archive expands beyond the configured limit of \
                 {max_total_bytes} bytes"
            );
        }
        if index.len() >= MAX_ARCHIVE_ENTRIES {
            bail!("build context archive holds more than {MAX_ARCHIVE_ENTRIES} entries");
        }

        index.push(EntryIndex {
            path: normalize_entry_path(&entry.path().context("entry path")?)?,
            is_dir: entry_type == tar::EntryType::Directory,
        });
    }

    if index.is_empty() {
        bail!("build context archive contains no files");
    }
    Ok(index)
}

/// Computes the final absolute guest path for every indexed entry.
///
/// The returned vector is positionally aligned with `index`.
fn map_entries(
    index: &[EntryIndex],
    src: &str,
    dest_raw: &str,
    workdir: &str,
) -> Result<Vec<String>> {
    let src = normalize_src(src);
    let dest_is_dir_hint = dest_raw.ends_with('/')
        || dest_raw.ends_with("/.")
        || dest_raw == "."
        || dest_raw.is_empty();
    let dest = resolve_guest_path(workdir, if dest_raw.is_empty() { "." } else { dest_raw })?;

    let mut mapped = Vec::with_capacity(index.len());

    let copy_whole_context = src.is_empty() || src == ".";
    let single_file_src = !copy_whole_context
        && !is_glob_pattern(&src)
        && index.len() == 1
        && index[0].path == src
        && !index[0].is_dir;

    if single_file_src {
        mapped.push(if dest_is_dir_hint {
            join_abs(&dest, base_name(&src))
        } else {
            dest
        });
        return Ok(mapped);
    }

    if copy_whole_context || !is_glob_pattern(&src) {
        // Directory source: Docker copies the directory *contents* into dest.
        for entry in index {
            let rel = if copy_whole_context {
                entry.path.as_str()
            } else if entry.path == src {
                ""
            } else if let Some(rel) = entry.path.strip_prefix(&format!("{src}/")) {
                rel
            } else {
                bail!(
                    "archive entry '{}' does not belong to COPY source '{}'",
                    entry.path,
                    src
                );
            };
            mapped.push(join_abs(&dest, rel));
        }
        return Ok(mapped);
    }

    // Glob source: every matched top-level item lands inside dest. Matched
    // files keep their base name; matched directories contribute their
    // contents (Docker treats each matched directory like a directory source).
    for entry in index {
        let mut components = entry.path.split('/');
        let mut prefix = String::new();
        let mut matched_root: Option<String> = None;
        for component in components.by_ref() {
            if prefix.is_empty() {
                prefix.push_str(component);
            } else {
                prefix.push('/');
                prefix.push_str(component);
            }
            if glob_match(&src, &prefix) {
                matched_root = Some(prefix.clone());
                break;
            }
        }
        let Some(root) = matched_root else {
            bail!(
                "archive entry '{}' does not match COPY source pattern '{}'",
                entry.path,
                src
            );
        };
        let rel = entry
            .path
            .strip_prefix(&root)
            .map(|rest| rest.trim_start_matches('/'))
            .unwrap_or("");
        mapped.push(if rel.is_empty() && !entry.is_dir {
            join_abs(&dest, base_name(&root))
        } else {
            join_abs(&dest, rel)
        });
    }
    Ok(mapped)
}

/// Rewrites the SDK context archive into `output` with final absolute guest
/// paths, the requested ownership, and the optional mode override applied.
pub(crate) fn plan_copy_archive(request: &CopyRequest<'_>, output: &Path) -> Result<CopyPlan> {
    let index = read_entry_index(request.source_tar, request.max_total_bytes)?;
    let targets = map_entries(&index, request.src, request.dest, request.workdir)?;

    let out_file = File::create(output)
        .with_context(|| format!("create rewritten copy archive '{}'", output.display()))?;
    let mut builder = tar::Builder::new(out_file);
    let mut entry_count = 0usize;
    let mut total_bytes = 0u64;
    let mut seen = 0usize;

    // Second pass: stream each entry's bytes into the rewritten archive.
    let mut archive = open_archive(request.source_tar)?;
    for entry in archive
        .entries()
        .context("read build context archive entries")?
    {
        let mut entry = entry.context("read build context archive entry")?;
        let Some(target) = targets.get(seen) else {
            bail!("build context archive changed while it was being rewritten");
        };
        seen += 1;

        let relative = target.trim_start_matches('/');
        if relative.is_empty() {
            // The destination root itself ("/"); parents always exist.
            continue;
        }

        let entry_type = entry.header().entry_type();
        check_entry_type(entry_type)?;
        let link_name = entry
            .link_name()
            .context("entry link name")?
            .map(|link| link.into_owned());
        let mut header = entry.header().clone();
        let (uid, gid) = request
            .ownership
            .map_or((0, 0), |owner| (owner.uid, owner.gid));
        header.set_uid(uid);
        header.set_gid(gid);
        // Clear the name fields so the numeric ids above are authoritative.
        // GNU tar prefers uname/gname when they resolve in the target image,
        // so leaving the uploader's account names in place could hand files
        // to an unrelated guest account.
        header
            .set_username("")
            .and_then(|()| header.set_groupname(""))
            .with_context(|| format!("clear ownership names on entry '{target}'"))?;
        if let Some(mode) = request.mode {
            header.set_mode(mode);
        }

        match entry_type {
            tar::EntryType::Directory => {
                header.set_size(0);
                builder
                    .append_data(&mut header, format!("{relative}/"), std::io::empty())
                    .with_context(|| format!("write directory entry '{target}'"))?;
            }
            tar::EntryType::Symlink => {
                let link = link_name.context("symlink entry is missing its target")?;
                header.set_size(0);
                builder
                    .append_link(&mut header, relative, &link)
                    .with_context(|| format!("write symlink entry '{target}'"))?;
            }
            _ => {
                let size = entry.size();
                header.set_size(size);
                // A GNU sparse entry is read back expanded, so the rewritten
                // entry is a plain regular file.
                header.set_entry_type(tar::EntryType::Regular);
                builder
                    .append_data(&mut header, relative, &mut entry)
                    .with_context(|| format!("write file entry '{target}'"))?;
                total_bytes += size;
            }
        }
        entry_count += 1;
    }

    if seen != targets.len() {
        bail!("build context archive changed while it was being rewritten");
    }

    let mut out_file = builder.into_inner().context("finish rewritten archive")?;
    out_file.flush().context("flush rewritten archive")?;

    Ok(CopyPlan {
        entry_count,
        total_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use tempfile::TempDir;

    const NO_LIMIT: u64 = u64::MAX;

    fn request<'a>(
        source_tar: &'a Path,
        src: &'a str,
        dest: &'a str,
        workdir: &'a str,
    ) -> CopyRequest<'a> {
        CopyRequest {
            source_tar,
            src,
            dest,
            workdir,
            mode: None,
            ownership: None,
            max_total_bytes: NO_LIMIT,
        }
    }

    fn build_source_tar(dir: &Path, entries: &[(&str, Option<&str>)]) -> std::path::PathBuf {
        // (path, Some(contents)) = file, (path, None) = directory
        let tar_path = dir.join("source.tar");
        let file = File::create(&tar_path).expect("create tar");
        let mut builder = tar::Builder::new(file);
        for (path, contents) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_uid(501);
            header.set_gid(20);
            match contents {
                Some(data) => {
                    header.set_entry_type(tar::EntryType::Regular);
                    header.set_mode(0o644);
                    header.set_size(data.len() as u64);
                    builder
                        .append_data(&mut header, path, data.as_bytes())
                        .expect("append file");
                }
                None => {
                    header.set_entry_type(tar::EntryType::Directory);
                    header.set_mode(0o755);
                    header.set_size(0);
                    builder
                        .append_data(&mut header, format!("{path}/"), std::io::empty())
                        .expect("append dir");
                }
            }
        }
        builder.finish().expect("finish tar");
        tar_path
    }

    struct Rewritten {
        kind: tar::EntryType,
        uid: u64,
        gid: u64,
        mode: u32,
        contents: String,
    }

    fn rewritten_entries(path: &Path) -> BTreeMap<String, Rewritten> {
        let mut archive = tar::Archive::new(File::open(path).expect("open rewritten"));
        let mut out = BTreeMap::new();
        for entry in archive.entries().expect("entries") {
            let mut entry = entry.expect("entry");
            let path = entry.path().expect("path").to_string_lossy().into_owned();
            let kind = entry.header().entry_type();
            let uid = entry.header().uid().expect("uid");
            let gid = entry.header().gid().expect("gid");
            let mode = entry.header().mode().expect("mode");
            let mut contents = String::new();
            entry.read_to_string(&mut contents).expect("read");
            out.insert(
                path,
                Rewritten {
                    kind,
                    uid,
                    gid,
                    mode,
                    contents,
                },
            );
        }
        out
    }

    #[test]
    fn single_file_to_absolute_file_dest() {
        let dir = TempDir::new().expect("tempdir");
        let tar = build_source_tar(dir.path(), &[("hello.txt", Some("hello\n"))]);
        let out = dir.path().join("out.tar");

        let plan =
            plan_copy_archive(&request(&tar, "hello.txt", "/hello.txt", "/"), &out).expect("plan");

        assert_eq!(plan.entry_count, 1);
        assert_eq!(plan.total_bytes, 6);
        let entries = rewritten_entries(&out);
        let entry = &entries["hello.txt"];
        assert_eq!(entry.kind, tar::EntryType::Regular);
        assert_eq!(entry.uid, 0, "ownership must default to root");
        assert_eq!(entry.gid, 0);
        assert_eq!(entry.contents, "hello\n");
    }

    #[test]
    fn single_file_to_directory_dest() {
        let dir = TempDir::new().expect("tempdir");
        let tar = build_source_tar(dir.path(), &[("requirements.txt", Some("e2b\n"))]);
        let out = dir.path().join("out.tar");

        plan_copy_archive(&request(&tar, "requirements.txt", "/home/user/", "/"), &out)
            .expect("plan");

        assert!(rewritten_entries(&out).contains_key("home/user/requirements.txt"));
    }

    #[test]
    fn relative_dest_resolves_against_workdir() {
        let dir = TempDir::new().expect("tempdir");
        let tar = build_source_tar(dir.path(), &[("config.py", Some("x = 1\n"))]);
        let out = dir.path().join("out.tar");

        plan_copy_archive(&request(&tar, "config.py", "conf/app.py", "/srv"), &out).expect("plan");

        assert!(rewritten_entries(&out).contains_key("srv/conf/app.py"));
    }

    #[test]
    fn directory_source_copies_contents_into_dest() {
        let dir = TempDir::new().expect("tempdir");
        let tar = build_source_tar(
            dir.path(),
            &[
                ("app", None),
                ("app/main.py", Some("print()\n")),
                ("app/sub", None),
                ("app/sub/util.py", Some("pass\n")),
            ],
        );
        let out = dir.path().join("out.tar");

        plan_copy_archive(&request(&tar, "app", "/opt/service", "/"), &out).expect("plan");

        let entries = rewritten_entries(&out);
        assert!(entries.contains_key("opt/service/"));
        assert!(entries.contains_key("opt/service/main.py"));
        assert!(entries.contains_key("opt/service/sub/"));
        assert!(entries.contains_key("opt/service/sub/util.py"));
    }

    #[test]
    fn whole_context_source_copies_everything() {
        let dir = TempDir::new().expect("tempdir");
        let tar = build_source_tar(
            dir.path(),
            &[
                ("a.txt", Some("a")),
                ("sub", None),
                ("sub/b.txt", Some("b")),
            ],
        );
        let out = dir.path().join("out.tar");

        plan_copy_archive(&request(&tar, ".", "/workspace", "/"), &out).expect("plan");

        let entries = rewritten_entries(&out);
        assert!(entries.contains_key("workspace/a.txt"));
        assert!(entries.contains_key("workspace/sub/b.txt"));
    }

    #[test]
    fn glob_source_places_matches_by_base_name() {
        let dir = TempDir::new().expect("tempdir");
        let tar = build_source_tar(
            dir.path(),
            &[("one.txt", Some("1")), ("two.txt", Some("2"))],
        );
        let out = dir.path().join("out.tar");

        let plan = plan_copy_archive(&request(&tar, "*.txt", "/data/", "/"), &out).expect("plan");

        assert_eq!(plan.entry_count, 2);
        let entries = rewritten_entries(&out);
        assert!(entries.contains_key("data/one.txt"));
        assert!(entries.contains_key("data/two.txt"));
    }

    #[test]
    fn glob_matching_directory_copies_its_contents() {
        let dir = TempDir::new().expect("tempdir");
        let tar = build_source_tar(
            dir.path(),
            &[
                ("pkg-a", None),
                ("pkg-a/lib.py", Some("a")),
                ("pkg-b", None),
                ("pkg-b/lib.py", Some("b")),
            ],
        );
        let out = dir.path().join("out.tar");

        plan_copy_archive(&request(&tar, "pkg-*", "/opt/pkgs", "/"), &out).expect("plan");

        // Docker merges contents of every matched directory into dest; the
        // second lib.py overwrites the first at extract time.
        assert!(rewritten_entries(&out).contains_key("opt/pkgs/lib.py"));
    }

    #[test]
    fn mode_override_applies_to_entries() {
        let dir = TempDir::new().expect("tempdir");
        let tar = build_source_tar(dir.path(), &[("run.sh", Some("#!/bin/sh\n"))]);
        let out = dir.path().join("out.tar");

        let mut req = request(&tar, "run.sh", "/usr/local/bin/run.sh", "/");
        req.mode = Some(0o755);
        plan_copy_archive(&req, &out).expect("plan");

        assert_eq!(rewritten_entries(&out)["usr/local/bin/run.sh"].mode, 0o755);
    }

    #[test]
    fn ownership_is_written_into_entry_headers() {
        let dir = TempDir::new().expect("tempdir");
        let tar = build_source_tar(
            dir.path(),
            &[("app", None), ("app/main.py", Some("print()\n"))],
        );
        let out = dir.path().join("out.tar");

        let mut req = request(&tar, "app", "/opt/service", "/");
        req.ownership = Some(CopyOwnership {
            uid: 1000,
            gid: 2000,
        });
        plan_copy_archive(&req, &out).expect("plan");

        // Every created entry carries the requested ownership, and nothing
        // outside the archive can be affected.
        for entry in rewritten_entries(&out).values() {
            assert_eq!(entry.uid, 1000);
            assert_eq!(entry.gid, 2000);
        }
    }

    #[test]
    fn gzip_archives_are_accepted() {
        let dir = TempDir::new().expect("tempdir");
        let tar = build_source_tar(dir.path(), &[("hello.txt", Some("hi"))]);
        let gz_path = dir.path().join("source.tar.gz");
        let mut encoder = flate2::write::GzEncoder::new(
            File::create(&gz_path).expect("create gz"),
            flate2::Compression::fast(),
        );
        std::io::copy(&mut File::open(&tar).expect("open tar"), &mut encoder).expect("compress");
        encoder.finish().expect("finish gz");
        let out = dir.path().join("out.tar");

        let plan = plan_copy_archive(&request(&gz_path, "hello.txt", "/hello.txt", "/"), &out)
            .expect("plan");
        assert_eq!(plan.entry_count, 1);
    }

    #[test]
    fn rejects_archives_over_the_decompressed_budget() {
        let dir = TempDir::new().expect("tempdir");
        let tar = build_source_tar(dir.path(), &[("big.txt", Some("0123456789"))]);
        let out = dir.path().join("out.tar");

        let mut req = request(&tar, "big.txt", "/big.txt", "/");
        req.max_total_bytes = 4;
        let err = plan_copy_archive(&req, &out).expect_err("oversized archive must fail");
        assert!(err.to_string().contains("expands beyond"));
    }

    #[test]
    fn rejects_entries_escaping_the_root() {
        let dir = TempDir::new().expect("tempdir");
        let tar_path = dir.path().join("evil.tar");
        let file = File::create(&tar_path).expect("create tar");
        let mut builder = tar::Builder::new(file);
        // `append_data` refuses to write `..` paths, so craft the header
        // manually the way a hostile client would.
        let mut header = tar::Header::new_gnu();
        let evil_path = b"../../etc/passwd";
        header.as_gnu_mut().expect("gnu header").name[..evil_path.len()].copy_from_slice(evil_path);
        header.set_entry_type(tar::EntryType::Regular);
        header.set_mode(0o644);
        header.set_size(4);
        header.set_cksum();
        builder
            .append(&header, "pwn\n".as_bytes())
            .expect("append raw entry");
        builder.finish().expect("finish");
        let out = dir.path().join("out.tar");

        let err = plan_copy_archive(&request(&tar_path, "passwd", "/tmp/x", "/"), &out)
            .expect_err("path escape must fail");
        assert!(err.to_string().contains("unsupported path component"));
    }

    #[test]
    fn rejects_dest_escaping_the_root() {
        let dir = TempDir::new().expect("tempdir");
        let tar = build_source_tar(dir.path(), &[("a.txt", Some("a"))]);
        let out = dir.path().join("out.tar");

        let err = plan_copy_archive(&request(&tar, "a.txt", "../../x", "/"), &out)
            .expect_err("dest escape must fail");
        assert!(err.to_string().contains("escapes the filesystem root"));
    }

    #[test]
    fn rejects_empty_archive() {
        let dir = TempDir::new().expect("tempdir");
        let tar_path = dir.path().join("empty.tar");
        let file = File::create(&tar_path).expect("create tar");
        tar::Builder::new(file).finish().expect("finish");
        let out = dir.path().join("out.tar");

        let err = plan_copy_archive(&request(&tar_path, "x", "/x", "/"), &out)
            .expect_err("empty archive must fail");
        assert!(err.to_string().contains("no files"));
    }

    #[test]
    fn guest_path_resolution_follows_docker_semantics() {
        assert_eq!(
            resolve_guest_path("/srv", "app").expect("relative"),
            "/srv/app"
        );
        assert_eq!(
            resolve_guest_path("/srv/app", "/opt").expect("absolute"),
            "/opt"
        );
        assert_eq!(
            resolve_guest_path("/srv/app", "../lib").expect("parent"),
            "/srv/lib"
        );
        assert_eq!(resolve_guest_path("", "opt").expect("empty base"), "/opt");
        assert!(resolve_guest_path("relative", "app").is_err());
        assert!(resolve_guest_path("/", "../escape").is_err());
    }

    #[test]
    fn glob_match_basics() {
        assert!(glob_match("*.txt", "a.txt"));
        assert!(!glob_match("*.txt", "a.txt.bak"));
        assert!(glob_match("data?", "data1"));
        assert!(glob_match("[ab]*", "b12"));
        assert!(!glob_match("[!ab]*", "b12"));
        assert!(glob_match("pkg-*", "pkg-a"));
    }
}
