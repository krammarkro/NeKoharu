use std::{
    collections::{HashMap, HashSet},
    io::Cursor,
    ops::Range,
    path::PathBuf,
    sync::Arc,
};

use anyhow::{Context as _, Result, anyhow, bail};
use image::{DynamicImage, ImageFormat, RgbaImage};
use koharu_desktop::Frame;
use koharu_scene::{
    AssetInput, AssetMetadata, AssetRole, At, Authored, Commit, EntityId, EntityOrigin,
    Geometry as SceneGeometry, Group as SceneGroup, Origin, PageDraft, Point as ScenePoint,
    Presents, RasterLayer as SceneRasterLayer, RasterLayerKind, RecognizedFrom,
    Region as SceneRegion, RemovePolicy, Revision, Session, Snapshot,
    SourceText as SceneSourceText, TextGroup as SceneTextGroup, TextLayout as SceneTextLayout,
    TextLayoutKind, TextRegion, TextRole as SceneTextRole, Translation as SceneTranslation,
    Typography as SceneTypography, Visibility as SceneVisibility,
};
use serde::Serialize;
use specta::Type;
use tokio::sync::Mutex;

use super::{
    canvas::Point,
    editing::{GeometryUpdate, TypographyUpdate},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RasterStrokeMode {
    Paint,
    Erase,
}

#[derive(Clone, Debug, Serialize, Type)]
pub struct ProjectInfo {
    pub name: String,
    pub revision: Revision,
    pub active_page: Option<EntityId>,
    pub can_undo: bool,
    pub can_redo: bool,
}

/// A text layer whose translation contains the searched text.
#[derive(Clone, Debug, Serialize, Type)]
pub struct TranslationMatch {
    pub page: EntityId,
    /// The page's position in the project, from zero.
    pub page_index: u32,
    pub layer: EntityId,
    /// The layer's whole translation.
    pub text: String,
    /// How many times the searched text occurs in it.
    pub count: u32,
}

#[derive(Clone, Debug, Serialize, Type)]
pub struct ProjectSummary {
    pub name: String,
}

#[derive(Clone, Copy, Debug, Serialize, Type)]
pub struct PageSize {
    pub width: f64,
    pub height: f64,
}

#[derive(Clone, Debug, Serialize, Type)]
pub struct PageSummary {
    pub id: EntityId,
    pub label: String,
    pub size: PageSize,
    pub source_asset: Option<String>,
    pub chapter_start: bool,
    #[specta(type = f64)]
    pub layer_count: usize,
}

#[derive(Clone, Debug, Serialize, Type)]
pub struct Page {
    pub id: EntityId,
    pub label: String,
    pub size: PageSize,
    pub layers: Vec<Layer>,
    pub regions: Vec<AnalysisRegion>,
}

#[derive(Clone, Debug, Serialize, Type)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Layer {
    Group {
        id: EntityId,
        parent: Option<EntityId>,
        visibility: LayerVisibility,
        name: String,
        role: Option<GroupRole>,
    },
    Text {
        id: EntityId,
        parent: Option<EntityId>,
        geometry: Option<Geometry>,
        angle_degrees: Option<f32>,
        visibility: LayerVisibility,
        content: Box<TextContent>,
        typography: Option<Typography>,
        layout: TextLayoutKind,
        automatic_region: Option<EntityId>,
    },
    Raster {
        id: EntityId,
        parent: Option<EntityId>,
        visibility: LayerVisibility,
        image: Option<String>,
        name: String,
        kind: RasterLayerKind,
    },
    Image {
        id: EntityId,
        parent: Option<EntityId>,
        geometry: Geometry,
        visibility: LayerVisibility,
        image: String,
    },
    Artwork {
        id: EntityId,
        parent: Option<EntityId>,
        geometry: Geometry,
        visibility: LayerVisibility,
        image: String,
    },
}

#[derive(Clone, Copy, Debug, Serialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum GroupRole {
    Text,
}

#[derive(Clone, Debug, Serialize, Type)]
pub struct TextContent {
    pub id: EntityId,
    pub source: Option<SourceText>,
    pub translation: Option<Translation>,
    pub role: Option<String>,
    pub source_region: Option<EntityId>,
}

#[derive(Clone, Debug, Serialize, Type)]
pub struct AnalysisRegion {
    pub id: EntityId,
    pub parent: Option<EntityId>,
    pub geometry: Geometry,
    pub kind: String,
    pub label: Option<String>,
}

#[derive(Clone, Debug, Serialize, Type)]
pub struct Geometry {
    pub points: Vec<Point>,
}

#[derive(Clone, Copy, Debug, Serialize, Type)]
pub struct LayerVisibility {
    pub visible: bool,
    pub opacity: f32,
}

#[derive(Clone, Debug, Serialize, Type)]
pub struct SourceText {
    pub text: String,
    pub language: Option<String>,
}

#[derive(Clone, Debug, Serialize, Type)]
pub struct Translation {
    pub text: String,
    pub language: Option<String>,
}

#[derive(Clone, Debug, serde::Deserialize, Serialize, Type)]
pub struct Typography {
    pub preferred_font: Option<String>,
    pub font_weight: Option<u16>,
    pub font_style: Option<koharu_scene::FontStyle>,
    pub size: Option<f32>,
    pub auto_fit: bool,
    pub color: Option<[u8; 4]>,
    pub stroke_color: Option<[u8; 4]>,
    pub stroke_width: Option<f32>,
    pub stroke_method: Option<koharu_scene::StrokeMethod>,
    pub stroke_position: Option<koharu_scene::StrokePosition>,
    pub stroke_join: Option<koharu_scene::StrokeJoin>,
    pub outer_stroke_color: Option<[u8; 4]>,
    pub outer_stroke_width: Option<f32>,
    pub alignment: Option<koharu_scene::TextAlignment>,
    pub writing_mode: Option<koharu_scene::WritingMode>,
    pub line_spacing: Option<f32>,
}

pub(crate) struct CurrentProject {
    pub(crate) project: Mutex<Option<Project>>,
}

#[derive(Clone)]
pub(crate) struct ProjectLibrary {
    root: PathBuf,
}

impl ProjectLibrary {
    pub(crate) fn new() -> Result<Self> {
        let root = dirs::document_dir()
            .context("the Documents directory is unavailable")?
            .join("Koharu");
        std::fs::create_dir_all(&root)
            .with_context(|| format!("failed to create {}", root.display()))?;
        Ok(Self { root })
    }

    pub(crate) fn list(&self) -> Result<Vec<ProjectSummary>> {
        let mut projects = std::fs::read_dir(&self.root)
            .with_context(|| format!("failed to read {}", self.root.display()))?
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .filter_map(|entry| {
                let path = entry.path();
                let is_project_directory = path
                    .extension()
                    .and_then(|extension| extension.to_str())
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("khrproj"))
                    && (path.join("state-a.khr").is_file() || path.join("state-b.khr").is_file());
                if !is_project_directory {
                    return None;
                }
                let last_used = ["state-a.khr", "state-b.khr"]
                    .into_iter()
                    .filter_map(|file| std::fs::metadata(path.join(file)).ok()?.modified().ok())
                    .max()
                    .unwrap_or(std::time::UNIX_EPOCH);
                Some((
                    last_used,
                    ProjectSummary {
                        name: path.file_stem()?.to_str()?.to_owned(),
                    },
                ))
            })
            .collect::<Vec<_>>();
        projects.sort_unstable_by(|(left_used, left), (right_used, right)| {
            right_used
                .cmp(left_used)
                .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
        });
        Ok(projects.into_iter().map(|(_, project)| project).collect())
    }

    pub(crate) async fn create(&self, name: &str) -> Result<Project> {
        let (name, path) = self.resolve(name)?;
        Project::create(name, path).await
    }

    pub(crate) async fn open(&self, name: &str) -> Result<Project> {
        let (name, path) = self.resolve(name)?;
        Project::open(name, path).await
    }

    pub(crate) fn delete(&self, name: &str) -> Result<()> {
        let (_, path) = self.resolve(name)?;
        if !path.is_dir() {
            bail!("project {name:?} does not exist");
        }
        std::fs::remove_dir_all(&path)
            .with_context(|| format!("failed to delete {}", path.display()))
    }

    fn resolve(&self, name: &str) -> Result<(String, PathBuf)> {
        let name = validate_project_name(name)?;
        Ok((name.clone(), self.root.join(format!("{name}.khrproj"))))
    }
}

/// Where a commit goes once it is built.
#[derive(Clone, Default)]
pub(crate) enum ProjectMode {
    /// Ordinary project: the files on disk are the record.
    #[default]
    Local,
    /// This machine owns the files and republishes every commit so guests can
    /// follow them.
    Host(koharu_session::Broadcaster),
    /// Another machine owns the files; commits travel there and come back as
    /// the host's own revision.
    Guest(koharu_session::Guest),
}

impl ProjectMode {
    /// Tells the other participants which page this instance has open.
    fn present(&self, page: Option<EntityId>) {
        match self {
            Self::Local => {}
            Self::Host(broadcaster) => broadcaster.present(super::session::presence(page)),
            Self::Guest(guest) => guest.present(super::session::presence(page)),
        }
    }
}

/// Sends an edit to the session host and adopts the commit it comes back as.
/// Everything the host sent before that edit is applied first, so the replica
/// follows the host's order exactly.
async fn commit_on_host(
    session: &mut Session,
    guest: &koharu_session::Guest,
    patch: koharu_scene::Patch,
) -> Result<Commit> {
    let encoded = patch.encode()?;
    let sequence = guest
        .send(encoded)
        .await
        .map_err(|reason| anyhow!("{reason}"))?;
    let mut landed: Option<Commit> = None;
    loop {
        let inbound = tokio::time::timeout(std::time::Duration::from_secs(60), guest.next())
            .await
            .map_err(|_| anyhow!("the session host did not answer in time"))?
            .map_err(|reason| {
                guest.request_resync();
                anyhow!("{reason}")
            })?;
        let Some(inbound) = inbound else {
            return Err(anyhow!("the session ended before your edit was saved"));
        };
        match inbound {
            koharu_session::Inbound::Commit { revision, patch } => {
                landed = apply_host_commit(session, revision, &patch).await?;
            }
            koharu_session::Inbound::Ack { seq, revision } if seq == sequence => {
                return landed
                    .filter(|commit| commit.revision == Revision::new(revision))
                    .ok_or_else(|| anyhow!("the host did not confirm your edit"));
            }
            koharu_session::Inbound::Rejected { seq, reason, .. } if seq == sequence => {
                guest.request_resync();
                return Err(anyhow!("{reason}"));
            }
            _ => {}
        }
    }
}

/// Applies one commit the host announced, skipping one already reflected here.
async fn apply_host_commit(
    session: &mut Session,
    revision: u64,
    patch: &[u8],
) -> Result<Option<Commit>> {
    let revision = Revision::new(revision);
    let snapshot = session.snapshot();
    if revision <= snapshot.revision() {
        return Ok(None);
    }
    let patch = koharu_scene::Patch::decode(patch, &snapshot)?;
    Ok(Some(session.commit_remote(patch, revision).await?))
}

