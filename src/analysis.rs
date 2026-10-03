use crate::frontend::{python::PythonFrontend, rust::RustFrontend, LanguageFrontend};
use crate::index::Index;
use crate::model::{
    identity, AnalysisContext, AnalysisStats, Completeness, Diagnostic, FileFacts, Snapshot,
};
use crate::project::{python::PythonProjectAdapter, ProjectAdapter, RustProjectAdapter};
use crate::semantic::{
    python::PythonSemanticResolver, rust::RustAnalyzerResolver, SemanticResolver,
};
use crate::source::{check_cancelled, SourceSet};
use anyhow::{Context, Result};
use std::collections::BTreeSet;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub fn analyze(
    index: &mut Index,
    sources: &SourceSet,
    context: AnalysisContext,
    progress: bool,
) -> Result<Snapshot> {
    let backend = match context.language.as_str() {
        "rust" => AnalysisBackend {
            project: &RustProjectAdapter,
            frontend: &RustFrontend,
            semantic: Some(&RustAnalyzerResolver),
        },
        "python" => AnalysisBackend {
            project: &PythonProjectAdapter,
            frontend: &PythonFrontend,
            semantic: Some(&PythonSemanticResolver),
        },
        other => anyhow::bail!("no analysis backend for language {other}"),
    };
    analyze_with(index, sources, context, backend, progress)
}

pub struct AnalysisBackend<'a> {
    pub project: &'a dyn ProjectAdapter,
    pub frontend: &'a dyn LanguageFrontend,
    pub semantic: Option<&'a dyn SemanticResolver>,
}

