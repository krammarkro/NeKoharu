use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::Instant,
};

use anyhow::{Context as _, Result, ensure};
use futures::{StreamExt as _, stream::FuturesUnordered};
use koharu_scene::{EntityId, RegionSpec, Snapshot};

use crate::{
    Committer, ErrorKind, PipelineError, Progress, ProgressSink, Report, Request, RunStatus, Stage,
    StageOutput, StopToken,
    images::ImageCache,
    progress,
    resources::ResourceMonitor,
    scheduler::Scheduler,
    scope::NormalizedScope,
    stage_runner::{StageCompletion, StageJob, StageOutcome, StageRunner},
    stages::StageInput,
};

pub(crate) struct Execution<'a> {
    runner: Arc<StageRunner>,
    resources: Arc<ResourceMonitor>,
    committer: &'a mut dyn Committer,
    stop: StopToken,
    progress: Option<ProgressSink>,
    scope: NormalizedScope,
    scheduler: Scheduler,
    scene: Snapshot,
    images: BTreeMap<EntityId, Arc<ImageCache>>,
    busy_stages: BTreeSet<Stage>,
    completed: usize,
    failure: Option<PipelineError>,
    base: koharu_scene::Revision,
    started: Instant,
    inpainting_mask: Option<crate::InpaintingMask>,
    text_box: Option<crate::TextBox>,
}

