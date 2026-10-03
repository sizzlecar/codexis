pub mod python;
pub mod rust;

use crate::model::{Capability, FileFacts, ProjectModel};
use crate::source::{SourceFile, SourceSet};
use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FileContext {
    pub path: String,
    pub package: String,
    pub unit: String,
    pub module: String,
    pub conditions: Vec<String>,
    pub is_test: bool,
    pub linked: bool,
}

pub trait FrontendCache {
    fn load_record(&self, key: &str) -> Result<Option<String>>;
    fn store_record(&self, key: &str, record: &str) -> Result<()>;
}

pub trait LanguageFrontend: Sync {
    fn language(&self) -> &str;
    fn version(&self) -> &str;
    fn capabilities(&self) -> Vec<Capability>;
    fn plan(
        &self,
        sources: &SourceSet,
        project: &ProjectModel,
        cache: Option<&dyn FrontendCache>,
    ) -> Result<Vec<FileContext>>;
    fn parse(&self, file: &SourceFile, context: &FileContext) -> Result<FileFacts>;
}

/// Bounded CPU parallelism without sharing the SQLite connection. Results stay
/// in input order, so scheduling cannot change identities, output or evidence.
pub(crate) fn parallel_map<T: Sync, R: Send>(
    items: &[T],
    operation: impl Fn(&T) -> Result<R> + Sync,
) -> Result<Vec<R>> {
    let workers = std::thread::available_parallelism()
        .map_or(1, usize::from)
        .min(4)
        .min(items.len());
    if workers <= 1 || items.len() < 8 {
        return items.iter().map(operation).collect();
    }
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..workers)
            .map(|worker| {
                let operation = &operation;
                scope.spawn(move || {
                    items
                        .iter()
                        .enumerate()
                        .skip(worker)
                        .step_by(workers)
                        .map(|(position, item)| operation(item).map(|result| (position, result)))
                        .collect::<Result<Vec<_>>>()
                })
            })
            .collect();
        let mut results = Vec::with_capacity(items.len());
        for handle in handles {
            results.extend(
                handle
                    .join()
                    .map_err(|_| anyhow::anyhow!("frontend worker panicked"))??,
            );
        }
        results.sort_by_key(|(position, _)| *position);
        Ok(results.into_iter().map(|(_, result)| result).collect())
    })
}
