use crate::{
    analysis,
    index::Index,
    model::{Report, Snapshot},
    review,
    source::{GitSource, SourceProvider, WorkingTreeSource, CANCELLED},
};
use anyhow::{bail, Result};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{atomic::Ordering, mpsc},
    thread,
};

#[derive(Clone, Debug)]
pub(super) enum Request {
    Semantic(String),
    Review {
        base: String,
        head: Option<String>,
        semantic: bool,
    },
    ReviewPage {
        before: String,
        after: String,
        offset: usize,
    },
}

pub(super) enum Completed {
    Semantic {
        package: String,
        snapshot: Box<Snapshot>,
    },
    Review(Box<Report<Value>>),
}

pub(super) struct Job {
    receiver: mpsc::Receiver<Result<Completed, String>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Job {
    pub fn start(
        request: Request,
        base: Snapshot,
        cache: Option<PathBuf>,
        hashes: BTreeMap<String, String>,
    ) -> Self {
        let (sender, receiver) = mpsc::channel();
        let thread = thread::spawn(move || {
            let result = perform(request, base, cache, hashes).map_err(|e| format!("{e:#}"));
            let _ = sender.send(result);
        });
        Self {
            receiver,
            thread: Some(thread),
        }
    }

    pub fn poll(&mut self) -> Option<Result<Completed, String>> {
        match self.receiver.try_recv() {
            Ok(result) => {
                if let Some(thread) = self.thread.take() {
                    let _ = thread.join();
                }
                Some(result)
            }
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => Some(Err(crate::localize!(
                "后台分析异常退出；已有快照仍可浏览",
                "Background analysis exited unexpectedly; existing snapshots remain available"
            )
            .into())),
        }
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        if let Some(thread) = self.thread.take() {
            // The semantic backend owns and reaps its child processes on cancellation.
            // Never detach a worker that could still publish after the browser closes.
            if !thread.is_finished() {
                CANCELLED.store(true, Ordering::Relaxed);
            }
            let _ = thread.join();
        }
    }
}

fn perform(
    request: Request,
    base: Snapshot,
    cache: Option<PathBuf>,
    hashes: BTreeMap<String, String>,
) -> Result<Completed> {
    let root = PathBuf::from(&base.project_root);
    let mut index = Index::open(&root, cache.as_deref())?;
    match request {
        Request::Semantic(package) => {
            let source = WorkingTreeSource { root }.snapshot()?;
            if source.files.len() != hashes.len()
                || source
                    .files
                    .iter()
                    .any(|(p, f)| hashes.get(p) != Some(&f.hash))
            {
                bail!("源码已变化；请退出后重新打开项目。当前页面仍对应原始快照。");
            }
            let mut context = base.context;
            context.analyzer_version = crate::model::ANALYZER_VERSION.into();
            context.analysis = "semantic".into();
            context.scope = Some(package.clone());
            let snapshot = analysis::analyze(&mut index, &source, context, false)?;
            Ok(Completed::Semantic {
                package,
                snapshot: Box::new(snapshot),
            })
        }
        Request::Review {
            base: revision,
            head,
            semantic,
        } => {
            let before_source = GitSource {
                root: root.clone(),
                revision,
            }
            .snapshot()?;
            let after_source = match head {
                Some(revision) => GitSource { root, revision }.snapshot()?,
                None => WorkingTreeSource { root }.snapshot()?,
            };
            let mut context = base.context;
            context.analyzer_version = crate::model::ANALYZER_VERSION.into();
            context.analysis = if semantic { "semantic" } else { "syntax" }.into();
            context.scope = None;
            let before = analysis::analyze(&mut index, &before_source, context.clone(), false)?;
            let after = analysis::analyze(&mut index, &after_source, context, false)?;
            Ok(Completed::Review(Box::new(review::compare(
                &index, &before, &after, 50, 0,
            )?)))
        }
        Request::ReviewPage {
            before,
            after,
            offset,
        } => {
            let before = index.snapshot(Some(&before))?;
            let after = index.snapshot(Some(&after))?;
            Ok(Completed::Review(Box::new(review::compare(
                &index, &before, &after, 50, offset,
            )?)))
        }
    }
}