impl<'a> Execution<'a> {
    pub(crate) fn new(
        runner: Arc<StageRunner>,
        resources: Arc<ResourceMonitor>,
        snapshot: Snapshot,
        request: Request,
        committer: &'a mut dyn Committer,
    ) -> std::result::Result<Self, PipelineError> {
        let started = Instant::now();
        let base = snapshot.revision();
        let stages = request
            .operation
            .stages()
            .map_err(|error| PipelineError::new(ErrorKind::InvalidInput, None, error))?;
        let scope = NormalizedScope::new(
            &snapshot,
            &request.scope,
            &stages,
            request.inpainting_mask.is_some(),
        )
        .map_err(|error| PipelineError::new(ErrorKind::InvalidInput, None, error))?;
        let mut pages = Vec::new();
        for page in scope.pages() {
            let finished = request.skip_finished_pages
                && stages
                    .iter()
                    .map(|stage| runner_is_finished(&snapshot, *page, *stage))
                    .collect::<anyhow::Result<Vec<_>>>()
                    .map_err(|error| {
                        PipelineError::new(ErrorKind::InvalidInput, None, error)
                    })?
                    .into_iter()
                    .all(|finished| finished);
            if !finished {
                pages.push(*page);
            }
        }
        let translation_batch_size = if stages.contains(&Stage::Translation) {
            runner.translation_batch_size()
        } else {
            1
        };
        if let Some(mask) = request.inpainting_mask.as_ref()
            && (!pages.contains(&mask.page) || !stages.contains(&Stage::Inpainting))
        {
            return Err(PipelineError::new(
                ErrorKind::InvalidInput,
                Some(Stage::Inpainting),
                anyhow::anyhow!("the inpainting mask page is outside the inpainting scope"),
            ));
        }

        if let Some(text_box) = request.text_box.as_ref() {
            let invalid = if request.inpainting_mask.is_some() {
                Some("a text box cannot be combined with an inpainting mask")
            } else if !pages.contains(&text_box.page) {
                Some("the text box page is outside the scope")
            } else if !stages.contains(&Stage::Detection) {
                Some("a text box needs the detection stage")
            } else if text_box.geometry.points.len() < 3 {
                Some("a text box needs at least three points")
            } else {
                None
            };
            if let Some(reason) = invalid {
                return Err(PipelineError::new(
                    ErrorKind::InvalidInput,
                    Some(Stage::Detection),
                    anyhow::anyhow!(reason),
                ));
            }
        }

        fn runner_is_finished(snapshot: &Snapshot, page: EntityId, stage: Stage) -> anyhow::Result<bool> {
            let entities = snapshot
                .descendants(page)?
                .map(|entity| entity.id())
                .collect::<Vec<_>>();
            match stage {
                Stage::Detection => {
                    let regions = entities
                        .iter()
                        .filter_map(|entity| {
                            snapshot
                                .component::<koharu_scene::Region>(*entity)
                                .ok()
                                .flatten()
                        })
                        .collect::<Vec<_>>();
                    Ok(!regions.is_empty()
                        && regions.iter().all(|region| {
                            matches!(region.origin, koharu_scene::Origin::Generated(_))
                        }))
                }
                Stage::Ocr => {
                    let text_regions = entities
                        .iter()
                        .filter(|entity| {
                            snapshot
                                .component::<koharu_scene::Region>(**entity)
                                .ok()
                                .flatten()
                                .is_some_and(|region| region.kind == koharu_scene::TextRegion::kind())
                        })
                        .copied()
                        .collect::<Vec<_>>();
                    Ok(text_regions.iter().all(|region| {
                        let ocr_generated = snapshot
                            .component::<koharu_scene::OcrAnalysis>(*region)
                            .ok()
                            .flatten()
                            .is_some_and(|analysis| {
                                matches!(analysis.origin, koharu_scene::Origin::Generated(_))
                            });
                        let source_generated = snapshot
                            .relations_to_as::<koharu_scene::RecognizedFrom>(*region)
                            .next()
                            .and_then(|relation| {
                                snapshot
                                    .component::<koharu_scene::SourceText>(relation.value().source)
                                    .ok()
                                    .flatten()
                            })
                            .is_some_and(|source| {
                                matches!(source.text.origin, koharu_scene::Origin::Generated(_))
                            });
                        ocr_generated && source_generated
                    }))
                }
                Stage::Translation => Ok(entities.iter().all(|entity| {
                    let Some(source) = snapshot
                        .component::<koharu_scene::SourceText>(*entity)
                        .ok()
                        .flatten()
                    else {
                        return true;
                    };
                    if !matches!(source.text.origin, koharu_scene::Origin::Generated(_))
                        || source.text.value.trim().is_empty()
                    {
                        return true;
                    }
                    snapshot
                        .component::<koharu_scene::Translation>(*entity)
                        .ok()
                        .flatten()
                        .is_some_and(|translation| {
                            matches!(translation.text.origin, koharu_scene::Origin::Generated(_))
                        })
                })),
                Stage::Inpainting => {
                    let cleanup = koharu_scene::AssetRole::new("source")?;
                    let has_bubble = entities.iter().any(|entity| {
                        snapshot
                            .component::<koharu_scene::Region>(*entity)
                            .ok()
                            .flatten()
                            .is_some_and(|region| {
                                region.kind == koharu_scene::BubbleRegion::kind()
                                    && matches!(region.origin, koharu_scene::Origin::Generated(_))
                            })
                    });
                    let has_cleanup = snapshot.children(page)?.any(|entity| {
                        snapshot
                            .component::<koharu_scene::RasterLayer>(entity)
                            .ok()
                            .flatten()
                            .is_some_and(|layer| {
                                layer.kind == koharu_scene::RasterLayerKind::Cleanup
                                    && matches!(layer.origin, koharu_scene::Origin::Generated(_))
                                    && snapshot.asset(entity, &cleanup).ok().flatten().is_some()
                            })
                    });
                    Ok(has_cleanup || !has_bubble)
                }
            }
        }
        progress::emit(
            request.progress.as_ref(),
            Progress::Started {
                pages: pages.clone(),
                stages: stages.clone(),
            },
        );

        Ok(Self {
            runner,
            resources,
            committer,
            stop: request.stop,
            progress: request.progress,
            scope,
            scheduler: Scheduler::new(&pages, &stages, translation_batch_size),
            scene: snapshot,
            images: BTreeMap::new(),
            busy_stages: BTreeSet::new(),
            completed: 0,
            failure: None,
            base,
            started,
            inpainting_mask: request.inpainting_mask,
            text_box: request.text_box,
        })
    }

    pub(crate) async fn run(mut self) -> std::result::Result<Report, PipelineError> {
        if self.stopped() {
            return Ok(self.report(RunStatus::Stopped));
        }

        self.resources.start();
        self.resources.wait_for_sample().await;

        let runner = self.runner.clone();
        let mut running = FuturesUnordered::new();
        loop {
            while let Some(job) = self.take_ready_job() {
                running.push(runner.run(job));
            }

            let Some(completion) = running.next().await else {
                break;
            };
            self.busy_stages.remove(&completion.stage);
            if self.stopped() || self.failure.is_some() {
                continue;
            }
            if let Err(error) = self.apply_completion(completion).await {
                self.failure = Some(error);
            }
        }

        self.finalize()
    }

