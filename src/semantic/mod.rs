mod lsp;
pub mod python;
pub mod rust;

use crate::model::{AnalysisContext, Capability, Diagnostic, FileFacts, ProjectModel};
use crate::source::SourceSet;
use anyhow::Result;

pub struct SemanticOutcome {
    pub capability: Capability,
    pub diagnostics: Vec<Diagnostic>,
    pub partial: bool,
}

pub trait SemanticResolver {
    fn provider(&self) -> Result<String>;
    fn enrich(
        &self,
        source: &SourceSet,
        project: &ProjectModel,
        context: &AnalysisContext,
        facts: &mut FileFacts,
        progress: bool,
    ) -> Result<SemanticOutcome>;
}