pub(crate) struct Project {
    pub(crate) session: Session,
    pub(crate) name: String,
    pub(crate) active_page: Option<EntityId>,
    pub(crate) undo: Vec<Vec<Revision>>,
    pub(crate) redo: Vec<Vec<Revision>>,
    pub(crate) mode: ProjectMode,
    /// Where the page last viewed is remembered, for a project in the library.
    view: Option<PathBuf>,
}

/// The page a project was last viewed at. It is view state rather than project
/// content, so it sits beside the stored states instead of in the scene, where
/// changing it would be an undoable edit shared with every guest.
#[derive(Serialize, serde::Deserialize)]
struct View {
    active_page: EntityId,
}

const VIEW_FILE: &str = "view.json";

impl Project {
    pub(crate) async fn create(name: String, path: PathBuf) -> Result<Self> {
        let session = Session::create(&path)
            .await
            .with_context(|| format!("failed to create {}", path.display()))?;
        Ok(Self::new(session, name).remembering_view(path.join(VIEW_FILE)))
    }

    pub(crate) async fn open(name: String, path: PathBuf) -> Result<Self> {
        let session = Session::open(&path)
            .await
            .with_context(|| format!("failed to open {}", path.display()))?;
        Ok(Self::new(session, name).remembering_view(path.join(VIEW_FILE)))
    }

    pub(crate) fn new(session: Session, name: String) -> Self {
        let active_page = session.snapshot().pages().next().map(|page| page.id());
        Self {
            session,
            name,
            active_page,
            undo: Vec::new(),
            redo: Vec::new(),
            mode: ProjectMode::Local,
            view: None,
        }
    }

