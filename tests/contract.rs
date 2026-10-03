//! A deliberately non-Rust backend exercises the public adapter contract.
use anyhow::Result;
use codexis::{
    analysis::{analyze_with, AnalysisBackend},
    frontend::{FileContext, FrontendCache, LanguageFrontend},
    index::Index,
    model::*,
    project::ProjectAdapter,
    semantic::{SemanticOutcome, SemanticResolver},
    source::{SourceFile, SourceProvider, SourceSet},
};
use std::collections::BTreeMap;
use tempfile::TempDir;

struct Fixture {
    source: SourceSet,
}

struct FailingResolver;
impl SemanticResolver for FailingResolver {
    fn provider(&self) -> Result<String> {
        Ok("failing-fixture:1".into())
    }
    fn enrich(
        &self,
        _: &SourceSet,
        _: &ProjectModel,
        _: &AnalysisContext,
        facts: &mut FileFacts,
        _: bool,
    ) -> Result<SemanticOutcome> {
        facts.edges[0].target = Some(facts.nodes[1].id.clone());
        facts.edges[0].resolution = "semantic".into();
        facts.edges[0].provider = "unverified".into();
        facts.nodes.pop();
        anyhow::bail!("backend failed final source-integrity check")
    }
}

impl SourceProvider for Fixture {
    fn snapshot(&self) -> Result<SourceSet> {
        Ok(self.source.clone())
    }
}
impl ProjectAdapter for Fixture {
    fn id(&self) -> &str {
        "fixture-project:1"
    }
    fn detects(&self, _: &SourceSet) -> bool {
        true
    }
    fn discover(&self, _: &SourceSet) -> Result<ProjectModel> {
        Ok(ProjectModel {
            kind: "fixture".into(),
            packages: vec![Package {
                id: "fixture-package".into(),
                name: "fixture".into(),
                root: "".into(),
                language: "fixture-language".into(),
                edition: "".into(),
                units: vec![],
                dependencies: vec![],
                features: BTreeMap::new(),
            }],
            diagnostics: vec![],
        })
    }
}
impl LanguageFrontend for Fixture {
    fn language(&self) -> &str {
        "fixture-language"
    }
    fn version(&self) -> &str {
        "fixture-syntax:1"
    }
    fn capabilities(&self) -> Vec<Capability> {
        vec![Capability {
            name: "fixture syntax".into(),
            provider: "test".into(),
            scope: "fixture".into(),
            limitations: vec![],
        }]
    }
    fn plan(
        &self,
        _: &SourceSet,
        _: &ProjectModel,
        _: Option<&dyn FrontendCache>,
    ) -> Result<Vec<FileContext>> {
        Ok(vec![FileContext {
            path: "sample.toy".into(),
            package: "fixture-package".into(),
            unit: "fixture-unit".into(),
            module: "fixture".into(),
            conditions: vec![],
            is_test: false,
            linked: true,
        }])
    }
    fn parse(&self, file: &SourceFile, _: &FileContext) -> Result<FileFacts> {
        let mut facts = FileFacts::default();
        let mut offset = 0;
        for (line, text) in file.content.lines().enumerate() {
            let name = text.split([' ', '=']).next().unwrap();
            let evidence = Evidence {
                path: file.path.clone(),
                content_hash: file.hash.clone(),
                start_byte: offset,
                end_byte: offset + text.len(),
                start_line: line + 1,
                start_column: 0,
                end_line: line + 1,
                end_column: text.len(),
            };
            facts.nodes.push(Node {
                id: identity(&[name, &offset.to_string()]),
                stable_key: identity(&[name]),
                name: name.into(),
                qualified_name: format!("fixture::{name}"),
                kind: "function".into(),
                language: self.language().into(),
                package: "fixture-package".into(),
                unit: "fixture-unit".into(),
                parent: None,
                visibility: "pub".into(),
                signature: name.into(),
                fingerprint: digest(text),
                evidence,
                conditions: vec![],
                is_test: false,
                attributes: BTreeMap::new(),
            });
            offset += text.len() + 1;
        }
        facts.edges.push(Edge {
            id: "fixture-call".into(),
            source: facts.nodes[0].id.clone(),
            target: None,
            target_name: "leaf".into(),
            kind: "calls".into(),
            resolution: "unresolved".into(),
            evidence: facts.nodes[0].evidence.clone(),
            conditions: vec![],
            provider: "fixture-syntax".into(),
        });
        Ok(facts)
    }
}
impl SemanticResolver for Fixture {
    fn provider(&self) -> Result<String> {
        Ok("fixture-semantic:1".into())
    }
    fn enrich(
        &self,
        _: &SourceSet,
        _: &ProjectModel,
        _: &AnalysisContext,
        facts: &mut FileFacts,
        _: bool,
    ) -> Result<SemanticOutcome> {
        facts.edges[0].target = Some(facts.nodes[1].id.clone());
        facts.edges[0].resolution = "semantic".into();
        facts.edges[0].provider = "fixture-semantic".into();
        Ok(SemanticOutcome {
            capability: Capability {
                name: "fixture semantics".into(),
                provider: "fixture-semantic".into(),
                scope: "fixture".into(),
                limitations: vec![],
            },
            diagnostics: vec![],
            partial: false,
        })
    }
}

