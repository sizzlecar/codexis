use crate::model::{digest, identity, Diagnostic};
use anyhow::{bail, Context, Result};
use ignore::WalkBuilder;
use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};

pub static CANCELLED: AtomicBool = AtomicBool::new(false);

pub fn check_cancelled() -> Result<()> {
    if CANCELLED.load(Ordering::Relaxed) {
        bail!("analysis cancelled; the previous completed snapshot is preserved");
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub struct SourceFile {
    pub path: String,
    pub content: String,
    pub hash: String,
}

impl SourceFile {
    pub fn new(path: String, content: String) -> Self {
        let hash = digest(content.as_bytes());
        Self {
            path,
            content,
            hash,
        }
    }
}

#[derive(Clone, Debug)]
pub struct SourceSet {
    pub root: PathBuf,
    pub revision: String,
    pub files: BTreeMap<String, SourceFile>,
    pub diagnostics: Vec<Diagnostic>,
}

impl SourceSet {
    pub fn content_id(&self) -> String {
        let diagnostics =
            serde_json::to_string(&self.diagnostics).expect("diagnostics are serializable");
        let mut parts = vec![self.revision.as_str(), &diagnostics];
        for file in self.files.values() {
            parts.extend([file.path.as_str(), file.hash.as_str()]);
        }
        identity(&parts)
    }
}

pub trait SourceProvider {
    fn snapshot(&self) -> Result<SourceSet>;
}

pub struct WorkingTreeSource {
    pub root: PathBuf,
}

pub fn allowed_path(path: &str) -> bool {
    let path = Path::new(path);
    if path.components().any(|c| {
        matches!(c, Component::Normal(s) if [".git", ".worktrees", ".codexis", "target", "node_modules", "vendor", "third_party", "__pycache__", ".venv", "venv", ".pytest_cache", ".mypy_cache"].iter().any(|v| s == *v))
    }) {
        return false;
    }
    let name = path.file_name().and_then(|v| v.to_str()).unwrap_or("");
    // Dependency locks are derived and often dominate the captured evidence.
    // Cargo.lock remains part of the existing build-context contract.
    if matches!(
        name,
        "package-lock.json" | "npm-shrinkwrap.json" | "pnpm-lock.yaml"
    ) {
        return false;
    }
    if name == "config" && path.parent().is_some_and(|p| p.ends_with(".cargo")) {
        return true;
    }
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let document_text = extension == "txt"
        && (name.starts_with("requirements")
            || ["readme", "license", "notice", "changelog", "authors", "contributing"]
                .iter()
                .any(|prefix| name.to_ascii_lowercase().starts_with(prefix))
            || path.components().any(|c| {
                matches!(c, Component::Normal(s) if ["doc", "docs", "documentation"].iter().any(|v| s == *v))
            }));
    matches!(
        extension.as_str(),
        "rs" | "py"
            | "pyi"
            | "toml"
            | "xml"
            | "md"
            | "markdown"
            | "rst"
            | "adoc"
            | "yaml"
            | "yml"
            | "json"
            | "sql"
            | "proto"
            | "graphql"
            | "gql"
            | "sh"
            | "properties"
            | "ini"
            | "cfg"
    ) || matches!(
        name,
        "Cargo.lock"
            | ".gitignore"
            | ".dockerignore"
            | "Dockerfile"
            | "Containerfile"
            | "Makefile"
            | "Justfile"
            | "Jenkinsfile"
            | "README"
            | "LICENSE"
            | "NOTICE"
    ) || name.starts_with("Dockerfile.")
        || name.starts_with("Containerfile.")
        || document_text
}

pub fn relative_path(path: &Path) -> Result<String> {
    if path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::Prefix(_)))
    {
        bail!("path escapes the source root: {}", path.display());
    }
    Ok(path
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/"))
}

impl SourceProvider for WorkingTreeSource {
    fn snapshot(&self) -> Result<SourceSet> {
        let root = self
            .root
            .canonicalize()
            .context("cannot open project directory")?;
        if !root.is_dir() {
            bail!("project must be a directory: {}", root.display());
        }
        let mut files = BTreeMap::new();
        let mut diagnostics = Vec::new();
        let mut walk = WalkBuilder::new(&root);
        walk.hidden(false)
            .follow_links(false)
            .require_git(false)
            .filter_entry(|entry| {
                !entry.file_type().is_some_and(|t| t.is_dir())
                    || ((!entry.file_name().to_string_lossy().starts_with('.')
                        || [".cargo", ".github", ".gitlab", ".circleci", ".devcontainer"]
                            .iter()
                            .any(|name| entry.file_name() == *name)
                        || entry.depth() == 0)
                        && ![
                            "target",
                            "node_modules",
                            "vendor",
                            "third_party",
                            "__pycache__",
                            "venv",
                        ]
                        .iter()
                        .any(|name| entry.file_name() == *name))
            });
        for entry in walk.build() {
            check_cancelled()?;
            let entry = entry.context("cannot enumerate project files")?;
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            let path = relative_path(entry.path().strip_prefix(&root)?)?;
            if !allowed_path(&path) {
                continue;
            }
            let metadata = entry.metadata()?;
            if metadata.len() > 32 * 1024 * 1024 {
                diagnostics.push(Diagnostic::warning(
                    "source_too_large",
                    "source exceeds the 32 MiB per-file limit",
                    Some(&path),
                ));
                continue;
            }
            let bytes = fs::read(entry.path()).with_context(|| format!("read {path}"))?;
            let content = match String::from_utf8(bytes) {
                Ok(value) => value,
                Err(_) => {
                    diagnostics.push(Diagnostic::warning(
                        "non_utf8_source",
                        "source is not valid UTF-8",
                        Some(&path),
                    ));
                    continue;
                }
            };
            let after = fs::metadata(entry.path())?;
            if metadata.len() != after.len() || metadata.modified()? != after.modified()? {
                bail!("{path} changed during capture; retry analysis");
            }
            files.insert(path.clone(), SourceFile::new(path, content));
        }
        let revision = git(&root, &["rev-parse", "--verify", "HEAD"])
            .map(|v| format!("worktree:{}", String::from_utf8_lossy(&v.stdout).trim()))
            .unwrap_or_else(|_| "directory".into());
        Ok(SourceSet {
            root,
            revision,
            files,
            diagnostics,
        })
    }
}

pub fn git(root: &Path, args: &[&str]) -> Result<Output> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .context("failed to start git")?;
    if !output.status.success() {
        bail!(
            "git failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output)
}

pub struct GitSource {
    pub root: PathBuf,
    pub revision: String,
}

impl SourceProvider for GitSource {
    fn snapshot(&self) -> Result<SourceSet> {
        let root = self.root.canonicalize()?;
        let requested = format!("{}^{{commit}}", self.revision);
        let resolved = git(
            &root,
            &["rev-parse", "--verify", "--end-of-options", &requested],
        )?;
        let revision = String::from_utf8(resolved.stdout)?.trim().to_owned();
        let repository = git(&root, &["rev-parse", "--show-toplevel"])?;
        let repository =
            PathBuf::from(String::from_utf8(repository.stdout)?.trim()).canonicalize()?;
        let prefix = relative_path(root.strip_prefix(&repository)?)?;
        let listing = git(&repository, &["ls-tree", "-rz", "--full-tree", &revision])?;
        let mut blobs = GitBlobs::open(&repository)?;
        let mut files = BTreeMap::new();
        let mut diagnostics = Vec::new();
        for record in listing.stdout.split(|b| *b == 0).filter(|v| !v.is_empty()) {
            check_cancelled()?;
            let record = std::str::from_utf8(record).context("non-UTF8 Git path")?;
            let (metadata, path) = record.split_once('\t').context("invalid Git tree record")?;
            let path = if prefix.is_empty() {
                path
            } else {
                let Some(relative) = path.strip_prefix(&format!("{prefix}/")) else {
                    continue;
                };
                relative
            };
            if !allowed_path(path) {
                continue;
            }
            relative_path(Path::new(path))?;
            let fields: Vec<_> = metadata.split_whitespace().collect();
            if fields.len() != 3 || fields[1] != "blob" || fields[0] == "120000" {
                continue;
            }
            let Some(bytes) = blobs.read(fields[2])? else {
                diagnostics.push(Diagnostic::warning(
                    "source_too_large",
                    "Git source exceeds the 32 MiB per-file limit",
                    Some(path),
                ));
                continue;
            };
            let content = match String::from_utf8(bytes) {
                Ok(v) => v,
                Err(_) => {
                    diagnostics.push(Diagnostic::warning(
                        "non_utf8_source",
                        "Git blob is not UTF-8",
                        Some(path),
                    ));
                    continue;
                }
            };
            files.insert(path.into(), SourceFile::new(path.into(), content));
        }
        Ok(SourceSet {
            root,
            revision,
            files,
            diagnostics,
        })
    }
}

struct GitBlobs {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}

impl GitBlobs {
    fn open(root: &Path) -> Result<Self> {
        let mut child = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["cat-file", "--batch"])
            .env("GIT_OPTIONAL_LOCKS", "0")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let input = child.stdin.take().context("git batch stdin missing")?;
        let output = BufReader::new(child.stdout.take().context("git batch stdout missing")?);
        Ok(Self {
            child,
            input,
            output,
        })
    }

    fn read(&mut self, object: &str) -> Result<Option<Vec<u8>>> {
        writeln!(self.input, "{object}")?;
        self.input.flush()?;
        let mut header = String::new();
        self.output.read_line(&mut header)?;
        let fields: Vec<_> = header.split_whitespace().collect();
        if fields.len() != 3 || fields[1] != "blob" {
            bail!("invalid git blob response: {header}");
        }
        let length = fields[2].parse::<u64>()?;
        let result = if length > 32 * 1024 * 1024 {
            std::io::copy(&mut self.output.by_ref().take(length), &mut std::io::sink())?;
            None
        } else {
            let mut content = vec![0; length as usize];
            self.output.read_exact(&mut content)?;
            Some(content)
        };
        let mut newline = [0u8];
        self.output.read_exact(&mut newline)?;
        if newline[0] != b'\n' {
            bail!("invalid git blob delimiter");
        }
        Ok(result)
    }
}

impl Drop for GitBlobs {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The working tree changed while a snapshot was being analyzed.
#[derive(Debug)]
pub struct SourceChanged {
    pub paths: Vec<String>,
}

impl SourceChanged {
    /// The first few changed paths, for messages.
    pub fn summary(&self) -> String {
        let mut shown: Vec<&str> = self.paths.iter().take(3).map(String::as_str).collect();
        let more = self.paths.len().saturating_sub(shown.len());
        let extra;
        if more > 0 {
            extra = crate::localize!("等 {} 个文件", "and {} more", more);
            shown.push(&extra);
        }
        shown.join(", ")
    }
}

impl std::fmt::Display for SourceChanged {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&crate::localize!(
            "分析期间项目文件发生变化（{}）；请在这些文件停止修改后重试",
            "project changed during analysis ({}); retry once these files stop changing",
            self.summary()
        ))
    }
}