    /// Reopens at the page remembered in `view` and keeps it current from now
    /// on. A missing, unreadable, or stale record leaves the first page.
    fn remembering_view(mut self, view: PathBuf) -> Self {
        let remembered = std::fs::read(&view)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<View>(&bytes).ok());
        if let Some(View { active_page }) = remembered
            && self.snapshot().page(active_page).is_ok()
        {
            self.active_page = Some(active_page);
        }
        self.view = Some(view);
        self
    }

    #[must_use]
    pub(crate) fn with_mode(mut self, mode: ProjectMode) -> Self {
        self.set_mode(mode);
        self
    }

    pub(crate) fn set_mode(&mut self, mode: ProjectMode) {
        self.mode = mode;
        self.present();
    }

    /// Tells the other participants, if any, who this is and where.
    pub(crate) fn present(&self) {
        self.mode.present(self.active_page);
    }

    /// Swaps in a freshly fetched copy of the project, dropping local history.
    pub(crate) fn reload(&mut self, session: Session) {
        self.session = session;
        self.undo.clear();
        self.redo.clear();
        self.reconcile_page();
    }

    pub(crate) fn snapshot(&self) -> Snapshot {
        self.session.snapshot()
    }

    pub(crate) fn revision(&self) -> Revision {
        self.snapshot().revision()
    }

    pub(crate) fn active_page(&self) -> Option<EntityId> {
        self.active_page
    }

    pub(crate) fn select_page(&mut self, page: EntityId) -> Result<()> {
        self.snapshot().page(page)?;
        self.active_page = Some(page);
        self.present();
        if let Some(view) = &self.view {
            let saved = serde_json::to_vec(&View { active_page: page })
                .map_err(anyhow::Error::from)
                .and_then(|bytes| std::fs::write(view, bytes).map_err(Into::into));
            if let Err(error) = saved {
                tracing::warn!(%error, "failed to remember the viewed page");
            }
        }
        Ok(())
    }

    pub(crate) fn reconcile_page(&mut self) {
        let snapshot = self.snapshot();
        if self
            .active_page
            .is_none_or(|page| snapshot.page(page).is_err())
        {
            self.active_page = snapshot.pages().next().map(|page| page.id());
            self.present();
        }
    }

    pub(crate) fn info(&self) -> ProjectInfo {
        ProjectInfo {
            name: self.name.clone(),
            revision: self.revision(),
            active_page: self.active_page,
            can_undo: !self.undo.is_empty(),
            can_redo: !self.redo.is_empty(),
        }
    }

    pub(crate) fn pages(snapshot: &Snapshot) -> Result<Vec<PageSummary>> {
        snapshot
            .pages()
            .map(|page| {
                let value = page.page()?;
                let source_asset = Self::asset_id(snapshot, page.id(), "source")?;
                let layer_count = snapshot
                    .descendants(page.id())?
                    .map(|entity| Self::is_content_layer(snapshot, entity.id()))
                    .collect::<Result<Vec<_>>>()?
                    .into_iter()
                    .filter(|present| *present)
                    .count()
                    + usize::from(source_asset.is_some());
                Ok(PageSummary {
                    id: page.id(),
                    label: value.label,
                    size: PageSize {
                        width: value.width,
                        height: value.height,
                    },
                    source_asset,
                    chapter_start: value.chapter_start,
                    layer_count,
                })
            })
            .collect()
    }

    pub(crate) async fn rename_page(&mut self, page: EntityId, label: String) -> Result<Commit> {
        let snapshot = self.snapshot();
        let current = snapshot.page(page)?.page()?;
        let patch = snapshot.patch(|edit| {
            edit.set_page(
                page,
                PageDraft::new(label, current.width, current.height)
                    .with_chapter_start(current.chapter_start),
            )
        })?;
        self.commit(patch).await
    }

    pub(crate) async fn set_page_chapter_start(
        &mut self,
        page: EntityId,
        chapter_start: bool,
    ) -> Result<Commit> {
        let snapshot = self.snapshot();
        let current = snapshot.page(page)?.page()?;
        let patch = snapshot.patch(|edit| {
            edit.set_page(
                page,
                PageDraft::new(current.label, current.width, current.height)
                    .with_chapter_start(chapter_start),
            )
        })?;
        self.commit(patch).await
    }

    pub(crate) async fn delete_pages(&mut self, pages: Vec<EntityId>) -> Result<Commit> {
        let snapshot = self.snapshot();
        let pages = Self::unique_roots(&snapshot, pages)?;
        let patch = snapshot.patch(|edit| {
            for page in pages {
                edit.remove_entity(page, RemovePolicy::Cascade)?;
            }
            Ok(())
        })?;
        self.commit(patch).await
    }

    pub(crate) async fn move_page(&mut self, page: EntityId, index: usize) -> Result<Commit> {
        let snapshot = self.snapshot();
        let siblings = snapshot.pages().map(|page| page.id()).collect::<Vec<_>>();
        let at = Self::placement(&siblings, page, index);
        let patch = snapshot.patch(|edit| edit.move_entity(page, None, at))?;
        self.commit(patch).await
    }

    /// Combines an adjacent run of pages into one wide spread page. NeKoharu
    /// diverges from upstream here: manga reads right to left, so the run's
    /// first page lands on the right half of the spread and every later page is
    /// placed to its left. Source and cleanup pixels follow those offsets while
    /// every child layer, text group, translation, and analysis region is
    /// re-parented under the merged page with the offset pages translated by the
    /// accumulated width of the pages to their right. Relations are document
    /// scoped, so moving the entities keeps `Presents`, `RecognizedFrom`,
    /// `FitsTo`, `FlowsIn`, and `Inside` intact rather than rebuilding text
    /// content from scratch. When `delete_originals` is false the source pages
    /// stay in the project as image-only pages, their layers having moved to the
    /// spread.
    pub(crate) async fn merge_pages(
        &mut self,
        pages: Vec<EntityId>,
        delete_originals: bool,
    ) -> Result<(Commit, EntityId)> {
        let snapshot = self.snapshot();
        let order = snapshot.pages().map(|page| page.id()).collect::<Vec<_>>();
        let pages = Self::contiguous_run(&order, pages)?;
        let source_role = AssetRole::new("source")?;

        // Read every page-sized image before building the patch, because blob
        // reads are asynchronous while the scene edit itself is synchronous.
        let mut offsets = Vec::with_capacity(pages.len());
        let mut widths = Vec::with_capacity(pages.len());
        let mut source_images = Vec::with_capacity(pages.len());
        let mut total_width = 0u32;
        let mut total_height = 0u32;
        for &page in &pages {
            let value = snapshot.page(page)?.page()?;
            let width = value.width.round().max(1.0) as u32;
            let height = value.height.round().max(1.0) as u32;
            widths.push(width);
            offsets.push(total_width);
            total_width = total_width
                .checked_add(width)
                .context("the merged page is too wide")?;
            total_height = total_height.max(height);
            let asset = snapshot
                .asset(page, &source_role)?
                .with_context(|| format!("page {page} has no source image to merge"))?;
            source_images.push(snapshot.read_blob(asset.blob).await?);
        }
        // Manga reads right to left: each page sits at the far right of the
        // pages that follow it, which is the total width minus the end of its
        // original left-to-right slot.
        offsets = offsets
            .into_iter()
            .zip(&widths)
            .map(|(offset, &width)| total_width - offset - width)
            .collect();
        if total_width > MAX_MERGE_DIMENSION || total_height > MAX_MERGE_DIMENSION {
            bail!("the merged page is larger than Koharu supports");
        }

        // Cleanup and paint layers are full-page pixel layers, so the renderer
        // stretches them across the spread. Re-pad each to the merged width so its
        // pixels stay in the correct half.
        let mut raster_blobs = Vec::new();
        for (index, &page) in pages.iter().enumerate() {
            for child in snapshot.children(page)? {
                if snapshot.component::<SceneRasterLayer>(child)?.is_none() {
                    continue;
                }
                if let Some(asset) = snapshot.asset(child, &source_role)? {
                    raster_blobs.push((
                        child,
                        offsets[index],
                        snapshot.read_blob(asset.blob).await?,
                    ));
                }
            }
        }

        let compose_offsets = offsets.clone();
        let (merged_source, raster_assets) =
            tokio::task::spawn_blocking(move || -> Result<MergedSpread> {
                let decoded = source_images
                    .iter()
                    .map(|bytes| Ok(image::load_from_memory(bytes)?.to_rgba8()))
                    .collect::<Result<Vec<_>>>()?;
                let merged = compose_row(
                    decoded
                        .iter()
                        .zip(&compose_offsets)
                        .map(|(image, offset)| (i64::from(*offset), image)),
                    total_width,
                    total_height,
                );
                let merged_source = encode_png(merged)?;
                let mut rasters = HashMap::new();
                for (layer, offset, bytes) in raster_blobs {
                    let image = image::load_from_memory(&bytes)?.to_rgba8();
                    let padded = compose_row(
                        std::iter::once((i64::from(offset), &image)),
                        total_width,
                        total_height,
                    );
                    rasters.insert(layer, encode_png(padded)?);
                }
                Ok((merged_source, rasters))
            })
            .await??;

        let first = pages[0];
        let first_page = snapshot.page(first)?.page()?;
        let label = first_page.label;
        let chapter_start = first_page.chapter_start;
        let mut target_group = None;
        for &page in &pages {
            if let Some(group) = snapshot.page(page)?.text_group()? {
                target_group = Some(group.id());
                break;
            }
        }

        let width = f64::from(total_width);
        let height = f64::from(total_height);
        let mut merged_page = None;
        let patch = snapshot.patch(|edit| {
            let merged = edit.add_page(
                PageDraft::new(label.clone(), width, height).with_chapter_start(chapter_start),
                At::Before(first),
            )?;
            merged_page = Some(merged);
            edit.set_asset(
                merged,
                &source_role,
                AssetInput::new(
                    Arc::clone(&merged_source),
                    "image/png",
                    AssetMetadata {
                        width: Some(total_width),
                        height: Some(total_height),
                        attributes: Default::default(),
                    },
                ),
            )?;

            // Adopt one source page's text group as the merged page's single text
            // group; a page may only own one, so the others are emptied instead.
            if let Some(group) = target_group {
                edit.move_entity(group, Some(merged), At::End)?;
            }

            let mut roots = Vec::new();
            for (&page, &offset) in pages.iter().zip(&offsets) {
                for child in snapshot.children(page)? {
                    if Some(child) == target_group {
                        continue;
                    }
                    if snapshot.component::<SceneTextGroup>(child)?.is_some() {
                        let group = target_group
                            .expect("a merged text group exists when text layers are present");
                        for layer in snapshot.children(child)? {
                            roots.push((layer, group, f64::from(offset)));
                        }
                    } else {
                        roots.push((child, merged, f64::from(offset)));
                    }
                }
            }
            for (root, parent, offset) in roots {
                edit.move_entity(root, Some(parent), At::End)?;
                for entity in snapshot.subtree(root)? {
                    let id = entity.id();
                    if let Some(asset) = raster_assets.get(&id) {
                        edit.set_asset(
                            id,
                            &source_role,
                            AssetInput::new(
                                Arc::clone(asset),
                                "image/png",
                                AssetMetadata {
                                    width: Some(total_width),
                                    height: Some(total_height),
                                    attributes: Default::default(),
                                },
                            ),
                        )?;
                    }
                    if offset == 0.0 {
                        continue;
                    }
                    if let Some(geometry) = snapshot.component::<SceneGeometry>(id)? {
                        edit.set(
                            id,
                            &SceneGeometry {
                                origin: geometry.origin,
                                points: geometry
                                    .points
                                    .into_iter()
                                    .map(|point| ScenePoint {
                                        x: point.x + offset,
                                        y: point.y,
                                    })
                                    .collect(),
                            },
                        )?;
                    }
                }
            }

            if delete_originals {
                for &page in &pages {
                    edit.remove_entity(page, RemovePolicy::Cascade)?;
                }
            }
            Ok(())
        })?;
        let merged = merged_page.context("the merged page was not created")?;
        let commit = self.commit(patch).await?;
        Ok((commit, merged))
    }

    /// Selects a run of unique, in-project pages, requires them to be adjacent in
    /// reading order, and returns them left to right.
    fn contiguous_run(order: &[EntityId], pages: Vec<EntityId>) -> Result<Vec<EntityId>> {
        let mut selected = pages;
        selected.sort_unstable();
        selected.dedup();
        if selected.len() < 2 {
            bail!("merging a spread needs at least two pages");
        }
        let mut indices = Vec::with_capacity(selected.len());
        for page in &selected {
            let index = order
                .iter()
                .position(|candidate| candidate == page)
                .with_context(|| format!("page {page} does not belong to this project"))?;
            indices.push(index);
        }
        indices.sort_unstable();
        if indices.windows(2).any(|window| window[1] != window[0] + 1) {
            bail!("only adjacent pages can be merged into a spread");
        }
        Ok(indices.into_iter().map(|index| order[index]).collect())
    }

    pub(crate) async fn add_point_text(
        &mut self,
        page: EntityId,
        point: Point,
    ) -> Result<(Commit, EntityId)> {
        if !point.x.is_finite() || !point.y.is_finite() {
            bail!("text position must contain finite coordinates");
        }
        let value = self.snapshot().page(page)?.page()?;
        if point.x < 0.0 || point.y < 0.0 || point.x >= value.width || point.y >= value.height {
            bail!("text position must be inside the page");
        }
        self.add_text(
            page,
            Frame {
                x: point.x as f32,
                y: point.y as f32,
                width: (value.width - point.x).clamp(1.0, 320.0) as f32,
                height: (value.height - point.y).clamp(1.0, 120.0) as f32,
                angle_degrees: 0.0,
            },
            TextLayoutKind::Point,
        )
        .await
    }

    pub(crate) async fn add_text_box(
        &mut self,
        page: EntityId,
        frame: Frame,
    ) -> Result<(Commit, EntityId)> {
        self.add_text(page, frame, TextLayoutKind::Paragraph).await
    }

    /// Creates a user-drawn text region and its presentation layer. OCR runs
    /// against the region's `RecognizedFrom` content, so the caller receives the
    /// region and layer to scope the pipeline to this box alone.
    pub(crate) async fn add_ocr_box(
        &mut self,
        page: EntityId,
        frame: Frame,
    ) -> Result<(Commit, EntityId, EntityId)> {
        let snapshot = self.snapshot();
        let geometry = Self::geometry_from_frame(frame)?;
        let mut layer = None;
        let mut region = None;
        let patch = snapshot.patch(|edit| {
            let added_region =
                edit.add_analysis_region::<TextRegion>(page, At::End, &geometry, None)?;
            let content = edit.add_text_content(page, At::End)?;
            edit.relate::<RecognizedFrom>(content, added_region)?;
            let added_layer = edit.add_text_layer(
                page,
                At::End,
                content,
                &SceneTextLayout {
                    origin: Origin::User,
                    kind: TextLayoutKind::Paragraph,
                    angle_degrees: Some(frame.angle_degrees),
                },
            )?;
            edit.set(added_layer, &geometry)?;
            edit.set(
                added_layer,
                &SceneTypography {
                    origin: Origin::User,
                    preferred_font: None,
                    font_weight: None,
                    font_style: None,
                    size: None,
                    auto_fit: true,
                    color: None,
                    stroke_color: None,
                    stroke_width: None,
                    stroke_method: Some(koharu_scene::StrokeMethod::Contour),
                    stroke_position: None,
                    stroke_join: None,
                    outer_stroke_color: None,
                    outer_stroke_width: None,
                    alignment: None,
                    writing_mode: None,
                    extensions: Default::default(),
                    line_spacing: None,
                },
            )?;
            layer = Some(added_layer);
            region = Some(added_region);
            Ok(())
        })?;
        Ok((
            self.commit(patch).await?,
            layer.expect("OCR box layer was added while building the patch"),
            region.expect("OCR box region was added while building the patch"),
        ))
    }

    pub(crate) async fn paste_text_layers(
        &mut self,
        page: EntityId,
        layers: Vec<EntityId>,
    ) -> Result<(Commit, Vec<EntityId>)> {
        let snapshot = self.snapshot();
        snapshot.page(page)?;
        let mut seen = HashSet::new();
        let layers = layers
            .into_iter()
            .filter(|layer| seen.insert(*layer))
            .collect::<Vec<_>>();
        if layers.is_empty() {
            bail!("at least one text layer is required");
        }

        let mut added = Vec::with_capacity(layers.len());
        let patch = snapshot.patch(|edit| {
            for source_layer in &layers {
                let source = snapshot.text_layer(*source_layer)?;
                let content = source.content()?;
                let mut layout = source.layout()?;
                layout.origin = Origin::User;
                let mut geometry = source.frame()?;
                if let Some(geometry) = &mut geometry {
                    geometry.origin = Origin::User;
                }
                let mut typography = source.typography()?;
                if let Some(typography) = &mut typography {
                    if !matches!(typography.origin, Origin::User) {
                        // A pipeline-inferred direction describes the original's
                        // source text; the user-owned copy must render by its own
                        // text instead of inheriting it as an authored override.
                        typography.writing_mode = None;
                    }
                    typography.origin = Origin::User;
                }
                let mut visibility = source.visibility()?.unwrap_or(SceneVisibility {
                    origin: Origin::User,
                    visible: true,
                    opacity: 1.0,
                });
                visibility.origin = Origin::User;
                let source_text = content.source()?;
                let translation = content.translation()?;
                let role = content.role()?;

                let copied_content = edit.add_text_content(page, At::End)?;
                if let Some(mut source_text) = source_text {
                    source_text.text.origin = Origin::User;
                    edit.set(copied_content, &source_text)?;
                }
                if let Some(mut translation) = translation {
                    translation.text.origin = Origin::User;
                    edit.set(copied_content, &translation)?;
                }
                if let Some(role) = role {
                    edit.set(
                        copied_content,
                        &SceneTextRole {
                            origin: Origin::User,
                            role: role.role,
                        },
                    )?;
                }
                let copied_layer = edit.add_text_layer(page, At::End, copied_content, &layout)?;
                if let Some(geometry) = geometry {
                    edit.set(copied_layer, &geometry)?;
                }
                if let Some(typography) = typography {
                    edit.set(copied_layer, &typography)?;
                }
                edit.set(copied_layer, &visibility)?;
                added.push(copied_layer);
            }
            Ok(())
        })?;
        Ok((self.commit(patch).await?, added))
    }

    async fn add_text(
        &mut self,
        page: EntityId,
        frame: Frame,
        kind: TextLayoutKind,
    ) -> Result<(Commit, EntityId)> {
        let snapshot = self.snapshot();
        let geometry = Self::geometry_from_frame(frame)?;
        let mut layer = None;
        let patch = snapshot.patch(|edit| {
            let content = edit.add_text_content(page, At::End)?;
            edit.set(
                content,
                &SceneSourceText {
                    text: Authored::user(String::new()),
                    language: None,
                },
            )?;
            let added_layer = edit.add_text_layer(
                page,
                At::End,
                content,
                &SceneTextLayout {
                    origin: Origin::User,
                    kind,
                    angle_degrees: Some(frame.angle_degrees),
                },
            )?;
            layer = Some(added_layer);
            edit.set(added_layer, &geometry)?;
            edit.set(
                added_layer,
                &SceneTypography {
                    origin: Origin::User,
                    preferred_font: None,
                    font_weight: None,
                    font_style: None,
                    size: None,
                    auto_fit: true,
                    color: None,
                    stroke_color: None,
                    stroke_width: None,
                    stroke_method: Some(koharu_scene::StrokeMethod::Contour),
                    stroke_position: None,
                    stroke_join: None,
                    outer_stroke_color: None,
                    outer_stroke_width: None,
                    alignment: match kind {
                        TextLayoutKind::Point => Some(koharu_scene::TextAlignment::Start),
                        TextLayoutKind::Paragraph => None,
                    },
                    writing_mode: None,
                    extensions: Default::default(),
                    line_spacing: None,
                },
            )?;
            Ok(())
        })?;
        Ok((
            self.commit(patch).await?,
            layer.expect("text layer was added while building the patch"),
        ))
    }

    pub(crate) async fn set_source_text(
        &mut self,
        layer: EntityId,
        text: String,
    ) -> Result<Commit> {
        let snapshot = self.snapshot();
        let content = Self::text_content(&snapshot, layer)?;
        let language = snapshot
            .component::<SceneSourceText>(content)?
            .and_then(|source| source.language);
        let patch = snapshot.patch(|edit| {
            edit.promote_entity_to_user(layer)?;
            edit.promote_entity_to_user(content)?;
            edit.set(
                content,
                &SceneSourceText {
                    text: Authored::user(text),
                    language,
                },
            )
        })?;
        self.commit(patch).await
    }

    pub(crate) async fn set_translation(
        &mut self,
        layer: EntityId,
        text: Option<String>,
    ) -> Result<Commit> {
        let snapshot = self.snapshot();
        let content = Self::text_content(&snapshot, layer)?;
        let language = snapshot
            .component::<SceneTranslation>(content)?
            .and_then(|translation| translation.language);
        let patch = snapshot.patch(|edit| {
            edit.promote_entity_to_user(layer)?;
            edit.promote_entity_to_user(content)?;
            match text {
                Some(text) => edit.set(
                    content,
                    &SceneTranslation {
                        text: Authored::user(text),
                        language,
                    },
                ),
                None => edit.remove::<SceneTranslation>(content),
            }
        })?;
        self.commit(patch).await
    }

    /// The text layers whose translation contains `query`, in project order.
    pub(crate) fn find_translations(
        &self,
        query: &str,
        match_case: bool,
    ) -> Result<Vec<TranslationMatch>> {
        let snapshot = self.snapshot();
        let mut matches = Vec::new();
        for (page_index, page) in snapshot.pages().enumerate() {
            let Some(group) = page.text_group()? else {
                continue;
            };
            for layer in group.text_layers()? {
                let Some(translation) = layer.content()?.translation()? else {
                    continue;
                };
                let count = find_ranges(&translation.text.value, query, match_case).len();
                if count > 0 {
                    matches.push(TranslationMatch {
                        page: page.id(),
                        page_index: u32::try_from(page_index)?,
                        layer: layer.id(),
                        text: translation.text.value,
                        count: u32::try_from(count)?,
                    });
                }
            }
        }
        Ok(matches)
    }

    /// Replaces `query` in translations, everywhere or only in `layers`, as
    /// one undo step, and returns how many occurrences were replaced.
    pub(crate) async fn replace_translations(
        &mut self,
        query: &str,
        replacement: &str,
        match_case: bool,
        layers: Option<&[EntityId]>,
    ) -> Result<(Commit, u32)> {
        let snapshot = self.snapshot();
        let mut updates = Vec::new();
        let mut count = 0;
        for found in self.find_translations(query, match_case)? {
            if layers.is_some_and(|layers| !layers.contains(&found.layer)) {
                continue;
            }
            let content = Self::text_content(&snapshot, found.layer)?;
            let language = snapshot
                .component::<SceneTranslation>(content)?
                .and_then(|translation| translation.language);
            let ranges = find_ranges(&found.text, query, match_case);
            count += found.count;
            updates.push((
                found.layer,
                content,
                language,
                replace_ranges(&found.text, &ranges, replacement),
            ));
        }
        if updates.is_empty() {
            bail!("no translation contains that text");
        }
        let patch = snapshot.patch(|edit| {
            for (layer, content, language, text) in updates {
                edit.promote_entity_to_user(layer)?;
                edit.promote_entity_to_user(content)?;
                edit.set(
                    content,
                    &SceneTranslation {
                        text: Authored::user(text),
                        language,
                    },
                )?;
            }
            Ok(())
        })?;
        Ok((self.commit(patch).await?, count))
    }

    pub(crate) async fn set_typography(
        &mut self,
        updates: Vec<TypographyUpdate>,
    ) -> Result<Commit> {
        let snapshot = self.snapshot();
        let updates = updates
            .into_iter()
            .map(|update| {
                let content = Self::text_content(&snapshot, update.layer)?;
                Ok((update, content))
            })
            .collect::<Result<Vec<_>>>()?;
        let patch = snapshot.patch(|edit| {
            for (update, content) in updates {
                edit.promote_entity_to_user(update.layer)?;
                edit.promote_entity_to_user(content)?;
                edit.set(
                    update.layer,
                    &SceneTypography {
                        origin: Origin::User,
                        preferred_font: update.typography.preferred_font,
                        font_weight: update.typography.font_weight,
                        font_style: update.typography.font_style,
                        size: update.typography.size.filter(|size| *size > 0.0),
                        auto_fit: update.typography.auto_fit,
                        color: update.typography.color,
                        stroke_color: update.typography.stroke_color,
                        stroke_width: update.typography.stroke_width,
                        stroke_method: update.typography.stroke_method,
                        stroke_position: update.typography.stroke_position,
                        stroke_join: update.typography.stroke_join,
                        outer_stroke_color: update.typography.outer_stroke_color,
                        outer_stroke_width: update
                            .typography
                            .outer_stroke_width
                            .filter(|width| *width > 0.0),
                        alignment: update.typography.alignment,
                        writing_mode: update.typography.writing_mode,
                        extensions: Default::default(),
                        line_spacing: update.typography.line_spacing,
                    },
                )?;
            }
            Ok(())
        })?;
        self.commit(patch).await
    }

    pub(crate) async fn set_geometry(&mut self, updates: Vec<GeometryUpdate>) -> Result<Commit> {
        let snapshot = self.snapshot();
        let updates = updates
            .into_iter()
            .map(|update| {
                let layout = snapshot
                    .component::<SceneTextLayout>(update.layer)?
                    .context("only text layers can change geometry")?;
                let content = Self::text_content(&snapshot, update.layer)?;
                if update.points.is_none()
                    && snapshot
                        .text_layer(update.layer)?
                        .automatic_target()?
                        .is_none()
                {
                    bail!("only automatically placed text can reset its geometry");
                }
                Ok((update, content, layout))
            })
            .collect::<Result<Vec<_>>>()?;
        let patch = snapshot.patch(|edit| {
            for (update, content, mut layout) in updates {
                edit.promote_entity_to_user(update.layer)?;
                edit.promote_entity_to_user(content)?;
                layout.origin = Origin::User;
                layout.angle_degrees = None;
                edit.set(update.layer, &layout)?;
                match update.points {
                    Some(points) => edit.set(
                        update.layer,
                        &SceneGeometry {
                            origin: Origin::User,
                            points: points
                                .into_iter()
                                .map(|point| ScenePoint {
                                    x: point.x,
                                    y: point.y,
                                })
                                .collect(),
                        },
                    )?,
                    None => edit.remove::<SceneGeometry>(update.layer)?,
                }
            }
            Ok(())
        })?;
        self.commit(patch).await
    }

    pub(crate) async fn set_visibility(
        &mut self,
        layers: Vec<EntityId>,
        visible: Option<bool>,
        opacity: Option<f32>,
    ) -> Result<Commit> {
        let snapshot = self.snapshot();
        let patch = snapshot.patch(|edit| {
            for layer in layers {
                let mut value =
                    snapshot
                        .component::<SceneVisibility>(layer)?
                        .unwrap_or(SceneVisibility {
                            origin: Origin::User,
                            visible: true,
                            opacity: 1.0,
                        });
                if let Some(visible) = visible {
                    value.visible = visible;
                }
                if let Some(opacity) = opacity {
                    value.opacity = opacity;
                }
                value.origin = Origin::User;
                edit.set(layer, &value)?;
            }
            Ok(())
        })?;
        self.commit(patch).await
    }

    pub(crate) async fn delete_layers(&mut self, layers: Vec<EntityId>) -> Result<Commit> {
        let snapshot = self.snapshot();
        let mut expanded = Vec::new();
        for layer in layers {
            if snapshot.component::<SceneTextGroup>(layer)?.is_some() {
                expanded.extend(snapshot.children(layer)?);
            } else {
                expanded.push(layer);
            }
        }
        let layers = Self::unique_roots(&snapshot, expanded)?;
        let mut orphaned_contents = Vec::new();
        for layer in &layers {
            let Some(relation) = snapshot.relation_from::<Presents>(*layer)? else {
                continue;
            };
            let content = relation.value().target;
            if snapshot.relations_to_as::<Presents>(content).count() == 1 {
                orphaned_contents.push(content);
            }
        }
        let patch = snapshot.patch(|edit| {
            for layer in layers {
                if snapshot.page(layer).is_ok() {
                    return Err(koharu_scene::Error::Invalid(
                        "delete pages with delete_pages".to_owned(),
                    ));
                }
                edit.remove_entity(layer, RemovePolicy::Cascade)?;
            }
            for content in orphaned_contents {
                if snapshot.entity(content).is_ok() {
                    edit.remove_entity(content, RemovePolicy::Cascade)?;
                }
            }
            Ok(())
        })?;
        self.commit(patch).await
    }

    #[tracing::instrument(level = "info", skip_all, fields(layer = %layer, parent = %parent))]
    pub(crate) async fn move_layer(
        &mut self,
        layer: EntityId,
        parent: EntityId,
        index: usize,
    ) -> Result<Commit> {
        let snapshot = self.snapshot();
        let siblings = snapshot
            .children(parent)?
            .map(|candidate| Ok(Self::is_layer(&snapshot, candidate)?.then_some(candidate)))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        let at = Self::placement(&siblings, layer, index);
        let patch = snapshot.patch(|edit| {
            edit.promote_entity_to_user(layer)?;
            if snapshot.component::<SceneTextGroup>(parent)?.is_some() {
                edit.promote_entity_to_user(parent)?;
            }
            edit.move_entity(layer, Some(parent), at)
        })?;
        self.commit(patch).await
    }

    pub(crate) async fn set_geometries(
        &mut self,
        geometries: impl IntoIterator<Item = (EntityId, SceneGeometry, f32)>,
    ) -> Result<Commit> {
        let snapshot = self.snapshot();
        let geometries = geometries
            .into_iter()
            .map(|(element, geometry, angle_degrees)| {
                let mut layout = snapshot
                    .component::<SceneTextLayout>(element)?
                    .context("only text layers can change geometry")?;
                layout.origin = Origin::User;
                layout.angle_degrees = Some(angle_degrees);
                let content = Self::text_content(&snapshot, element)?;
                Ok((element, geometry, content, layout))
            })
            .collect::<Result<Vec<_>>>()?;
        let patch = snapshot.patch(|edit| {
            for (element, mut geometry, content, layout) in geometries {
                edit.promote_entity_to_user(element)?;
                edit.promote_entity_to_user(content)?;
                geometry.origin = Origin::User;
                edit.set(element, &geometry)?;
                edit.set(element, &layout)?;
            }
            Ok(())
        })?;
        self.commit(patch).await
    }

    pub(crate) async fn apply_raster_stroke(
        &mut self,
        page: EntityId,
        layer: Option<EntityId>,
        mode: RasterStrokeMode,
        color: [u8; 4],
        diameter: f32,
        points: Vec<ScenePoint>,
    ) -> Result<(Commit, EntityId)> {
        if !diameter.is_finite() || diameter <= 0.0 || points.is_empty() {
            bail!("a raster stroke requires a positive diameter and at least one point");
        }
        if points
            .iter()
            .any(|point| !point.x.is_finite() || !point.y.is_finite())
        {
            bail!("raster stroke points must be finite");
        }
        let snapshot = self.snapshot();
        let page_value = snapshot.page(page)?.page()?;
        let width = page_value.width.round() as u32;
        let height = page_value.height.round() as u32;
        if width == 0 || height == 0 {
            bail!("page dimensions must be positive");
        }
        let mut raster_layer = None;
        let mut promote_layer = false;
        let mut image = if let Some(layer) = layer {
            if snapshot.parent(layer)? != Some(page) {
                bail!("the raster target must be a layer on the active page");
            }
            let target = snapshot
                .component::<SceneRasterLayer>(layer)?
                .context("the raster target must be a layer on the active page")?;
            promote_layer = target.origin != Origin::User
                || snapshot
                    .component::<EntityOrigin>(layer)?
                    .is_some_and(|origin| origin.origin != Origin::User);
            raster_layer = Some(target);
            match snapshot.asset(layer, &AssetRole::new("source")?)? {
                Some(asset) => {
                    let bytes = snapshot.read_blob(asset.blob).await?;
                    image::load_from_memory(&bytes)?.to_rgba8()
                }
                None => RgbaImage::new(width, height),
            }
        } else {
            if mode == RasterStrokeMode::Erase {
                bail!("eraser requires a raster layer target");
            }
            RgbaImage::new(width, height)
        };
        if image.dimensions() != (width, height) {
            bail!("raster layer dimensions must match the page");
        }
        rasterize_stroke(&mut image, mode, color, diameter, &points);
        let mut bytes = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(image).write_to(&mut bytes, ImageFormat::Png)?;
        let source = AssetRole::new("source")?;
        let name = format!(
            "Paint {}",
            snapshot
                .children(page)?
                .filter(|entity| {
                    snapshot
                        .component::<SceneRasterLayer>(*entity)
                        .ok()
                        .flatten()
                        .is_some_and(|layer| layer.kind == RasterLayerKind::Paint)
                })
                .count()
                + 1
        );
        let mut committed_layer = layer;
        let patch = snapshot.patch(|edit| {
            let layer = if let Some(layer) = committed_layer {
                if promote_layer {
                    edit.promote_entity_to_user(layer)?;
                    let raster = raster_layer
                        .as_mut()
                        .expect("an existing raster target has a layer component");
                    raster.origin = Origin::User;
                    edit.set(layer, raster)?;
                }
                layer
            } else {
                let at = snapshot
                    .page(page)?
                    .text_group()?
                    .map_or(At::End, |group| At::Before(group.id()));
                let layer = edit.add_entity(page, at)?;
                edit.set(
                    layer,
                    &SceneRasterLayer {
                        origin: Origin::User,
                        name,
                        kind: RasterLayerKind::Paint,
                    },
                )?;
                committed_layer = Some(layer);
                layer
            };
            edit.set_asset(
                layer,
                &source,
                AssetInput::new(
                    bytes.into_inner(),
                    "image/png",
                    AssetMetadata {
                        width: Some(width),
                        height: Some(height),
                        attributes: Default::default(),
                    },
                ),
            )
        })?;
        Ok((
            self.commit(patch).await?,
            committed_layer.expect("raster layer was selected or added while building the patch"),
        ))
    }

    pub(crate) async fn undo(&mut self) -> Result<Commit> {
        let revisions = self.undo.pop().ok_or_else(|| anyhow!("nothing to undo"))?;
        let patch = match self.session.undo_patch(revisions.iter().copied()) {
            Ok(patch) => patch,
            Err(error) => {
                self.undo.push(revisions);
                return Err(error.into());
            }
        };
        match self.commit(patch).await {
            Ok(commit) => {
                self.redo.push(vec![commit.revision]);
                Ok(commit)
            }
            Err(error) => {
                self.undo.push(revisions);
                Err(error)
            }
        }
    }

    pub(crate) async fn redo(&mut self) -> Result<Commit> {
        let revisions = self.redo.pop().ok_or_else(|| anyhow!("nothing to redo"))?;
        let patch = match self.session.undo_patch(revisions.iter().copied()) {
            Ok(patch) => patch,
            Err(error) => {
                self.redo.push(revisions);
                return Err(error.into());
            }
        };
        match self.commit(patch).await {
            Ok(commit) => {
                self.undo.push(vec![commit.revision]);
                Ok(commit)
            }
            Err(error) => {
                self.redo.push(revisions);
                Err(error)
            }
        }
    }

    pub(crate) fn record(&mut self, revisions: Vec<Revision>) {
        if !revisions.is_empty() {
            self.undo.push(revisions);
            self.redo.clear();
        }
    }

    pub(crate) fn record_commit(&mut self, commit: &Commit) {
        if commit.changes.to != commit.changes.from {
            self.record(vec![commit.revision]);
        }
    }

    pub(crate) async fn commit_rebased(
        &mut self,
        patch: koharu_scene::Patch,
    ) -> Result<Option<Commit>> {
        let current = self.snapshot();
        let patch = match patch.rebase_on(&current) {
            Ok(patch) => patch,
            Err(
                error @ (koharu_scene::Error::PatchConflict(_)
                | koharu_scene::Error::EntityNotFound(_)
                | koharu_scene::Error::RelationNotFound(_)),
            ) => {
                tracing::debug!(%error, "pipeline output was superseded by a document edit");
                return Ok(None);
            }
            Err(error) => return Err(error.into()),
        };
        if patch.is_empty() {
            return Ok(None);
        }
        Ok(Some(self.commit(patch).await?))
    }

    /// Every edit leaves the project through here: onto disk when local,
    /// broadcast to guests when hosting, and over the wire when joined.
    pub(crate) async fn commit(&mut self, patch: koharu_scene::Patch) -> Result<Commit> {
        match &self.mode {
            ProjectMode::Local => Ok(self.session.commit(patch).await?),
            ProjectMode::Host(broadcaster) => {
                let encoded = patch.encode()?;
                let commit = self.session.commit(patch).await?;
                broadcaster.broadcast(commit.revision.get(), &encoded).await;
                Ok(commit)
            }
            ProjectMode::Guest(guest) => commit_on_host(&mut self.session, guest, patch).await,
        }
    }

    pub(crate) fn page(snapshot: &Snapshot, page: EntityId) -> Result<Page> {
        let value = snapshot.page(page)?.page()?;
        let source = Self::asset_id(snapshot, page, "source")?;
        let mut layers = Vec::new();
        Self::collect_layer_views(snapshot, page, &mut layers)?;
        if let Some(image) = source.clone() {
            layers.insert(
                0,
                Layer::Artwork {
                    id: page,
                    parent: None,
                    geometry: Geometry {
                        points: vec![
                            Point { x: 0.0, y: 0.0 },
                            Point {
                                x: value.width,
                                y: 0.0,
                            },
                            Point {
                                x: value.width,
                                y: value.height,
                            },
                            Point {
                                x: 0.0,
                                y: value.height,
                            },
                        ],
                    },
                    visibility: LayerVisibility {
                        visible: true,
                        opacity: 1.0,
                    },
                    image,
                },
            );
        }
        let regions = snapshot
            .descendants(page)?
            .map(|entity| {
                let id = entity.id();
                snapshot
                    .component::<SceneRegion>(id)?
                    .map(|region| Self::analysis_region(snapshot, id, region))
                    .transpose()
            })
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect();
        Ok(Page {
            id: page,
            label: value.label,
            size: PageSize {
                width: value.width,
                height: value.height,
            },
            layers,
            regions,
        })
    }

    fn layer_view(snapshot: &Snapshot, layer: EntityId) -> Result<Layer> {
        let parent = snapshot.parent(layer)?;
        let visibility = Self::layer_visibility(snapshot, layer)?;
        if let Some(group) = snapshot.component::<SceneGroup>(layer)? {
            return Ok(Layer::Group {
                id: layer,
                parent,
                visibility,
                name: group.name,
                role: snapshot
                    .component::<SceneTextGroup>(layer)?
                    .map(|_| GroupRole::Text),
            });
        }
        if let Some(layout) = snapshot.component::<SceneTextLayout>(layer)? {
            let text_layer = snapshot.text_layer(layer)?;
            let content = text_layer.content()?;
            let source = content.source()?.map(|source| SourceText {
                text: source.text.value,
                language: source.language.map(|language| language.to_string()),
            });
            let translation = content.translation()?.map(|translation| Translation {
                text: translation.text.value,
                language: translation.language.map(|language| language.to_string()),
            });
            let role = content.role()?.map(|role| role.role);
            let source_region = content.source_region()?.map(|region| region.id());
            let automatic_region = text_layer.automatic_target()?.map(|region| region.id());
            let typography = text_layer.typography()?.map(Self::typography_view);
            return Ok(Layer::Text {
                id: layer,
                parent,
                geometry: snapshot
                    .component::<SceneGeometry>(layer)?
                    .map(Self::geometry_view),
                angle_degrees: layout.angle_degrees,
                visibility,
                content: Box::new(TextContent {
                    id: content.id(),
                    source,
                    translation,
                    role,
                    source_region,
                }),
                typography,
                layout: layout.kind,
                automatic_region,
            });
        }
        if let Some(raster) = snapshot.component::<SceneRasterLayer>(layer)? {
            return Ok(Layer::Raster {
                id: layer,
                parent,
                visibility,
                image: Self::asset_id(snapshot, layer, "source")?,
                name: raster.name,
                kind: raster.kind,
            });
        }
        let geometry = snapshot
            .component::<SceneGeometry>(layer)?
            .map(Self::geometry_view)
            .context("image layer has no geometry")?;
        let image = Self::asset_id(snapshot, layer, "source")?
            .context("image layer has no source asset")?;
        Ok(Layer::Image {
            id: layer,
            parent,
            geometry,
            visibility,
            image,
        })
    }

    fn analysis_region(
        snapshot: &Snapshot,
        id: EntityId,
        region: SceneRegion,
    ) -> Result<AnalysisRegion> {
        let geometry = snapshot
            .component::<SceneGeometry>(id)?
            .map(Self::geometry_view)
            .context("analysis region has no geometry")?;
        Ok(AnalysisRegion {
            id,
            parent: snapshot.parent(id)?,
            geometry,
            kind: region.kind.as_str().to_owned(),
            label: region.label,
        })
    }

    fn layer_visibility(snapshot: &Snapshot, layer: EntityId) -> Result<LayerVisibility> {
        Ok(snapshot.component::<SceneVisibility>(layer)?.map_or(
            LayerVisibility {
                visible: true,
                opacity: 1.0,
            },
            |visibility| LayerVisibility {
                visible: visibility.visible,
                opacity: visibility.opacity,
            },
        ))
    }

    fn typography_view(typography: SceneTypography) -> Typography {
        let writing_mode = if matches!(&typography.origin, Origin::User) {
            typography.writing_mode
        } else {
            None
        };
        Typography {
            preferred_font: typography.preferred_font,
            font_weight: typography.font_weight,
            font_style: typography.font_style,
            size: typography.size,
            auto_fit: typography.auto_fit,
            color: typography.color,
            stroke_color: typography.stroke_color,
            stroke_width: typography.stroke_width,
            stroke_method: typography.stroke_method,
            stroke_position: typography.stroke_position,
            stroke_join: typography.stroke_join,
            outer_stroke_color: typography.outer_stroke_color,
            outer_stroke_width: typography.outer_stroke_width,
            alignment: typography.alignment,
            writing_mode,
            line_spacing: typography.line_spacing,
        }
    }

    fn geometry_view(geometry: SceneGeometry) -> Geometry {
        Geometry {
            points: geometry
                .points
                .into_iter()
                .map(|point| Point {
                    x: point.x,
                    y: point.y,
                })
                .collect(),
        }
    }

    fn asset_id(snapshot: &Snapshot, entity: EntityId, role: &str) -> Result<Option<String>> {
        Ok(snapshot
            .asset(entity, &AssetRole::new(role)?)?
            .map(|asset| asset.blob.to_string()))
    }

    fn is_layer(snapshot: &Snapshot, entity: EntityId) -> Result<bool> {
        Ok(snapshot.component::<SceneGroup>(entity)?.is_some()
            || snapshot.component::<SceneTextLayout>(entity)?.is_some()
            || snapshot.component::<SceneRasterLayer>(entity)?.is_some()
            || (snapshot.component::<SceneGeometry>(entity)?.is_some()
                && snapshot
                    .asset(entity, &AssetRole::new("source")?)?
                    .is_some()))
    }

    fn is_content_layer(snapshot: &Snapshot, entity: EntityId) -> Result<bool> {
        Ok(
            snapshot.component::<SceneGroup>(entity)?.is_none()
                && Self::is_layer(snapshot, entity)?,
        )
    }

    fn collect_layer_views(
        snapshot: &Snapshot,
        parent: EntityId,
        layers: &mut Vec<Layer>,
    ) -> Result<()> {
        for child in snapshot.children(parent)? {
            if !Self::is_layer(snapshot, child)? {
                continue;
            }
            let group = snapshot.component::<SceneGroup>(child)?.is_some();
            layers.push(Self::layer_view(snapshot, child)?);
            if group {
                Self::collect_layer_views(snapshot, child, layers)?;
            }
        }
        Ok(())
    }

    fn text_content(snapshot: &Snapshot, layer: EntityId) -> Result<EntityId> {
        snapshot
            .text_layer(layer)?
            .content()
            .map(|content| content.id())
            .map_err(Into::into)
    }

    fn placement(siblings: &[EntityId], moving: EntityId, index: usize) -> At {
        siblings
            .iter()
            .copied()
            .filter(|entity| *entity != moving)
            .nth(index)
            .map_or(At::End, At::Before)
    }

    fn unique_roots(snapshot: &Snapshot, entities: Vec<EntityId>) -> Result<Vec<EntityId>> {
        let selected = entities.into_iter().collect::<HashSet<_>>();
        let mut roots = Vec::new();
        for entity in selected.iter().copied() {
            let mut parent = snapshot.parent(entity)?;
            let mut nested = false;
            while let Some(value) = parent {
                if selected.contains(&value) {
                    nested = true;
                    break;
                }
                parent = snapshot.parent(value)?;
            }
            if !nested {
                roots.push(entity);
            }
        }
        roots.sort_unstable();
        Ok(roots)
    }

    pub(crate) fn geometry_from_frame(frame: Frame) -> Result<SceneGeometry> {
        if !frame.x.is_finite()
            || !frame.y.is_finite()
            || !frame.width.is_finite()
            || !frame.height.is_finite()
            || !frame.angle_degrees.is_finite()
            || frame.width <= 0.0
            || frame.height <= 0.0
        {
            bail!("frame must contain finite coordinates and positive dimensions");
        }
        let center_x = f64::from(frame.x + frame.width * 0.5);
        let center_y = f64::from(frame.y + frame.height * 0.5);
        let half_width = f64::from(frame.width) * 0.5;
        let half_height = f64::from(frame.height) * 0.5;
        let (sin, cos) = f64::from(frame.angle_degrees).to_radians().sin_cos();
        Ok(SceneGeometry {
            origin: Origin::User,
            points: [
                (-half_width, -half_height),
                (half_width, -half_height),
                (half_width, half_height),
                (-half_width, half_height),
            ]
            .map(|(x, y)| ScenePoint {
                x: center_x + x * cos - y * sin,
                y: center_y + x * sin + y * cos,
            })
            .into(),
        })
    }
}