    fn take_ready_job(&mut self) -> Option<StageJob> {
        if self.stopped() || self.failure.is_some() {
            return None;
        }
        let (pages, stage) = self.scheduler.start_next(&self.busy_stages)?;
        self.busy_stages.insert(stage);
        let inputs = pages
            .iter()
            .map(|page| {
                let images = self
                    .images
                    .entry(*page)
                    .or_insert_with(|| Arc::new(ImageCache::default()))
                    .clone();
                StageInput::new(
                    self.scene.clone(),
                    *page,
                    self.scope.entities(),
                    self.scope.region(*page),
                    images,
                    self.inpainting_mask
                        .as_ref()
                        .filter(|mask| stage == Stage::Inpainting && mask.page == *page)
                        .cloned(),
                    self.text_box
                        .as_ref()
                        .filter(|text_box| {
                            matches!(stage, Stage::Detection | Stage::Inpainting)
                                && text_box.page == *page
                        })
                        .cloned(),
                )
            })
            .collect();
        Some(StageJob::new(
            stage,
            inputs,
            self.stop.clone(),
            self.progress.clone(),
        ))
    }

    async fn apply_completion(
        &mut self,
        completion: StageCompletion,
    ) -> std::result::Result<(), PipelineError> {
        let StageCompletion {
            pages,
            stage,
            model,
            elapsed,
            outcome,
        } = completion;
        match outcome? {
            StageOutcome::Stopped => {}
            StageOutcome::Skipped => {
                for page in pages {
                    self.mark_complete(page, stage);
                    progress::emit(self.progress.as_ref(), Progress::Skipped { page, stage });
                }
            }
            StageOutcome::Patch(patch) => {
                if !self.commit_patch(&pages, stage, patch).await? {
                    return Ok(());
                }
                for page in pages {
                    self.mark_complete(page, stage);
                    progress::emit(
                        self.progress.as_ref(),
                        Progress::Finished {
                            page,
                            stage,
                            model: model.clone(),
                            elapsed,
                        },
                    );
                }
            }
        }
        Ok(())
    }

    async fn commit_patch(
        &mut self,
        pages: &[EntityId],
        stage: Stage,
        patch: koharu_scene::Patch,
    ) -> std::result::Result<bool, PipelineError> {
        let patch = patch
            .rebase_on(&self.scene)
            .and_then(|patch| {
                patch.validate_on(&self.scene)?;
                let label = if pages.len() == 1 {
                    format!("Pipeline {stage} for page {}", pages[0])
                } else {
                    format!("Pipeline {stage} for {} pages", pages.len())
                };
                Ok(patch.with_label(label))
            })
            .context("failed to rebase stage output onto the latest scene")
            .map_err(|error| PipelineError::new(ErrorKind::InvalidOutput, Some(stage), error))?;
        if self.stopped() {
            return Ok(false);
        }

        let next = self
            .committer
            .commit(StageOutput {
                page: pages[0],
                stage,
                patch,
            })
            .await
            .with_context(|| format!("failed to commit {stage} output for {} pages", pages.len()))
            .map_err(|error| PipelineError::new(ErrorKind::Commit, Some(stage), error))?;
        validate_commit(&self.scene, &next)
            .map_err(|error| PipelineError::new(ErrorKind::Commit, Some(stage), error))?;
        self.scene = next;
        Ok(true)
    }

    fn mark_complete(&mut self, page: EntityId, stage: Stage) {
        if self.scheduler.complete_stage(page, stage) {
            self.images.remove(&page);
        }
        self.completed += 1;
    }

    fn stopped(&self) -> bool {
        self.stop.stopped()
    }

    fn finalize(mut self) -> std::result::Result<Report, PipelineError> {
        if let Some(error) = self.failure.take() {
            return Err(error);
        }
        if !self.stopped() && self.completed != self.scheduler.total() {
            return Err(PipelineError::new(
                ErrorKind::InvalidOutput,
                None,
                anyhow::anyhow!(
                    "pipeline scheduler stopped after {} of {} work items",
                    self.completed,
                    self.scheduler.total()
                ),
            ));
        }
        let status = if self.stopped() {
            RunStatus::Stopped
        } else {
            RunStatus::Completed
        };
        Ok(self.report(status))
    }

    fn report(&self, status: RunStatus) -> Report {
        Report {
            status,
            base: self.base,
            final_revision: self.scene.revision(),
            completed: self.completed,
            total: self.scheduler.total(),
            elapsed: self.started.elapsed(),
        }
    }
}

fn validate_commit(previous: &Snapshot, next: &Snapshot) -> Result<()> {
    ensure!(
        previous.project_id() == next.project_id(),
        "committer returned a snapshot from another project"
    );
    ensure!(
        next.revision() > previous.revision(),
        "committer did not advance the scene revision"
    );
    Ok(())
}