pub fn analyze_with(
    index: &mut Index,
    sources: &SourceSet,
    mut context: AnalysisContext,
    backend: AnalysisBackend<'_>,
    progress: bool,
) -> Result<Snapshot> {
    let started = Instant::now();
    let profiling = std::env::var_os("CODEXIS_PROFILE").is_some();
    let mut parse_time = std::time::Duration::ZERO;
    let mut cache_time = std::time::Duration::ZERO;
    check_cancelled()?;
    let project = backend.project.discover(sources)?;
    let frontend = backend.frontend;
    anyhow::ensure!(
        context.language == frontend.language(),
        "analysis language does not match the selected frontend"
    );
    if context.analysis == "semantic" {
        let provider = backend
            .semantic
            .and_then(|s| s.provider().ok())
            .unwrap_or_else(|| "semantic:unavailable".into());
        context.analyzer_version = format!("{}|{}", context.analyzer_version, provider);
    }
    let source_id = sources.content_id();
    let context_json = serde_json::to_string(&context)?;
    let snapshot_id = identity(&[
        &source_id,
        &context_json,
        backend.project.id(),
        frontend.version(),
    ]);
    if let Ok(mut cached) = index.snapshot(Some(&snapshot_id)) {
        if cached.completeness.status == "complete" {
            crate::source::verify_working_snapshot(sources)?;
            cached.created_at_ms = now_ms();
            cached.stats.parsed_files = 0;
            cached.stats.reused_files = cached.stats.source_files;
            cached.stats.elapsed_ms = started.elapsed().as_millis() as u64;
            index.connection.execute(
                "UPDATE snapshots SET created=?1,data=?2 WHERE id=?3",
                rusqlite::params![
                    cached.created_at_ms,
                    serde_json::to_string(&cached)?,
                    cached.id
                ],
            )?;
            if progress {
                eprintln!(
                    "Reusing completed snapshot: {} source files unchanged",
                    cached.stats.source_files
                );
            }
            return Ok(cached);
        }
    }
    let plan_started = Instant::now();
    let plan = frontend.plan(sources, &project, Some(index))?;
    let plan_time = plan_started.elapsed();
    let mut facts = FileFacts::default();
    let mut diagnostics = sources.diagnostics.clone();
    diagnostics.extend(project.diagnostics.clone());
    let mut parsed = BTreeSet::new();
    let mut reused = BTreeSet::new();
    let mut failed = BTreeSet::new();
    for (batch_index, batch) in plan.chunks(32).enumerate() {
        let mut ready = Vec::with_capacity(batch.len());
        let mut missing = Vec::new();
        for file_context in batch {
            check_cancelled()?;
            let file = sources
                .files
                .get(&file_context.path)
                .context("planned file is missing")?;
            let cache_key = identity(&[
                frontend.version(),
                &file.hash,
                &serde_json::to_string(file_context)?,
            ]);
            let cache_started = Instant::now();
            let cached = index.cached(&cache_key)?;
            cache_time += cache_started.elapsed();
            if cached.is_some() {
                reused.insert(file.path.clone());
            } else {
                missing.push((file, file_context, cache_key));
            }
            ready.push((file, cached));
        }
        let parse_started = Instant::now();
        let results = crate::frontend::parallel_map(&missing, |(file, context, _)| {
            check_cancelled()?;
            frontend
                .parse(file, context)
                .with_context(|| format!("parse {}", file.path))
        })?;
        parse_time += parse_started.elapsed();
        let mut results = results.into_iter().zip(missing.iter());
        for (file, cached) in ready {
            let file_facts = match cached {
                Some(cached) => cached,
                None => {
                    let (facts, (_, _, key)) =
                        results.next().context("missing parsed file result")?;
                    let cache_started = Instant::now();
                    index.cache(key, &facts)?;
                    cache_time += cache_started.elapsed();
                    parsed.insert(file.path.clone());
                    facts
                }
            };
            if file_facts
                .diagnostics
                .iter()
                .any(|d| d.code.ends_with("_parse_error") || d.code == "syntax_depth_limit")
            {
                failed.insert(file.path.clone());
            }
            facts.nodes.extend(file_facts.nodes);
            facts.edges.extend(file_facts.edges);
            diagnostics.extend(file_facts.diagnostics);
        }
        if progress && (batch_index % 4 == 0 || (batch_index + 1) * 32 >= plan.len()) {
            eprintln!(
                "Indexing {}: {}/{} file contexts",
                frontend.language(),
                ((batch_index + 1) * 32).min(plan.len()),
                plan.len()
            );
        }
    }
    let mut capabilities = frontend.capabilities();
    let mut status = if failed.is_empty() && sources.diagnostics.is_empty() {
        "complete"
    } else {
        "partial"
    }
    .to_owned();
    if context.analysis == "semantic" {
        let materialized;
        let semantic_source =
            if sources.revision.starts_with("worktree:") || sources.revision == "directory" {
                sources
            } else {
                materialized = crate::source::materialize(
                    sources,
                    index.path.parent().context("cache root missing")?,
                )?;
                &materialized
            };
        let semantic_result = match backend.semantic {
            Some(resolver) => {
                resolver.enrich(semantic_source, &project, &context, &mut facts, progress)
            }
            None => Err(anyhow::anyhow!(
                "no semantic resolver registered for {}",
                frontend.language()
            )),
        };
        match semantic_result {
            Ok(outcome) => {
                capabilities.push(outcome.capability);
                diagnostics.extend(outcome.diagnostics);
                if outcome.partial {
                    status = "partial".into();
                }
            }
            Err(error) => {
                check_cancelled()?;
                // An enrichment may fail after mutating facts (for example a
                // final source-integrity check). Restore the exact syntax facts
                // from the already-written cache, not guessed unresolved edges.
                // This avoids cloning the entire graph on successful analyses.
                facts = FileFacts::default();
                for file_context in &plan {
                    check_cancelled()?;
                    let file = &sources.files[&file_context.path];
                    let key = identity(&[
                        frontend.version(),
                        &file.hash,
                        &serde_json::to_string(file_context)?,
                    ]);
                    let original = index
                        .cached(&key)?
                        .context("syntax cache missing while rolling back semantic enrichment")?;
                    facts.nodes.extend(original.nodes);
                    facts.edges.extend(original.edges);
                }
                status = "partial".into();
                diagnostics.push(Diagnostic::warning(
                    "semantic_unavailable",
                    format!("{error:#}"),
                    None,
                ));
            }
        }
    }
    capabilities.sort_by(|a, b| a.name.cmp(&b.name));
    let planned_paths: BTreeSet<_> = plan.iter().map(|c| &c.path).collect();
    let source_files: Vec<_> = planned_paths
        .iter()
        .filter_map(|p| sources.files.get(*p))
        .collect();
    let unresolved = facts
        .edges
        .iter()
        .filter(|e| e.kind == "calls" && e.target.is_none())
        .count();
    let stats = AnalysisStats {
        discovered_files: sources.files.len(),
        source_files: source_files.len(),
        parsed_files: parsed.len(),
        reused_files: reused.difference(&parsed).count(),
        failed_files: failed.len(),
        source_bytes: source_files.iter().map(|f| f.content.len()).sum(),
        source_lines: source_files.iter().map(|f| f.content.lines().count()).sum(),
        nodes: facts.nodes.len(),
        edges: facts.edges.len(),
        unresolved_calls: unresolved,
        resolved_calls: facts
            .edges
            .iter()
            .filter(|e| e.kind == "calls" && e.target.is_some())
            .count(),
        elapsed_ms: started.elapsed().as_millis() as u64,
    };
    diagnostics.sort_by(|a, b| (&a.path, &a.code, &a.message).cmp(&(&b.path, &b.code, &b.message)));
    diagnostics.dedup_by(|a, b| a.path == b.path && a.code == b.code && a.message == b.message);
    let mut snapshot = Snapshot {
        id: snapshot_id,
        project_root: sources.root.to_string_lossy().into_owned(),
        source_revision: sources.revision.clone(),
        created_at_ms: now_ms(),
        context,
        project,
        stats,
        completeness: Completeness {
            status,
            capabilities,
            unresolved,
            ..Completeness::default()
        },
        diagnostics,
    };
    crate::source::verify_working_snapshot(sources)?;
    let publish_started = Instant::now();
    index.publish(&snapshot, sources, &facts)?;
    if profiling {
        eprintln!(
            "CODEXIS_PROFILE plan_ms={} parse_ms={} syntax_cache_ms={} publish_ms={} total_ms={}",
            plan_time.as_millis(),
            parse_time.as_millis(),
            cache_time.as_millis(),
            publish_started.elapsed().as_millis(),
            started.elapsed().as_millis()
        );
    }
    snapshot.stats.elapsed_ms = started.elapsed().as_millis() as u64;
    index.connection.execute(
        "UPDATE snapshots SET data=?1 WHERE id=?2",
        rusqlite::params![serde_json::to_string(&snapshot)?, snapshot.id],
    )?;
    Ok(snapshot)
}