impl std::error::Error for SourceChanged {}

pub fn verify_working_snapshot(source: &SourceSet) -> Result<()> {
    if source.revision.starts_with("worktree:") || source.revision == "directory" {
        let current = WorkingTreeSource {
            root: source.root.clone(),
        }
        .snapshot()?;
        if current.content_id() != source.content_id() {
            let mut paths: Vec<String> = source
                .files
                .iter()
                .filter(|(path, file)| current.files.get(*path).is_none_or(|c| c.hash != file.hash))
                .map(|(path, _)| path.clone())
                .collect();
            paths.extend(
                current
                    .files
                    .keys()
                    .filter(|path| !source.files.contains_key(*path))
                    .cloned(),
            );
            paths.sort();
            paths.dedup();
            return Err(SourceChanged { paths }.into());
        }
    }
    Ok(())
}

pub fn materialize(source: &SourceSet, cache_root: &Path) -> Result<SourceSet> {
    let root = cache_root
        .join("semantic-snapshots")
        .join(source.content_id());
    fs::create_dir_all(&root)?;
    for file in source.files.values() {
        check_cancelled()?;
        let relative = relative_path(Path::new(&file.path))?;
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut output) => output.write_all(file.content.as_bytes())?,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                if path.symlink_metadata()?.file_type().is_symlink()
                    || digest(fs::read(&path)?) != file.hash
                {
                    bail!(
                        "cached semantic source differs from snapshot: {}",
                        path.display()
                    );
                }
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(SourceSet {
        root: root.canonicalize()?,
        revision: source.revision.clone(),
        files: source.files.clone(),
        diagnostics: source.diagnostics.clone(),
    })
}

#[cfg(test)]
mod change_tests {
    use super::*;

    #[test]
    fn edits_during_analysis_name_the_changed_files() {
        let root = tempfile::TempDir::new().unwrap();
        fs::write(root.path().join("lib.rs"), "pub fn a() {}\n").unwrap();
        fs::write(root.path().join("NOTES.md"), "draft\n").unwrap();
        let source = WorkingTreeSource {
            root: root.path().into(),
        }
        .snapshot()
        .unwrap();
        verify_working_snapshot(&source).unwrap();
        fs::write(root.path().join("NOTES.md"), "edited by another tool\n").unwrap();
        let error = verify_working_snapshot(&source).unwrap_err();
        let changed = error.downcast_ref::<SourceChanged>().unwrap();
        assert_eq!(changed.paths, ["NOTES.md"]);
    }
}
