//! Host-side planning for template `COPY` steps.
//!
//! The E2B SDK uploads one tar archive per `COPY` instruction whose entry
//! paths are relative to the build context (the glob in `src` is already
//! resolved by the SDK). This module rewrites that archive so every entry
//! carries its final absolute guest path per Docker `COPY` semantics; the
//! build sandbox then only needs a single `tar -xpf archive -C /`.
//!
//! Ownership is normalized to root:root (Docker's `COPY` default — the SDK
//! archive carries the uploader's local uids); an explicit `--chown` user is
//! applied afterwards with `chown -R` on the created roots.

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::Path;

use anyhow::{bail, Context, Result};

/// Summary of a rewritten copy archive.
#[derive(Debug)]
pub(crate) struct CopyPlan {
    /// Absolute guest paths of the top-level items this copy creates, used
    /// for the optional post-extract `chown -R`.
    pub(crate) created_roots: Vec<String>,
    /// Number of file/dir/symlink entries written to the rewritten archive.
    pub(crate) entry_count: usize,
}

/// One entry read from the SDK context archive.
struct SourceEntry {
    /// Normalized context-relative path ("dir/file.txt").
    path: String,
    header: tar::Header,
    link_name: Option<std::path::PathBuf>,
    /// Byte range of the entry data within the decompressed stream is not
    /// seekable, so file contents are buffered per entry during rewrite.
    data: Vec<u8>,
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

/// Joins and lexically normalizes an absolute guest destination path.
fn resolve_dest(dest: &str, workdir: &str) -> Result<String> {
    let joined = if dest.starts_with('/') {
        dest.to_string()
    } else {
        let workdir = if workdir.trim().is_empty() {
            "/"
        } else {
            workdir
        };
        if !workdir.starts_with('/') {
            bail!("workdir '{workdir}' must be absolute to resolve relative COPY destination");
        }
        format!("{}/{}", workdir.trim_end_matches('/'), dest)
    };

    let mut parts: Vec<&str> = Vec::new();
    for part in joined.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.pop().is_none() {
                    bail!("COPY destination '{dest}' escapes the filesystem root");
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

fn read_source_entries(source_tar: &Path) -> Result<Vec<SourceEntry>> {
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

    let mut archive = tar::Archive::new(reader);
    let mut entries = Vec::new();
    for entry in archive
        .entries()
        .context("read build context archive entries")?
    {
        let mut entry = entry.context("read build context archive entry")?;
        let entry_type = entry.header().entry_type();
        match entry_type {
            tar::EntryType::Regular
            | tar::EntryType::Directory
            | tar::EntryType::Symlink
            | tar::EntryType::GNUSparse => {}
            // Metadata-only companion entries (long names, pax headers) are
            // consumed by the tar crate itself and never surface here.
            other => bail!("unsupported entry type {other:?} in build context archive"),
        }

        let path = normalize_entry_path(&entry.path().context("entry path")?)?;
        let link_name = entry
            .link_name()
            .context("entry link name")?
            .map(|l| l.into_owned());
        let mut data = Vec::new();
        if entry_type == tar::EntryType::Regular || entry_type == tar::EntryType::GNUSparse {
            entry.read_to_end(&mut data).context("entry contents")?;
        }
        entries.push(SourceEntry {
            path,
            header: entry.header().clone(),
            link_name,
            data,
        });
    }
    if entries.is_empty() {
        bail!("build context archive contains no files");
    }
    Ok(entries)
}

/// Computes the final absolute guest path for every entry.
///
/// Returns `(mappings, created_roots)` where `mappings[i]` matches
/// `entries[i]`.
fn map_entries(
    entries: &[SourceEntry],
    src: &str,
    dest_raw: &str,
    workdir: &str,
) -> Result<(Vec<String>, Vec<String>)> {
    let src = normalize_src(src);
    let dest_is_dir_hint = dest_raw.ends_with('/')
        || dest_raw.ends_with("/.")
        || dest_raw == "."
        || dest_raw.is_empty();
    let dest = resolve_dest(if dest_raw.is_empty() { "." } else { dest_raw }, workdir)?;

    let mut mapped = Vec::with_capacity(entries.len());
    let mut roots: Vec<String> = Vec::new();
    let mut push_root = |root: String| {
        if !roots.contains(&root) {
            roots.push(root);
        }
    };

    let copy_whole_context = src.is_empty() || src == ".";
    let single_file_src = !copy_whole_context
        && !is_glob_pattern(&src)
        && entries.len() == 1
        && entries[0].path == src
        && entries[0].header.entry_type() != tar::EntryType::Directory;

    if single_file_src {
        let target = if dest_is_dir_hint {
            join_abs(&dest, base_name(&src))
        } else {
            dest.clone()
        };
        push_root(target.clone());
        mapped.push(target);
        return Ok((mapped, roots));
    }

    if copy_whole_context || !is_glob_pattern(&src) {
        // Directory source: Docker copies the directory *contents* into dest.
        for entry in entries {
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
        push_root(dest.clone());
        return Ok((mapped, roots));
    }

    // Glob source: every matched top-level item lands inside dest. Matched
    // files keep their base name; matched directories contribute their
    // contents (Docker treats each matched directory like a directory source).
    for entry in entries {
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
        let target = if rel.is_empty() && entry.header.entry_type() != tar::EntryType::Directory {
            let target = join_abs(&dest, base_name(&root));
            push_root(target.clone());
            target
        } else {
            push_root(dest.clone());
            join_abs(&dest, rel)
        };
        mapped.push(target);
    }
    Ok((mapped, roots))
}

/// Rewrites the SDK context archive into `output` with final absolute guest
/// paths, root ownership, and the optional mode override applied.
pub(crate) fn plan_copy_archive(
    source_tar: &Path,
    src: &str,
    dest: &str,
    workdir: &str,
    mode_override: Option<u32>,
    output: &Path,
) -> Result<CopyPlan> {
    let entries = read_source_entries(source_tar)?;
    let (mapped, created_roots) = map_entries(&entries, src, dest, workdir)?;

    let out_file = File::create(output)
        .with_context(|| format!("create rewritten copy archive '{}'", output.display()))?;
    let mut builder = tar::Builder::new(out_file);
    let mut entry_count = 0usize;

    for (entry, target) in entries.iter().zip(mapped.iter()) {
        let relative = target.trim_start_matches('/');
        if relative.is_empty() {
            // The destination root itself ("/"); parents always exist.
            continue;
        }

        let mut header = entry.header.clone();
        header.set_uid(0);
        header.set_gid(0);
        let _ = header.set_username("");
        let _ = header.set_groupname("");
        if let Some(mode) = mode_override {
            header.set_mode(mode);
        }

        match entry.header.entry_type() {
            tar::EntryType::Directory => {
                header.set_size(0);
                builder
                    .append_data(&mut header, format!("{relative}/"), std::io::empty())
                    .with_context(|| format!("write directory entry '{target}'"))?;
            }
            tar::EntryType::Symlink => {
                let link = entry
                    .link_name
                    .as_ref()
                    .context("symlink entry is missing its target")?;
                header.set_size(0);
                builder
                    .append_link(&mut header, relative, link)
                    .with_context(|| format!("write symlink entry '{target}'"))?;
            }
            _ => {
                header.set_size(entry.data.len() as u64);
                builder
                    .append_data(&mut header, relative, entry.data.as_slice())
                    .with_context(|| format!("write file entry '{target}'"))?;
            }
        }
        entry_count += 1;
    }

    let mut out_file = builder.into_inner().context("finish rewritten archive")?;
    out_file.flush().context("flush rewritten archive")?;

    Ok(CopyPlan {
        created_roots,
        entry_count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use tempfile::TempDir;

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

    fn rewritten_entries(path: &Path) -> BTreeMap<String, (tar::EntryType, u64, u32, String)> {
        let mut archive = tar::Archive::new(File::open(path).expect("open rewritten"));
        let mut out = BTreeMap::new();
        for entry in archive.entries().expect("entries") {
            let mut entry = entry.expect("entry");
            let path = entry.path().expect("path").to_string_lossy().into_owned();
            let kind = entry.header().entry_type();
            let uid = entry.header().uid().expect("uid");
            let mode = entry.header().mode().expect("mode");
            let mut contents = String::new();
            entry.read_to_string(&mut contents).expect("read");
            out.insert(path, (kind, uid, mode, contents));
        }
        out
    }

    #[test]
    fn single_file_to_absolute_file_dest() {
        let dir = TempDir::new().expect("tempdir");
        let tar = build_source_tar(dir.path(), &[("hello.txt", Some("hello\n"))]);
        let out = dir.path().join("out.tar");

        let plan =
            plan_copy_archive(&tar, "hello.txt", "/hello.txt", "/", None, &out).expect("plan");

        assert_eq!(plan.entry_count, 1);
        assert_eq!(plan.created_roots, vec!["/hello.txt".to_string()]);
        let entries = rewritten_entries(&out);
        let (kind, uid, _, contents) = &entries["hello.txt"];
        assert_eq!(*kind, tar::EntryType::Regular);
        assert_eq!(*uid, 0, "ownership must be normalized to root");
        assert_eq!(contents, "hello\n");
    }

    #[test]
    fn single_file_to_directory_dest() {
        let dir = TempDir::new().expect("tempdir");
        let tar = build_source_tar(dir.path(), &[("requirements.txt", Some("e2b\n"))]);
        let out = dir.path().join("out.tar");

        let plan = plan_copy_archive(&tar, "requirements.txt", "/home/user/", "/", None, &out)
            .expect("plan");

        assert_eq!(
            plan.created_roots,
            vec!["/home/user/requirements.txt".to_string()]
        );
        let entries = rewritten_entries(&out);
        assert!(entries.contains_key("home/user/requirements.txt"));
    }

    #[test]
    fn relative_dest_resolves_against_workdir() {
        let dir = TempDir::new().expect("tempdir");
        let tar = build_source_tar(dir.path(), &[("config.py", Some("x = 1\n"))]);
        let out = dir.path().join("out.tar");

        let plan =
            plan_copy_archive(&tar, "config.py", "conf/app.py", "/srv", None, &out).expect("plan");

        assert_eq!(plan.created_roots, vec!["/srv/conf/app.py".to_string()]);
        let entries = rewritten_entries(&out);
        assert!(entries.contains_key("srv/conf/app.py"));
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

        let plan = plan_copy_archive(&tar, "app", "/opt/service", "/", None, &out).expect("plan");

        assert_eq!(plan.created_roots, vec!["/opt/service".to_string()]);
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

        let plan = plan_copy_archive(&tar, ".", "/workspace", "/", None, &out).expect("plan");

        assert_eq!(plan.created_roots, vec!["/workspace".to_string()]);
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

        let plan = plan_copy_archive(&tar, "*.txt", "/data/", "/", None, &out).expect("plan");

        assert_eq!(plan.entry_count, 2);
        let entries = rewritten_entries(&out);
        assert!(entries.contains_key("data/one.txt"));
        assert!(entries.contains_key("data/two.txt"));
        assert_eq!(
            plan.created_roots,
            vec!["/data/one.txt".to_string(), "/data/two.txt".to_string()]
        );
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

        let plan = plan_copy_archive(&tar, "pkg-*", "/opt/pkgs", "/", None, &out).expect("plan");

        let entries = rewritten_entries(&out);
        // Docker merges contents of every matched directory into dest; the
        // second lib.py overwrites the first at extract time.
        assert!(entries.contains_key("opt/pkgs/lib.py"));
        assert_eq!(plan.created_roots, vec!["/opt/pkgs".to_string()]);
    }

    #[test]
    fn mode_override_applies_to_entries() {
        let dir = TempDir::new().expect("tempdir");
        let tar = build_source_tar(dir.path(), &[("run.sh", Some("#!/bin/sh\n"))]);
        let out = dir.path().join("out.tar");

        plan_copy_archive(
            &tar,
            "run.sh",
            "/usr/local/bin/run.sh",
            "/",
            Some(0o755),
            &out,
        )
        .expect("plan");

        let entries = rewritten_entries(&out);
        let (_, _, mode, _) = &entries["usr/local/bin/run.sh"];
        assert_eq!(*mode, 0o755);
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

        let plan =
            plan_copy_archive(&gz_path, "hello.txt", "/hello.txt", "/", None, &out).expect("plan");
        assert_eq!(plan.entry_count, 1);
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

        let err = plan_copy_archive(&tar_path, "passwd", "/tmp/x", "/", None, &out)
            .expect_err("path escape must fail");
        assert!(err.to_string().contains("unsupported path component"));
    }

    #[test]
    fn rejects_dest_escaping_the_root() {
        let dir = TempDir::new().expect("tempdir");
        let tar = build_source_tar(dir.path(), &[("a.txt", Some("a"))]);
        let out = dir.path().join("out.tar");

        let err = plan_copy_archive(&tar, "a.txt", "../../x", "/", None, &out)
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

        let err = plan_copy_archive(&tar_path, "x", "/x", "/", None, &out)
            .expect_err("empty archive must fail");
        assert!(err.to_string().contains("no files"));
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