fn validate_project_name(name: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty() {
        bail!("project name cannot be empty");
    }
    if name.ends_with(['.', ' '])
        || name
            .chars()
            .any(|character| character.is_control() || r#"<>:"/\|?*"#.contains(character))
    {
        bail!("project name contains characters that cannot be used in a file name");
    }
    let stem = name.split('.').next().unwrap_or(name).to_ascii_uppercase();
    if matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || stem
            .strip_prefix("COM")
            .or_else(|| stem.strip_prefix("LPT"))
            .is_some_and(|number| {
                matches!(number, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
            })
    {
        bail!("project name is reserved by Windows");
    }
    Ok(name.to_owned())
}

/// Upper bound for a composed spread. Matches the renderer surface limit so a
/// merged page is always renderable.
const MAX_MERGE_DIMENSION: u32 = 32_768;

/// Composed spread PNG plus each cleanup/paint layer's re-padded PNG, keyed by
/// layer.
type MergedSpread = (Arc<[u8]>, HashMap<EntityId, Arc<[u8]>>);

/// Paints the given images into one transparent canvas at the supplied horizontal
/// offsets, top aligned.
fn compose_row<'a>(
    fragments: impl IntoIterator<Item = (i64, &'a RgbaImage)>,
    width: u32,
    height: u32,
) -> RgbaImage {
    let mut canvas = RgbaImage::new(width, height);
    for (offset, image) in fragments {
        image::imageops::overlay(&mut canvas, image, offset, 0);
    }
    canvas
}

fn encode_png(image: RgbaImage) -> Result<Arc<[u8]>> {
    let mut encoded = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(image)
        .write_to(&mut encoded, ImageFormat::Png)
        .context("failed to encode a merged page image")?;
    Ok(Arc::from(encoded.into_inner()))
}

fn rasterize_stroke(
    image: &mut RgbaImage,
    mode: RasterStrokeMode,
    color: [u8; 4],
    diameter: f32,
    points: &[ScenePoint],
) {
    let radius = f64::from(diameter) * 0.5;
    for (start, end) in points
        .iter()
        .zip(points.iter().skip(1))
        .chain(points.last().map(|point| (point, point)))
    {
        let left = (start.x.min(end.x) - radius - 0.5).floor().max(0.0) as u32;
        let top = (start.y.min(end.y) - radius - 0.5).floor().max(0.0) as u32;
        let right = (start.x.max(end.x) + radius + 0.5)
            .ceil()
            .min(f64::from(image.width())) as u32;
        let bottom = (start.y.max(end.y) + radius + 0.5)
            .ceil()
            .min(f64::from(image.height())) as u32;
        let dx = end.x - start.x;
        let dy = end.y - start.y;
        let length_squared = dx * dx + dy * dy;
        for y in top..bottom {
            for x in left..right {
                let px = f64::from(x) + 0.5;
                let py = f64::from(y) + 0.5;
                let t = if length_squared == 0.0 {
                    0.0
                } else {
                    (((px - start.x) * dx + (py - start.y) * dy) / length_squared).clamp(0.0, 1.0)
                };
                let distance =
                    ((px - (start.x + t * dx)).powi(2) + (py - (start.y + t * dy)).powi(2)).sqrt();
                let coverage = (radius + 0.5 - distance).clamp(0.0, 1.0) as f32;
                if coverage == 0.0 {
                    continue;
                }
                let pixel = image.get_pixel_mut(x, y);
                match mode {
                    RasterStrokeMode::Paint => {
                        let source_alpha = f32::from(color[3]) / 255.0 * coverage;
                        let destination_alpha = f32::from(pixel[3]) / 255.0;
                        let output_alpha = source_alpha + destination_alpha * (1.0 - source_alpha);
                        if output_alpha > 0.0 {
                            for channel in 0..3 {
                                let source = f32::from(color[channel]) / 255.0;
                                let destination = f32::from(pixel[channel]) / 255.0;
                                pixel[channel] = (((source * source_alpha
                                    + destination * destination_alpha * (1.0 - source_alpha))
                                    / output_alpha)
                                    * 255.0)
                                    .round() as u8;
                            }
                        }
                        pixel[3] = (output_alpha * 255.0).round() as u8;
                    }
                    RasterStrokeMode::Erase => {
                        pixel[3] = (f32::from(pixel[3]) * (1.0 - coverage)).round() as u8;
                    }
                }
            }
        }
    }
}

/// The byte ranges where `query` occurs in `haystack`, left to right and
/// without overlap. Without `match_case`, characters compare by their
/// lowercase forms, so the match follows Unicode rather than ASCII rules.
fn find_ranges(haystack: &str, query: &str, match_case: bool) -> Vec<Range<usize>> {
    if query.is_empty() {
        return Vec::new();
    }
    if match_case {
        return haystack
            .match_indices(query)
            .map(|(start, found)| start..start + found.len())
            .collect();
    }
    let query = query
        .chars()
        .flat_map(char::to_lowercase)
        .collect::<Vec<_>>();
    let mut ranges = Vec::new();
    let mut start = 0;
    while let Some(next) = haystack[start..].chars().next() {
        match folded_prefix(&haystack[start..], &query) {
            Some(length) => {
                ranges.push(start..start + length);
                start += length;
            }
            None => start += next.len_utf8(),
        }
    }
    ranges
}

/// The byte length of the start of `text` whose lowercase form is `query`.
fn folded_prefix(text: &str, query: &[char]) -> Option<usize> {
    let mut remaining = query;
    for (index, character) in text.char_indices() {
        if remaining.is_empty() {
            return Some(index);
        }
        for lower in character.to_lowercase() {
            let (first, rest) = remaining.split_first()?;
            if *first != lower {
                return None;
            }
            remaining = rest;
        }
    }
    remaining.is_empty().then_some(text.len())
}

fn replace_ranges(text: &str, ranges: &[Range<usize>], replacement: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut copied = 0;
    for range in ranges {
        output.push_str(&text[copied..range.start]);
        output.push_str(replacement);
        copied = range.end;
    }
    output.push_str(&text[copied..]);
    output
}

#[cfg(test)]
mod tests {
    use koharu_scene::{Generation, ProducerId, WritingMode};

    use super::*;

    #[tokio::test]
    async fn merging_adjacent_pages_composes_a_wide_spread_and_keeps_layers() {
        let mut session = Session::memory().await.unwrap();
        let source = AssetRole::new("source").unwrap();
        let mut setup = session.snapshot().edit();
        let first = setup
            .add_page(PageDraft::new("first", 2.0, 3.0), At::End)
            .unwrap();
        let second = setup
            .add_page(PageDraft::new("second", 4.0, 3.0), At::End)
            .unwrap();
        for (page, width, height) in [(first, 2u32, 3u32), (second, 4u32, 3u32)] {
            let image = RgbaImage::from_pixel(width, height, image::Rgba([0, 0, 0, 255]));
            let mut encoded = Cursor::new(Vec::new());
            DynamicImage::ImageRgba8(image)
                .write_to(&mut encoded, ImageFormat::Png)
                .unwrap();
            setup
                .set_asset(
                    page,
                    &source,
                    AssetInput::new(
                        Arc::<[u8]>::from(encoded.into_inner()),
                        "image/png",
                        AssetMetadata {
                            width: Some(width),
                            height: Some(height),
                            attributes: Default::default(),
                        },
                    ),
                )
                .unwrap();
        }

        let layout = SceneTextLayout {
            origin: Origin::User,
            kind: TextLayoutKind::Point,
            angle_degrees: None,
        };
        let first_content = setup.add_text_content(first, At::End).unwrap();
        setup
            .add_text_layer(first, At::End, first_content, &layout)
            .unwrap();
        let second_content = setup.add_text_content(second, At::End).unwrap();
        let second_layer = setup
            .add_text_layer(second, At::End, second_content, &layout)
            .unwrap();
        let first_region = setup
            .add_analysis_region::<koharu_scene::PanelRegion>(
                first,
                At::End,
                &SceneGeometry::rectangle(0.0, 0.0, 2.0, 2.0),
                None,
            )
            .unwrap();
        let second_region = setup
            .add_analysis_region::<koharu_scene::PanelRegion>(
                second,
                At::End,
                &SceneGeometry::rectangle(0.0, 0.0, 2.0, 2.0),
                None,
            )
            .unwrap();
        session.commit(setup.finish().unwrap()).await.unwrap();
        let mut project = Project::new(session, "test".to_owned());

        let (commit, merged) = project
            .merge_pages(vec![first, second], true)
            .await
            .unwrap();
        project.record_commit(&commit);

        let snapshot = project.snapshot();
        let view = snapshot.page(merged).unwrap().page().unwrap();
        assert_eq!((view.width, view.height), (6.0, 3.0));
        assert_eq!(Project::pages(&snapshot).unwrap().len(), 1);
        // Text layers from both pages survive under the single merged text group.
        assert_eq!(
            snapshot.entities_with::<SceneTextLayout>().unwrap().count(),
            2
        );
        let group = snapshot
            .page(merged)
            .unwrap()
            .text_group()
            .unwrap()
            .unwrap();
        assert_eq!(snapshot.parent(second_layer).unwrap(), Some(group.id()));
        // NeKoharu spreads read right to left: the first page lands on the
        // right half, so its region is translated by the second page's width
        // while the second page's region stays at the origin.
        let first_region = snapshot
            .component::<SceneGeometry>(first_region)
            .unwrap()
            .unwrap();
        assert_eq!(first_region.points[0].x, 4.0);
        let second_region = snapshot
            .component::<SceneGeometry>(second_region)
            .unwrap()
            .unwrap();
        assert_eq!(second_region.points[0].x, 0.0);

        // Undo restores the two source pages.
        project.undo().await.unwrap();
        assert_eq!(Project::pages(&project.snapshot()).unwrap().len(), 2);
    }

    #[tokio::test]
    async fn merging_can_keep_the_original_pages() {
        let mut session = Session::memory().await.unwrap();
        let source = AssetRole::new("source").unwrap();
        let mut setup = session.snapshot().edit();
        let first = setup
            .add_page(PageDraft::new("first", 2.0, 3.0), At::End)
            .unwrap();
        let second = setup
            .add_page(PageDraft::new("second", 4.0, 3.0), At::End)
            .unwrap();
        for (page, width, height) in [(first, 2u32, 3u32), (second, 4u32, 3u32)] {
            let image = RgbaImage::from_pixel(width, height, image::Rgba([0, 0, 0, 255]));
            let mut encoded = Cursor::new(Vec::new());
            DynamicImage::ImageRgba8(image)
                .write_to(&mut encoded, ImageFormat::Png)
                .unwrap();
            setup
                .set_asset(
                    page,
                    &source,
                    AssetInput::new(
                        Arc::<[u8]>::from(encoded.into_inner()),
                        "image/png",
                        AssetMetadata {
                            width: Some(width),
                            height: Some(height),
                            attributes: Default::default(),
                        },
                    ),
                )
                .unwrap();
        }
        let layout = SceneTextLayout {
            origin: Origin::User,
            kind: TextLayoutKind::Point,
            angle_degrees: None,
        };
        let first_content = setup.add_text_content(first, At::End).unwrap();
        let first_layer = setup
            .add_text_layer(first, At::End, first_content, &layout)
            .unwrap();
        session.commit(setup.finish().unwrap()).await.unwrap();
        let mut project = Project::new(session, "test".to_owned());

        let (commit, merged) = project
            .merge_pages(vec![first, second], false)
            .await
            .unwrap();
        project.record_commit(&commit);

        let snapshot = project.snapshot();
        // Both source pages survive next to the new spread.
        assert_eq!(Project::pages(&snapshot).unwrap().len(), 3);
        // Their layers still moved onto the merged page.
        let group = snapshot.parent(first_layer).unwrap().unwrap();
        assert_eq!(snapshot.parent(group).unwrap(), Some(merged));
    }

    #[test]
    fn only_user_authored_direction_is_projected_as_an_override() {
        let mut typography = SceneTypography {
            origin: Origin::Generated(Generation::new(
                ProducerId::new("dev.koharu.pipeline.detection").expect("valid producer"),
            )),
            preferred_font: None,
            font_weight: None,
            font_style: None,
            size: None,
            auto_fit: true,
            color: None,
            stroke_color: None,
            stroke_width: None,
            stroke_method: None,
            stroke_position: None,
            stroke_join: None,
            outer_stroke_color: None,
            outer_stroke_width: None,
            alignment: None,
            writing_mode: Some(WritingMode::Vertical),
            extensions: Default::default(),
            line_spacing: None,
        };

        assert_eq!(
            Project::typography_view(typography.clone()).writing_mode,
            None
        );
        typography.origin = Origin::User;
        assert_eq!(
            Project::typography_view(typography).writing_mode,
            Some(WritingMode::Vertical)
        );
    }

    #[tokio::test]
    async fn chapter_start_markers_are_projected_and_survive_page_renames() {
        let mut session = Session::memory().await.unwrap();
        let mut setup = session.snapshot().edit();
        let page = setup
            .add_page(PageDraft::new("chapter start", 100.0, 100.0), At::End)
            .unwrap();
        session.commit(setup.finish().unwrap()).await.unwrap();
        let mut project = Project::new(session, "test".to_owned());

        project.set_page_chapter_start(page, true).await.unwrap();
        assert!(
            Project::pages(&project.snapshot())
                .unwrap()
                .first()
                .unwrap()
                .chapter_start
        );

        project
            .rename_page(page, "renamed chapter".to_owned())
            .await
            .unwrap();
        let summary = Project::pages(&project.snapshot()).unwrap();
        assert_eq!(summary[0].label, "renamed chapter");
        assert!(summary[0].chapter_start);
    }

    #[tokio::test]
    async fn moving_pages_uses_the_destination_index_after_removing_the_source() {
        let mut session = Session::memory().await.unwrap();
        let mut setup = session.snapshot().edit();
        let first = setup
            .add_page(PageDraft::new("first", 100.0, 100.0), At::End)
            .unwrap();
        setup
            .add_page(PageDraft::new("middle", 100.0, 100.0), At::End)
            .unwrap();
        let last = setup
            .add_page(PageDraft::new("last", 100.0, 100.0), At::End)
            .unwrap();
        session.commit(setup.finish().unwrap()).await.unwrap();
        let mut project = Project::new(session, "test".to_owned());

        project.move_page(first, 2).await.unwrap();
        assert_eq!(
            Project::pages(&project.snapshot())
                .unwrap()
                .iter()
                .map(|page| page.label.as_str())
                .collect::<Vec<_>>(),
            ["middle", "last", "first"]
        );

        project.move_page(last, 0).await.unwrap();
        assert_eq!(
            Project::pages(&project.snapshot())
                .unwrap()
                .iter()
                .map(|page| page.label.as_str())
                .collect::<Vec<_>>(),
            ["last", "middle", "first"]
        );
    }

    #[tokio::test]
    async fn pastes_text_layer_content_and_placement_to_another_page() {
        let mut session = Session::memory().await.unwrap();
        let mut setup = session.snapshot().edit();
        let source_page = setup
            .add_page(PageDraft::new("source", 100.0, 100.0), At::End)
            .unwrap();
        let target_page = setup
            .add_page(PageDraft::new("target", 100.0, 100.0), At::End)
            .unwrap();
        session.commit(setup.finish().unwrap()).await.unwrap();
        let mut project = Project::new(session, "test".to_owned());

        let (_, source_layer) = project
            .add_text_box(
                source_page,
                Frame {
                    x: 12.0,
                    y: 24.0,
                    width: 32.0,
                    height: 18.0,
                    angle_degrees: 0.0,
                },
            )
            .await
            .unwrap();
        project
            .set_source_text(source_layer, "BOOM".to_owned())
            .await
            .unwrap();
        project
            .set_translation(source_layer, Some("Bang!".to_owned()))
            .await
            .unwrap();

        let (_, pasted) = project
            .paste_text_layers(target_page, vec![source_layer])
            .await
            .unwrap();
        let target = Project::page(&project.snapshot(), target_page).unwrap();
        let Some(Layer::Text {
            id,
            geometry,
            content,
            ..
        }) = target
            .layers
            .iter()
            .find(|layer| matches!(layer, Layer::Text { .. }))
        else {
            panic!("expected a pasted text layer");
        };
        assert_eq!(pasted, vec![*id]);
        assert_ne!(*id, source_layer);
        assert_eq!(
            content.source.as_ref().map(|source| source.text.as_str()),
            Some("BOOM")
        );
        assert_eq!(
            content
                .translation
                .as_ref()
                .map(|translation| translation.text.as_str()),
            Some("Bang!")
        );
        assert_eq!(
            geometry.as_ref().map(|geometry| geometry.points[0].x),
            Some(12.0)
        );
        assert_eq!(
            geometry.as_ref().map(|geometry| geometry.points[0].y),
            Some(24.0)
        );
        assert!(matches!(
            target.layers.first(),
            Some(Layer::Group {
                role: Some(GroupRole::Text),
                ..
            })
        ));
    }

    #[tokio::test]
    async fn pasted_copies_only_keep_user_authored_writing_direction() {
        use koharu_scene::{BubbleRegion, FlowsIn};

        let mut session = Session::memory().await.unwrap();
        let mut setup = session.snapshot().edit();
        let page = setup
            .add_page(PageDraft::new("page", 200.0, 300.0), At::End)
            .unwrap();
        let bubble = setup
            .add_analysis_region::<BubbleRegion>(
                page,
                At::End,
                &SceneGeometry::rectangle(20.0, 20.0, 160.0, 180.0),
                None,
            )
            .unwrap();
        let pipeline_content = setup.add_text_content(page, At::End).unwrap();
        let pipeline_layer = setup
            .add_text_layer(
                page,
                At::End,
                pipeline_content,
                &SceneTextLayout {
                    origin: Origin::User,
                    kind: TextLayoutKind::Paragraph,
                    angle_degrees: None,
                },
            )
            .unwrap();
        setup.relate::<FlowsIn>(pipeline_layer, bubble).unwrap();

        let user_content = setup.add_text_content(page, At::End).unwrap();
        let user_layer = setup
            .add_text_layer(
                page,
                At::End,
                user_content,
                &SceneTextLayout {
                    origin: Origin::User,
                    kind: TextLayoutKind::Paragraph,
                    angle_degrees: None,
                },
            )
            .unwrap();
        setup
            .set(
                user_layer,
                &SceneTypography {
                    origin: Origin::User,
                    preferred_font: None,
                    font_weight: None,
                    font_style: None,
                    size: Some(30.0),
                    auto_fit: false,
                    color: None,
                    stroke_color: None,
                    stroke_width: None,
                    stroke_method: None,
                    stroke_position: None,
                    stroke_join: None,
                    outer_stroke_color: None,
                    outer_stroke_width: None,
                    alignment: None,
                    writing_mode: Some(WritingMode::Vertical),
                    extensions: Default::default(),
                    line_spacing: None,
                },
            )
            .unwrap();
        session.commit(setup.finish().unwrap()).await.unwrap();

        // Detection writes its inferred direction as pipeline output; user
        // edits stamp components as theirs, so this must be a pipeline edit.
        let generation = Generation::new(
            ProducerId::new("dev.koharu.pipeline.detection").expect("valid producer"),
        );
        let mut detection = session.snapshot().edit_as(generation.clone());
        detection
            .set(
                pipeline_layer,
                &SceneTypography {
                    origin: Origin::Generated(generation),
                    preferred_font: None,
                    font_weight: None,
                    font_style: None,
                    size: None,
                    auto_fit: true,
                    color: Some([12, 34, 56, u8::MAX]),
                    stroke_color: None,
                    stroke_width: None,
                    stroke_method: None,
                    stroke_position: None,
                    stroke_join: None,
                    outer_stroke_color: None,
                    outer_stroke_width: None,
                    alignment: None,
                    writing_mode: Some(WritingMode::Vertical),
                    extensions: Default::default(),
                    line_spacing: None,
                },
            )
            .unwrap();
        session.commit(detection.finish().unwrap()).await.unwrap();

        let mut project = Project::new(session, "test".to_owned());
        project
            .set_source_text(pipeline_layer, "ハズカシイ".to_owned())
            .await
            .unwrap();
        project
            .set_translation(
                pipeline_layer,
                Some("I THINK HUMILIATION SCENARIOS ARE TRENDING".to_owned()),
            )
            .await
            .unwrap();
        project
            .set_source_text(user_layer, "カタカナ".to_owned())
            .await
            .unwrap();
        project
            .set_translation(user_layer, Some("Vertical on purpose".to_owned()))
            .await
            .unwrap();

        let (_, pasted) = project
            .paste_text_layers(page, vec![pipeline_layer, user_layer])
            .await
            .unwrap();

        let snapshot = project.snapshot();
        let pipeline_copy = snapshot
            .component::<SceneTypography>(pasted[0])
            .unwrap()
            .unwrap();
        // The inference described the original's source text; the copy must
        // render horizontal English by its own content, like the original does.
        assert_eq!(pipeline_copy.origin, Origin::User);
        assert_eq!(pipeline_copy.writing_mode, None);
        assert_eq!(pipeline_copy.color, Some([12, 34, 56, u8::MAX]));

        let user_copy = snapshot
            .component::<SceneTypography>(pasted[1])
            .unwrap()
            .unwrap();
        // An explicit user direction is authored intent and survives the copy.
        assert_eq!(user_copy.origin, Origin::User);
        assert_eq!(user_copy.writing_mode, Some(WritingMode::Vertical));
    }

    #[tokio::test]
    async fn pipeline_commit_rebases_or_yields_to_the_manual_edit() {
        let mut session = Session::memory().await.unwrap();
        let mut setup = session.snapshot().edit();
        let pipeline_page = setup
            .add_page(PageDraft::new("pipeline", 100.0, 100.0), At::End)
            .unwrap();
        let manual_page = setup
            .add_page(PageDraft::new("manual", 100.0, 100.0), At::End)
            .unwrap();
        session.commit(setup.finish().unwrap()).await.unwrap();
        let mut project = Project::new(session, "test".to_owned());

        let base = project.snapshot();
        let pipeline = base
            .patch(|edit| {
                edit.set_page(
                    pipeline_page,
                    PageDraft::new("pipeline result", 100.0, 100.0),
                )
            })
            .unwrap();
        let manual = base
            .patch(|edit| edit.set_page(manual_page, PageDraft::new("manual edit", 100.0, 100.0)))
            .unwrap();
        project.commit(manual).await.unwrap();

        let commit = project.commit_rebased(pipeline).await.unwrap().unwrap();
        assert_eq!(
            commit
                .snapshot
                .page(pipeline_page)
                .unwrap()
                .page()
                .unwrap()
                .label,
            "pipeline result"
        );
        assert_eq!(
            commit
                .snapshot
                .page(manual_page)
                .unwrap()
                .page()
                .unwrap()
                .label,
            "manual edit"
        );

        let base = project.snapshot();
        let pipeline = base
            .patch(|edit| {
                edit.set_page(
                    pipeline_page,
                    PageDraft::new("stale pipeline", 100.0, 100.0),
                )
            })
            .unwrap();
        let manual = base
            .patch(|edit| {
                edit.set_page(pipeline_page, PageDraft::new("latest manual", 100.0, 100.0))
            })
            .unwrap();
        project.commit(manual).await.unwrap();

        assert!(project.commit_rebased(pipeline).await.unwrap().is_none());
        assert_eq!(
            project
                .snapshot()
                .page(pipeline_page)
                .unwrap()
                .page()
                .unwrap()
                .label,
            "latest manual"
        );
    }

    #[test]
    fn raster_strokes_are_continuous_and_erasable() {
        let mut image = RgbaImage::new(32, 16);
        let points = [
            ScenePoint { x: 4.0, y: 8.0 },
            ScenePoint { x: 28.0, y: 8.0 },
        ];
        rasterize_stroke(
            &mut image,
            RasterStrokeMode::Paint,
            [210, 40, 20, 255],
            5.0,
            &points,
        );
        for x in 4..28 {
            assert_eq!(image.get_pixel(x, 8).0, [210, 40, 20, 255]);
        }

        rasterize_stroke(
            &mut image,
            RasterStrokeMode::Erase,
            [0, 0, 0, 0],
            5.0,
            &[ScenePoint { x: 16.0, y: 8.0 }],
        );
        assert_eq!(image.get_pixel(16, 8)[3], 0);
        assert_eq!(image.get_pixel(4, 8)[3], 255);

        let mut white = RgbaImage::new(4, 4);
        rasterize_stroke(
            &mut white,
            RasterStrokeMode::Paint,
            [255, 255, 255, 255],
            3.0,
            &[ScenePoint { x: 2.0, y: 2.0 }],
        );
        assert_eq!(white.get_pixel(2, 2).0, [255, 255, 255, 255]);
    }

    #[test]
    fn find_ranges_matches_with_and_without_case() {
        assert_eq!(find_ranges("Ken and KEN", "Ken", true), vec![0..3]);
        assert_eq!(find_ranges("Ken and KEN", "ken", false), vec![0..3, 8..11]);
        assert_eq!(
            find_ranges("ÉCOLE école", "école", false),
            vec![0..6, 7..13]
        );
        assert_eq!(find_ranges("aaaa", "aa", false), vec![0..2, 2..4]);
        assert!(find_ranges("anything", "", false).is_empty());
        assert_eq!(
            replace_ranges("Ken and KEN", &[0..3, 8..11], "Kenji"),
            "Kenji and Kenji"
        );
    }

    #[tokio::test]
    async fn translations_are_found_and_replaced_across_pages_as_one_step() {
        let mut session = Session::memory().await.unwrap();
        let mut setup = session.snapshot().edit();
        let first = setup
            .add_page(PageDraft::new("first", 100.0, 100.0), At::End)
            .unwrap();
        let second = setup
            .add_page(PageDraft::new("second", 100.0, 100.0), At::End)
            .unwrap();
        session.commit(setup.finish().unwrap()).await.unwrap();
        let mut project = Project::new(session, "test".to_owned());
        let frame = Frame {
            x: 10.0,
            y: 10.0,
            width: 40.0,
            height: 20.0,
            angle_degrees: 0.0,
        };
        let mut layers = Vec::new();
        for (page, text) in [(first, "Onii-chan!"), (second, "Thanks, onii-chan.")] {
            let (_, layer) = project.add_text_box(page, frame).await.unwrap();
            project
                .set_translation(layer, Some(text.to_owned()))
                .await
                .unwrap();
            layers.push(layer);
        }

        let found = project.find_translations("onii-chan", false).unwrap();
        assert_eq!(
            found
                .iter()
                .map(|found| (found.page_index, found.layer, found.count))
                .collect::<Vec<_>>(),
            vec![(0, layers[0], 1), (1, layers[1], 1)]
        );
        assert_eq!(
            project.find_translations("onii-chan", true).unwrap().len(),
            1
        );

        // Replacing one layer leaves the other alone.
        let (commit, count) = project
            .replace_translations("onii-chan", "big bro", false, Some(&layers[1..]))
            .await
            .unwrap();
        project.record_commit(&commit);
        assert_eq!(count, 1);
        let text = |project: &Project, layer| {
            project
                .snapshot()
                .text_layer(layer)
                .unwrap()
                .content()
                .unwrap()
                .translation()
                .unwrap()
                .unwrap()
                .text
                .value
        };
        assert_eq!(text(&project, layers[0]), "Onii-chan!");
        assert_eq!(text(&project, layers[1]), "Thanks, big bro.");

        // Replacing everywhere is one step that one undo reverts.
        let (commit, count) = project
            .replace_translations("big bro", "Kenji", true, None)
            .await
            .unwrap();
        project.record_commit(&commit);
        assert_eq!(count, 1);
        project.undo().await.unwrap();
        assert_eq!(text(&project, layers[1]), "Thanks, big bro.");
        assert!(
            project
                .replace_translations("nobody", "x", false, None)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn ocr_box_creates_a_text_region_that_ocr_can_recognize() {
        use koharu_scene::RegionSpec as _;

        let mut session = Session::memory().await.unwrap();
        let mut setup = session.snapshot().edit();
        let page = setup
            .add_page(PageDraft::new("page", 100.0, 100.0), At::End)
            .unwrap();
        session.commit(setup.finish().unwrap()).await.unwrap();
        let mut project = Project::new(session, "test".to_owned());

        let (_, layer, region) = project
            .add_ocr_box(
                page,
                Frame {
                    x: 10.0,
                    y: 20.0,
                    width: 30.0,
                    height: 40.0,
                    angle_degrees: 0.0,
                },
            )
            .await
            .unwrap();

        let snapshot = project.snapshot();
        let view = Project::page(&snapshot, page).unwrap();
        let region_view = view
            .regions
            .iter()
            .find(|candidate| candidate.id == region)
            .expect("the OCR box creates a region");
        assert_eq!(region_view.kind, TextRegion::kind().as_str());

        let Some(Layer::Text {
            id,
            content,
            geometry,
            ..
        }) = view
            .layers
            .iter()
            .find(|layer| matches!(layer, Layer::Text { .. }))
        else {
            panic!("expected an OCR text layer");
        };
        assert_eq!(*id, layer);
        assert!(content.source.is_none());
        assert_eq!(content.source_region, Some(region));
        assert_eq!(
            geometry.as_ref().map(|geometry| geometry.points[0].x),
            Some(10.0)
        );
        // The OCR stage discovers targets through RecognizedFrom on the region.
        assert_eq!(
            snapshot
                .text_content(content.id)
                .unwrap()
                .source_region()
                .unwrap()
                .unwrap()
                .id(),
            region
        );
    }
}