#[test]
fn non_rust_adapter_uses_the_same_cache_query_review_marks_and_outputs() {
    let root = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    let mut fixture = Fixture {
        source: SourceSet {
            root: root.path().canonicalize().unwrap(),
            revision: "fixture:1".into(),
            files: BTreeMap::from([(
                "sample.toy".into(),
                SourceFile::new("sample.toy".into(), "entry -> leaf\nleaf=v1\n".into()),
            )]),
            diagnostics: vec![],
        },
    };
    let mut index = Index::open(root.path(), Some(cache.path())).unwrap();
    let context = AnalysisContext {
        language: "fixture-language".into(),
        analysis: "semantic".into(),
        analyzer_version: "fixture-orchestrator:1".into(),
        ..AnalysisContext::default()
    };
    let analyze = |index: &mut Index, fixture: &Fixture| {
        analyze_with(
            index,
            &fixture.snapshot().unwrap(),
            context.clone(),
            AnalysisBackend {
                project: fixture,
                frontend: fixture,
                semantic: Some(fixture),
            },
            false,
        )
        .unwrap()
    };
    let first = analyze(&mut index, &fixture);
    assert_eq!(first.stats.parsed_files, 1);
    let reused = analyze(&mut index, &fixture);
    assert_eq!(reused.stats.parsed_files, 0);
    assert_eq!(reused.id, first.id);
    let report = codexis::query::trace(&index, &first, "entry", false, 3, 10).unwrap();
    assert_eq!(report.analysis_context.language, "fixture-language");
    assert_eq!(report.data["nodes"].as_array().unwrap().len(), 2);
    for format in ["text", "markdown", "json"] {
        let rendered = codexis::render::render(&report, format).unwrap();
        assert!(rendered.contains("fixture-language"));
    }
    let entry = index.find_nodes(&first.id, "entry", 1).unwrap().remove(0);
    codexis::marks::set(&index, &first, &entry, "seen", "generic").unwrap();
    fixture.source.files.insert(
        "sample.toy".into(),
        SourceFile::new("sample.toy".into(), "entry -> leaf\nleaf=v2\n".into()),
    );
    let second = analyze(&mut index, &fixture);
    let changed_entry = index.find_nodes(&second.id, "entry", 1).unwrap().remove(0);
    assert_eq!(entry.fingerprint, changed_entry.fingerprint);
    assert_eq!(
        codexis::marks::get(&index, &second, &changed_entry).unwrap()["state"],
        "needs_review"
    );
    let review = codexis::review::compare(&index, &first, &second, 10, 0).unwrap();
    assert_eq!(review.data["total_changes"], 1);
    assert_eq!(
        review.data["changes"][0]["current_impacts"][0]["caller"]["name"],
        "entry"
    );
    let unavailable = analyze_with(
        &mut index,
        &fixture.source,
        context,
        AnalysisBackend {
            project: &fixture,
            frontend: &fixture,
            semantic: None,
        },
        false,
    )
    .unwrap();
    assert_eq!(unavailable.completeness.status, "partial");
    assert!(unavailable
        .diagnostics
        .iter()
        .any(|d| d.code == "semantic_unavailable"));

    let failed = analyze_with(
        &mut index,
        &fixture.source,
        unavailable.context.clone(),
        AnalysisBackend {
            project: &fixture,
            frontend: &fixture,
            semantic: Some(&FailingResolver),
        },
        false,
    )
    .unwrap();
    assert_eq!(failed.completeness.status, "partial");
    assert_eq!(failed.stats.nodes, 2);
    assert_eq!(failed.stats.resolved_calls, 0);
    let edges = index.all_edges(&failed.id).unwrap();
    assert!(edges[0].target.is_none());
    assert_eq!(edges[0].resolution, "unresolved");
    assert_eq!(edges[0].provider, "fixture-syntax");
    // Failure cannot corrupt a prior verified semantic snapshot.
    assert_eq!(
        index.all_edges(&first.id).unwrap()[0].resolution,
        "semantic"
    );
}
