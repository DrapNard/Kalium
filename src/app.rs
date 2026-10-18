use crate::{
    brush_preset,
    document::{BlendMode, Channel, Document, Layer},
    export,
    history::{
        History, LayerProperties, Snapshot, StrokeCapture, StrokeCommitResult, STROKE_TILE_SIZE,
    },
    layout::{self, Pane},
    mesh::Mesh,
    ops,
    paint::{Brush, BrushMode},
    project,
    recovery::{self, DocumentEvent, Journal, Record},
    renderer::{MaterialLibrary, Viewport},
    spp,
    uv::UvCoverage,
};
use eframe::egui::{self, Color32, ColorImage, TextureHandle, TextureOptions, Vec2};
use egui_dock::{DockArea, DockState, Style, TabViewer};
use std::collections::HashSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Canvas {
    Model,
    UV,
}
#[derive(Clone, Copy)]
enum Action {
    New,
    Open,
    Save,
    SaveAs,
    SaveCheckpoint,
    RecoverCheckpoint,
    ImportMesh,
    ImportTexture,
    Export,
    InspectSpp,
    LoadBrush,
    SaveBrush,
    Fill,
    ClearChannel,
    InvertMask,
    ResetMask,
    ExportAll,
}
enum DestructiveAction {
    New,
    Open {
        doc: Box<Document>,
        path: PathBuf,
        recovered: bool,
    },
    Close,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum DestructiveChoice {
    Save,
    Discard,
    Cancel,
}

fn should_continue_after_choice(choice: DestructiveChoice, save_succeeded: bool) -> bool {
    match choice {
        DestructiveChoice::Save => save_succeeded,
        DestructiveChoice::Discard => true,
        DestructiveChoice::Cancel => false,
    }
}

fn paths_refer_to_same_file(left: &std::path::Path, right: &std::path::Path) -> bool {
    match (std::fs::canonicalize(left), std::fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

fn recovery_journal_path(project_path: &std::path::Path) -> PathBuf {
    let mut filename = project_path
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("project"))
        .to_os_string();
    filename.push(".journal");
    project_path.with_file_name(filename)
}

fn recovery_journal_disabled_path(journal_path: &std::path::Path) -> PathBuf {
    let mut name = journal_path.as_os_str().to_os_string();
    name.push(".disabled");
    PathBuf::from(name)
}

fn project_id_for_save_target(
    current_id: uuid::Uuid,
    current_path: Option<&std::path::Path>,
    target_path: &std::path::Path,
) -> uuid::Uuid {
    if current_path.is_some_and(|current| !paths_refer_to_same_file(current, target_path)) {
        uuid::Uuid::new_v4()
    } else {
        current_id
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LayerCommand {
    Add,
    Duplicate,
    Remove,
    Up,
    Down,
}

#[derive(Clone)]
struct LayerRecoveryProperties {
    name: String,
    visible: bool,
    opacity: f32,
    blend: BlendMode,
    disabled_channels: HashSet<Channel>,
}

impl LayerRecoveryProperties {
    fn capture(layer: &Layer) -> Self {
        Self {
            name: layer.name.clone(),
            visible: layer.visible,
            opacity: layer.opacity,
            blend: layer.blend,
            disabled_channels: layer.disabled_channels.clone(),
        }
    }

    fn events_since(&self, set_id: u32, layer: &Layer) -> Vec<DocumentEvent> {
        let mut events = Vec::new();
        if self.name != layer.name {
            events.push(DocumentEvent::LayerRename {
                set_id,
                layer_id: layer.id,
                name: layer.name.clone(),
            });
        }
        if self.visible != layer.visible {
            events.push(DocumentEvent::LayerVisibility {
                set_id,
                layer_id: layer.id,
                visible: layer.visible,
            });
        }
        if self.opacity != layer.opacity {
            events.push(DocumentEvent::LayerOpacity {
                set_id,
                layer_id: layer.id,
                opacity: layer.opacity,
            });
        }
        if self.blend != layer.blend {
            events.push(DocumentEvent::LayerBlendMode {
                set_id,
                layer_id: layer.id,
                blend: layer.blend,
            });
        }
        for channel in Channel::ALL {
            let was_enabled = !self.disabled_channels.contains(&channel);
            let is_enabled = !layer.disabled_channels.contains(&channel);
            if was_enabled != is_enabled {
                events.push(DocumentEvent::LayerChannelEnabled {
                    set_id,
                    layer_id: layer.id,
                    channel,
                    enabled: is_enabled,
                });
            }
        }
        events
    }
}
struct PendingStroke {
    capture: StrokeCapture,
    was_unsaved: bool,
}

struct PendingJournalRecovery {
    project_path: PathBuf,
    journal_path: PathBuf,
    records: Vec<Record>,
    journal: Journal,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum JournalRecoveryChoice {
    Restore,
    Discard,
}

pub struct KaliumApp {
    doc: Document,
    brush: Brush,
    channel: Channel,
    solo_channel: bool,
    show_uv_wire: bool,
    view: Viewport,
    uv_coverage: Option<UvCoverage>,
    uv_clipping: bool,
    material: Option<MaterialLibrary>,
    model_texture: Option<TextureHandle>,
    uv_texture: Option<TextureHandle>,
    paint_dirty: bool,
    model_dirty: bool,
    history: History,
    previous_uv: Option<[f32; 2]>,
    pending_stroke: Option<PendingStroke>,
    file_path: Option<PathBuf>,
    recovery_journal: Option<Journal>,
    recovery_journal_path: Option<PathBuf>,
    pending_journal_recovery: Option<PendingJournalRecovery>,
    allow_recovery_change: bool,
    unsaved: bool,
    pending_destructive: Option<DestructiveAction>,
    allow_close: bool,
    status: String,
    dock: DockState<Pane>,
    last_layout_save: Instant,
    saved_layout: Vec<u8>,
    asset_search: String,
    show_tool_shelf: bool,
    texture_set_name_input: String,
    auto_switch_on_paint: bool,
    pending_export_dir: Option<PathBuf>,
    pending_export_all: bool,
    export_padding_radius: u32,
    export_normal_y: export::NormalYConvention,
}

impl KaliumApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let mut visuals = egui::Visuals::dark();
        visuals.selection.bg_fill = Color32::from_rgb(44, 105, 156);
        visuals.widgets.active.bg_fill = Color32::from_rgb(50, 98, 144);
        visuals.widgets.hovered.bg_fill = Color32::from_rgb(55, 63, 75);
        visuals.panel_fill = Color32::from_rgb(33, 34, 38);
        visuals.window_fill = Color32::from_rgb(34, 35, 40);
        cc.egui_ctx.set_visuals(visuals);
        let doc = Document::new(512);
        let uv_coverage = UvCoverage::generate(&doc.mesh, doc.size).ok();
        Self {
            doc,
            brush: Brush::default(),
            channel: Channel::BaseColor,
            solo_channel: false,
            show_uv_wire: true,
            view: Viewport::new(600, 600),
            uv_coverage,
            uv_clipping: true,
            material: None,
            model_texture: None,
            uv_texture: None,
            paint_dirty: true,
            model_dirty: true,
            history: History::new(),
            previous_uv: None,
            pending_stroke: None,
            file_path: None,
            recovery_journal: None,
            recovery_journal_path: None,
            pending_journal_recovery: None,
            allow_recovery_change: false,
            unsaved: false,
            pending_destructive: None,
            allow_close: false,
            status: "Ready • Left drag to paint • Right drag to orbit • Scroll to zoom".into(),
            dock: layout::load_workspace(),
            last_layout_save: Instant::now(),
            saved_layout: Vec::new(),
            asset_search: String::new(),
            show_tool_shelf: true,
            texture_set_name_input: "Default".into(),
            auto_switch_on_paint: true,
            pending_export_dir: None,
            pending_export_all: false,
            export_padding_radius: 0,
            export_normal_y: crate::export::NormalYConvention::OpenGl,
        }
    }

    fn rebuild_coverage(&mut self) {
        match UvCoverage::generate_for_set(
            &self.doc.mesh,
            self.doc.size,
            &self.doc.material_bindings,
            self.doc.active_set_id,
        ) {
            Ok(map) => self.uv_coverage = Some(map),
            Err(err) => {
                self.uv_coverage = None;
                self.uv_clipping = false;
                self.status = format!("UV clipping disabled: {err}");
            }
        }
    }
    fn change(&mut self) {
        if self.recovery_journal.is_some() && !self.allow_recovery_change {
            self.disable_recovery_journal(
                "A project edit is not yet covered by journal events. Save to start a new recovery baseline.",
            );
        }
        self.allow_recovery_change = false;
        self.paint_dirty = true;
        self.model_dirty = true;
        self.unsaved = true;
    }
    fn change_with_recovery_events(&mut self, events: &[DocumentEvent]) {
        if self.recovery_journal.is_some() && !events.is_empty() {
            self.append_recovery_batch(events);
            self.allow_recovery_change = self.recovery_journal.is_some();
        }
        self.change();
    }
    fn update_painted_tiles(&mut self, channel: Channel, tile_keys: &HashSet<usize>) {
        if tile_keys.is_empty() {
            return;
        }
        if self.paint_dirty || self.material.is_none() {
            self.change();
            return;
        }

        let size = self.doc.size;
        let tile_columns = size.div_ceil(STROKE_TILE_SIZE) as usize;
        let set_id = self.doc.active_set_id;
        for &key in tile_keys {
            let tile_x = (key % tile_columns) as u32;
            let tile_y = (key / tile_columns) as u32;
            let x = tile_x * STROKE_TILE_SIZE;
            let y = tile_y * STROKE_TILE_SIZE;
            let width = STROKE_TILE_SIZE.min(size - x);
            let height = STROKE_TILE_SIZE.min(size - y);
            let Some(region) = self
                .doc
                .composite_region_for_set(set_id, channel, x, y, width, height)
            else {
                self.change();
                return;
            };
            let patched = self
                .material
                .as_mut()
                .is_some_and(|material| material.patch_region(set_id, channel, x, y, &region));
            if !patched {
                self.change();
                return;
            }
            if channel == self.channel {
                if let Some(texture) = self.uv_texture.as_mut() {
                    let image = ColorImage::from_rgba_unmultiplied(
                        [region.width() as usize, region.height() as usize],
                        region.as_raw(),
                    );
                    texture.set_partial([x as usize, y as usize], image, TextureOptions::LINEAR);
                }
            }
        }
        self.unsaved = true;
        self.model_dirty = true;
    }
    fn refresh_textures(&mut self, ctx: &egui::Context) {
        if self.paint_dirty || self.material.is_none() {
            self.material = Some(MaterialLibrary::from_document(&self.doc));
            self.paint_dirty = false;
            self.model_dirty = true;
            self.uv_texture = None;
        }
        if self.uv_texture.is_none() {
            let image = self.material.as_ref().unwrap().active().image(self.channel);
            let rgba = ColorImage::from_rgba_unmultiplied(
                [image.width() as usize, image.height() as usize],
                image.as_raw(),
            );
            self.uv_texture = Some(ctx.load_texture("kalium-uv", rgba, TextureOptions::LINEAR));
        }
        if self.model_dirty {
            self.view.render_bound(
                &self.doc.mesh,
                self.material.as_ref().unwrap(),
                &self.doc.material_bindings,
                if self.solo_channel {
                    Some(self.channel)
                } else {
                    None
                },
            );
            let rgba = ColorImage::from_rgba_unmultiplied(
                [self.view.width, self.view.height],
                &self.view.rgba,
            );
            if let Some(tex) = &mut self.model_texture {
                tex.set(rgba, TextureOptions::LINEAR);
            } else {
                self.model_texture =
                    Some(ctx.load_texture("kalium-model", rgba, TextureOptions::LINEAR));
            }
            self.model_dirty = false;
        }
    }
    fn start_new_project(&mut self) {
        self.finish_stroke();
        self.recovery_journal = None;
        self.pending_journal_recovery = None;
        if let Some(path) = self.recovery_journal_path.take() {
            let _ = std::fs::remove_file(recovery_journal_disabled_path(&path));
            let _ = std::fs::remove_file(path);
        }
        self.doc = Document::new(512);
        self.texture_set_name_input = self.doc.active_set_name.clone();
        self.history.clear();
        self.file_path = None;
        self.unsaved = false;
        self.view = Viewport::new(600, 600);
        self.rebuild_coverage();
        self.change();
        self.unsaved = false;
        self.status = "New Kalium project created".into();
    }

    fn install_project(&mut self, doc: Document, path: PathBuf) {
        self.finish_stroke();
        self.recovery_journal = None;
        self.recovery_journal_path = None;
        self.pending_journal_recovery = None;
        self.doc = doc;
        self.texture_set_name_input = self.doc.active_set_name.clone();
        self.rebuild_coverage();
        self.history.clear();
        self.file_path = Some(path.clone());
        self.change();
        self.unsaved = false;
        self.status = format!("Opened {}", path.display());
    }

    fn install_recovered_project(&mut self, doc: Document, path: PathBuf) {
        self.install_project(doc, path.clone());
        // The validated sibling temp is a recovery source, not the current
        // primary file. Require Save As so the damaged primary is never
        // silently overwritten by a later ordinary Save command.
        self.file_path = None;
        self.unsaved = true;
        self.status = format!(
            "Recovered project data from {}; use Save As to preserve it",
            path.display()
        );
    }

    fn install_open_project(&mut self, doc: Document, path: PathBuf, recovered: bool) {
        if recovered {
            self.install_recovered_project(doc, path);
        } else {
            self.install_project(doc, path);
            self.configure_recovery_journal();
        }
    }

    fn configure_recovery_journal(&mut self) {
        const JOURNAL_SCHEMA: u32 = 1;
        let Some(project_path) = self.file_path.clone() else {
            return;
        };
        if !project_path.is_file() {
            return;
        }
        let journal_path = recovery_journal_path(&project_path);
        if recovery_journal_disabled_path(&journal_path).exists() {
            self.recovery_journal_path = Some(journal_path);
            self.status = "Crash recovery is paused because this project contains edits not yet covered by journal events. Save to start a new recovery baseline.".into();
            return;
        }
        let digest = match project::file_digest(&project_path) {
            Ok(digest) => digest,
            Err(error) => {
                self.status = format!("Crash recovery is unavailable: {error:#}");
                return;
            }
        };
        self.recovery_journal_path = Some(journal_path.clone());
        if journal_path.exists() {
            match Journal::open(&journal_path, self.doc.project_id, digest, JOURNAL_SCHEMA) {
                Ok((journal, records)) => {
                    let mut has_events = false;
                    for record in &records {
                        match recovery::decode_event_batch_bounded(&record.payload) {
                            Ok(batch) => has_events |= !batch.is_empty(),
                            Err(error) => {
                                self.status =
                                    format!("Crash recovery journal was rejected: {error}");
                                return;
                            }
                        }
                    }
                    if !has_events {
                        self.recovery_journal = Some(journal);
                    } else {
                        self.pending_journal_recovery = Some(PendingJournalRecovery {
                            project_path,
                            journal_path,
                            records,
                            journal,
                        });
                    }
                }
                Err(error) => {
                    self.status = format!("Crash recovery journal was rejected: {error}");
                }
            }
        } else if let Err(error) = self.create_recovery_journal(&journal_path, digest) {
            self.status = format!("Crash recovery is unavailable: {error:#}");
        }
    }

    fn create_recovery_journal(
        &mut self,
        journal_path: &std::path::Path,
        digest: [u8; 32],
    ) -> anyhow::Result<()> {
        const JOURNAL_SCHEMA: u32 = 1;
        if let Some(parent) = journal_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        self.recovery_journal = Some(Journal::create(
            journal_path,
            self.doc.project_id,
            digest,
            JOURNAL_SCHEMA,
        )?);
        self.recovery_journal_path = Some(journal_path.to_owned());
        Ok(())
    }

    fn reset_recovery_journal_after_save(&mut self, path: &std::path::Path) -> anyhow::Result<()> {
        let journal_path = recovery_journal_path(path);
        self.recovery_journal = None;
        self.pending_journal_recovery = None;
        let disabled_path = recovery_journal_disabled_path(&journal_path);
        if disabled_path.exists() {
            std::fs::remove_file(disabled_path)?;
        }
        if journal_path.exists() {
            std::fs::remove_file(&journal_path)?;
        }
        let digest = project::file_digest(path)?;
        self.create_recovery_journal(&journal_path, digest)
    }

    fn disable_recovery_journal(&mut self, reason: &str) {
        self.recovery_journal = None;
        let Some(journal_path) = self.recovery_journal_path.as_ref() else {
            return;
        };
        let disabled_path = recovery_journal_disabled_path(journal_path);
        match std::fs::write(disabled_path, reason.as_bytes()) {
            Ok(()) => self.status = format!("Crash recovery paused: {reason}"),
            Err(error) => {
                let remove_result = std::fs::remove_file(journal_path);
                self.status = match remove_result {
                    Ok(()) => format!(
                        "Crash recovery paused: {reason}; the incomplete journal was removed because its disabled marker could not be written: {error}"
                    ),
                    Err(remove_error) => format!(
                        "Crash recovery could not be safely disabled: {reason}; writing its marker failed ({error}) and removing the journal failed ({remove_error})"
                    ),
                };
            }
        }
    }

    fn append_recovery_batch(&mut self, events: &[DocumentEvent]) {
        if events.is_empty() {
            return;
        }
        if self.recovery_journal.is_none() {
            return;
        }
        let payload = match recovery::encode_event_batch_bounded(events) {
            Ok(payload) => payload,
            Err(error) => {
                self.disable_recovery_journal(&format!("could not encode the edit: {error}"));
                return;
            }
        };
        let result = self
            .recovery_journal
            .as_mut()
            .expect("journal was checked above")
            .append(&payload);
        if let Err(error) = result {
            self.disable_recovery_journal(&format!("journal append failed: {error}"));
        }
    }

    fn commit_image_snapshot(&mut self, snapshot: Snapshot) {
        let recovery_events = self.recovery_journal.as_ref().map(|_| {
            History::snapshot_recovery_events(&snapshot, &self.doc, self.doc.active_set_id)
        });
        self.history.commit(snapshot);
        match recovery_events {
            Some(Some(events)) => {
                self.append_recovery_batch(&events);
                self.allow_recovery_change = self.recovery_journal.is_some();
            }
            Some(None) => {
                self.disable_recovery_journal(
                    "Committed image edit could not be encoded as recovery patches",
                );
            }
            None => {}
        }
    }

    fn resolve_journal_recovery(&mut self, choice: JournalRecoveryChoice) {
        let Some(pending) = self.pending_journal_recovery.take() else {
            return;
        };
        match choice {
            JournalRecoveryChoice::Restore => {
                let replay = pending
                    .records
                    .iter()
                    .try_for_each(|record| {
                        let events = recovery::decode_event_batch_bounded(&record.payload)?;
                        for event in events {
                            event.apply(&mut self.doc)?;
                        }
                        Ok::<(), anyhow::Error>(())
                    })
                    .and_then(|()| self.doc.validate());
                if let Err(error) = replay {
                    let project_path = pending.project_path.clone();
                    let journal_path = pending.journal_path.clone();
                    match project::load(&pending.project_path) {
                        Ok(base) => self.install_project(base, project_path),
                        Err(load_error) => {
                            self.recovery_journal = None;
                            self.recovery_journal_path = Some(journal_path);
                            self.status = format!(
                                "Journal replay failed ({error:#}); reopening the saved project also failed ({load_error:#})"
                            );
                            return;
                        }
                    }
                    self.recovery_journal_path = Some(journal_path);
                    self.disable_recovery_journal(
                        "journal replay failed validation; the saved project was reloaded",
                    );
                    self.status = format!(
                        "Crash recovery was rejected and the saved project was reloaded: {error:#}"
                    );
                    return;
                }
                self.recovery_journal = Some(pending.journal);
                self.recovery_journal_path = Some(pending.journal_path);
                self.allow_recovery_change = true;
                self.change();
                self.status = "Unsaved edits restored from the recovery journal".into();
            }
            JournalRecoveryChoice::Discard => {
                drop(pending.journal);
                match std::fs::remove_file(&pending.journal_path) {
                    Ok(()) => {
                        self.recovery_journal = None;
                        let digest = project::file_digest(&pending.project_path);
                        match digest.and_then(|digest| {
                            self.create_recovery_journal(&pending.journal_path, digest)
                        }) {
                            Ok(()) => self.status = "Recovery journal discarded".into(),
                            Err(error) => {
                                self.recovery_journal = None;
                                self.status =
                                    format!("Journal discarded; recovery unavailable: {error:#}");
                            }
                        }
                    }
                    Err(error) => {
                        self.recovery_journal_path = Some(pending.journal_path);
                        self.disable_recovery_journal(&format!(
                            "could not discard the existing journal: {error}"
                        ));
                    }
                }
            }
        }
    }

    fn save_project(&mut self, save_as: bool) -> anyhow::Result<Option<String>> {
        self.finish_stroke();
        let path = if save_as {
            None
        } else {
            self.file_path.clone()
        };
        let path = match path {
            Some(path) => path,
            None => {
                let Some(path) = rfd::FileDialog::new()
                    .add_filter("Kalium project", &["kalium"])
                    .set_file_name("Untitled.kalium")
                    .save_file()
                else {
                    return Ok(None);
                };
                path
            }
        };
        let previous_project_id = self.doc.project_id;
        self.doc.project_id =
            project_id_for_save_target(previous_project_id, self.file_path.as_deref(), &path);
        if let Err(error) = project::save(&self.doc, &path) {
            self.doc.project_id = previous_project_id;
            return Err(error);
        }
        self.file_path = Some(path.clone());
        self.unsaved = false;
        let saved_message = format!("Saved {}", path.display());
        if let Err(error) = self.reset_recovery_journal_after_save(&path) {
            self.recovery_journal = None;
            self.recovery_journal_path = Some(recovery_journal_path(&path));
            return Ok(Some(format!(
                "{saved_message}; crash recovery is unavailable: {error:#}"
            )));
        }
        Ok(Some(saved_message))
    }

    fn resolve_destructive_choice(&mut self, choice: DestructiveChoice, ctx: &egui::Context) {
        if choice == DestructiveChoice::Cancel {
            self.pending_destructive = None;
            self.status = "Current project kept".into();
            return;
        }

        let saved = if choice == DestructiveChoice::Save {
            match self.save_project(false) {
                Ok(Some(message)) => {
                    self.status = message;
                    true
                }
                Ok(None) => {
                    self.status = "Save canceled; current project kept".into();
                    return;
                }
                Err(error) => {
                    self.status = format!("Error saving project: {error:#}");
                    return;
                }
            }
        } else {
            false
        };

        if !should_continue_after_choice(choice, saved) {
            return;
        }
        if choice == DestructiveChoice::Discard {
            self.recovery_journal = None;
            if let Some(path) = self.recovery_journal_path.take() {
                let _ = std::fs::remove_file(recovery_journal_disabled_path(&path));
                let _ = std::fs::remove_file(path);
            }
        }
        if let Some(action) = self.pending_destructive.take() {
            match action {
                DestructiveAction::New => self.start_new_project(),
                DestructiveAction::Open {
                    doc,
                    path,
                    recovered,
                } => self.install_open_project(*doc, path, recovered),
                DestructiveAction::Close => {
                    self.allow_close = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }
    }

    fn unsaved_changes_dialog(&mut self, ctx: &egui::Context) {
        if self.pending_destructive.is_none() {
            return;
        }

        let mut choice = None;
        egui::Modal::new(egui::Id::new("unsaved-changes")).show(ctx, |ui| {
            ui.heading("Unsaved changes");
            ui.vertical(|ui| {
                ui.label("The current project has unsaved changes.");
                ui.label("Save before continuing?");
                ui.horizontal(|ui| {
                    if ui.button("Save").clicked() {
                        choice = Some(DestructiveChoice::Save);
                    }
                    if ui.button("Discard").clicked() {
                        choice = Some(DestructiveChoice::Discard);
                    }
                    if ui.button("Cancel").clicked() {
                        choice = Some(DestructiveChoice::Cancel);
                    }
                });
            });
        });
        if let Some(choice) = choice {
            self.resolve_destructive_choice(choice, ctx);
        }
    }

    fn journal_recovery_dialog(&mut self, ctx: &egui::Context) {
        if self.pending_journal_recovery.is_none() {
            return;
        }
        let mut choice = None;
        egui::Modal::new(egui::Id::new("journal-recovery")).show(ctx, |ui| {
            ui.heading("Unsaved recovery data found");
            ui.label("A validated recovery journal contains edits made after the last save.");
            ui.horizontal(|ui| {
                if ui.button("Restore edits").clicked() {
                    choice = Some(JournalRecoveryChoice::Restore);
                }
                if ui.button("Discard recovery data").clicked() {
                    choice = Some(JournalRecoveryChoice::Discard);
                }
            });
        });
        if let Some(choice) = choice {
            self.resolve_journal_recovery(choice);
        }
    }

    fn perform(&mut self, action: Action) {
        let result: anyhow::Result<Option<String>> = (|| match action {
            Action::New => {
                self.finish_stroke();
                if self.unsaved {
                    self.pending_destructive = Some(DestructiveAction::New);
                    Ok(Some("Choose how to handle unsaved changes".into()))
                } else {
                    self.start_new_project();
                    Ok(Some(self.status.clone()))
                }
            }
            Action::Open => {
                let Some(path) = rfd::FileDialog::new()
                    .add_filter("Kalium project", &["kalium"])
                    .pick_file()
                else {
                    return Ok(None);
                };
                let (doc, recovered) = match project::load_with_recovery(&path)? {
                    project::RecoveryOutcome::Primary(doc) => (doc, false),
                    project::RecoveryOutcome::RecoveredTemporary(doc) => (doc, true),
                };
                self.finish_stroke();
                if self.unsaved {
                    self.pending_destructive = Some(DestructiveAction::Open {
                        doc: Box::new(doc),
                        path,
                        recovered,
                    });
                    Ok(Some("Choose how to handle unsaved changes".into()))
                } else {
                    self.install_open_project(doc, path, recovered);
                    Ok(Some(self.status.clone()))
                }
            }
            Action::Save | Action::SaveAs => self.save_project(matches!(action, Action::SaveAs)),
            Action::SaveCheckpoint => {
                let path = layout::recovery_checkpoint_file()
                    .ok_or_else(|| anyhow::anyhow!("Cannot locate the user config directory"))?;
                if self
                    .file_path
                    .as_deref()
                    .is_some_and(|primary| paths_refer_to_same_file(primary, &path))
                {
                    anyhow::bail!(
                        "The recovery checkpoint path is the open project; use Save As first"
                    );
                }
                self.finish_stroke();
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                project::save_checkpoint(&self.doc, &path)?;
                Ok(Some(format!(
                    "Recovery checkpoint saved to {}",
                    path.display()
                )))
            }
            Action::RecoverCheckpoint => {
                let path = layout::recovery_checkpoint_file()
                    .ok_or_else(|| anyhow::anyhow!("Cannot locate the user config directory"))?;
                if !path.exists() {
                    anyhow::bail!("No recovery checkpoint exists yet");
                }
                let doc = project::load_checkpoint(&path)?;
                self.finish_stroke();
                if self.unsaved {
                    self.pending_destructive = Some(DestructiveAction::Open {
                        doc: Box::new(doc),
                        path,
                        recovered: true,
                    });
                    Ok(Some("Choose how to handle unsaved changes".into()))
                } else {
                    self.install_recovered_project(doc, path.clone());
                    Ok(Some(format!(
                        "Recovered checkpoint from {}; use Save As to preserve it",
                        path.display()
                    )))
                }
            }
            Action::ImportMesh => {
                let Some(path) = rfd::FileDialog::new()
                    .add_filter("3D mesh", &["obj", "gltf", "glb"])
                    .pick_file()
                else {
                    return Ok(None);
                };
                let mesh = Mesh::from_file(&path)?;
                self.finish_stroke();
                self.doc.import_mesh(mesh)?;
                self.rebuild_coverage();
                self.history.clear();
                self.change();
                Ok(Some(format!("Imported {}", path.display())))
            }
            Action::ImportTexture => {
                let Some(path) = rfd::FileDialog::new()
                    .add_filter("Image texture", &["png", "jpg", "jpeg"])
                    .pick_file()
                else {
                    return Ok(None);
                };
                self.finish_stroke();
                ops::import_texture_with_history(
                    &mut self.doc,
                    &mut self.history,
                    self.channel,
                    &path,
                )?;
                self.change();
                Ok(Some(format!(
                    "Imported texture {} as new {} layer",
                    path.display(),
                    self.channel.label()
                )))
            }
            Action::LoadBrush => {
                let Some(path) = rfd::FileDialog::new()
                    .add_filter("Kalium brush preset", &["kbrush"])
                    .pick_file()
                else {
                    return Ok(None);
                };
                self.finish_stroke();
                let (name, brush) = brush_preset::load(&path)?;
                self.brush = brush;
                Ok(Some(format!("Loaded brush preset {name}")))
            }
            Action::SaveBrush => {
                let Some(path) = rfd::FileDialog::new()
                    .add_filter("Kalium brush preset", &["kbrush"])
                    .set_file_name("MyBrush.kbrush")
                    .save_file()
                else {
                    return Ok(None);
                };
                let name = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("MyBrush");
                brush_preset::save(&self.brush, name, &path)?;
                Ok(Some(format!("Saved brush preset {}", path.display())))
            }
            Action::Fill => {
                self.finish_stroke();
                let mode = self.brush.mode;
                let mask = mode.is_mask();
                let before = History::capture(&mut self.doc, self.channel, mode);
                let layer_id = self.doc.selected_layer().id;
                let changed = ops::fill_selected(&mut self.doc, self.channel, &self.brush);
                if changed {
                    let after = History::capture(&mut self.doc, self.channel, mode);
                    self.commit_image_snapshot(Snapshot {
                        layer_id,
                        channel: self.channel,
                        mask,
                        before,
                        after,
                    });
                    self.change();
                }
                Ok(Some(if changed {
                    "Filled active layer channel or mask".into()
                } else {
                    "Nothing to fill (already identical)".into()
                }))
            }
            Action::ClearChannel => {
                self.finish_stroke();
                let mode = BrushMode::Paint;
                let before = History::capture(&mut self.doc, self.channel, mode);
                let layer_id = self.doc.selected_layer().id;
                let mut brush = self.brush.clone();
                brush.mode = BrushMode::Erase;
                if ops::fill_selected(&mut self.doc, self.channel, &brush) {
                    let after = History::capture(&mut self.doc, self.channel, mode);
                    self.commit_image_snapshot(Snapshot {
                        layer_id,
                        channel: self.channel,
                        mask: false,
                        before,
                        after,
                    });
                    self.change();
                }
                Ok(Some(format!(
                    "Cleared {} on selected layer",
                    self.channel.label()
                )))
            }
            Action::InvertMask | Action::ResetMask => {
                self.finish_stroke();
                let mode = BrushMode::MaskWhite;
                let before = History::capture(&mut self.doc, self.channel, mode);
                let layer_id = self.doc.selected_layer().id;
                let changed = if matches!(action, Action::InvertMask) {
                    self.doc.invert_selected_mask()
                } else {
                    let mut brush = self.brush.clone();
                    brush.mode = BrushMode::MaskWhite;
                    ops::fill_selected(&mut self.doc, self.channel, &brush)
                };
                if changed {
                    let after = History::capture(&mut self.doc, self.channel, mode);
                    self.commit_image_snapshot(Snapshot {
                        layer_id,
                        channel: self.channel,
                        mask: true,
                        before,
                        after,
                    });
                    self.change();
                }
                Ok(Some(if matches!(action, Action::InvertMask) {
                    "Inverted selected layer mask".into()
                } else {
                    "Reset layer mask to white".into()
                }))
            }
            Action::Export => {
                self.finish_stroke();
                let Some(dir) = rfd::FileDialog::new().pick_folder() else {
                    return Ok(None);
                };
                self.export_padding_radius = 0;
                self.pending_export_all = false;
                self.pending_export_dir = Some(dir);
                Ok(Some("Choose Normal-map convention, then export".into()))
            }
            Action::ExportAll => {
                self.finish_stroke();
                let Some(dir) = rfd::FileDialog::new().pick_folder() else {
                    return Ok(None);
                };
                self.export_padding_radius = 0;
                self.pending_export_all = true;
                self.pending_export_dir = Some(dir);
                Ok(Some(
                    "Choose Normal-map convention and per-set UV padding, then export".into(),
                ))
            }
            Action::InspectSpp => {
                let Some(path) = rfd::FileDialog::new()
                    .add_filter("Substance Painter project", &["spp"])
                    .pick_file()
                else {
                    return Ok(None);
                };
                let report = spp::inspect(&path)?;
                Ok(Some(format!("SPP inspection: {report}")))
            }
        })();
        match result {
            Ok(Some(s)) => self.status = s,
            Ok(None) => {}
            Err(e) => self.status = format!("Error: {e:#}"),
        }
    }
    fn menu(&mut self, ctx: &egui::Context) {
        let mut action = None;
        egui::TopBottomPanel::top("menu").show(ctx, |ui| {
            egui::menu::bar(ui, |ui| {
                ui.heading(egui::RichText::new("◆ Kalium").color(Color32::from_rgb(156, 201, 248)));
                ui.separator();
                ui.menu_button("File", |ui| {
                    for (label, a) in [
                        ("New", Action::New),
                        ("Open .kalium…", Action::Open),
                        ("Save", Action::Save),
                        ("Save As…", Action::SaveAs),
                        ("Save recovery checkpoint", Action::SaveCheckpoint),
                        ("Recover global checkpoint…", Action::RecoverCheckpoint),
                        ("Import OBJ / glTF / GLB…", Action::ImportMesh),
                        ("Import PNG / JPEG as layer…", Action::ImportTexture),
                        ("Load brush preset (.kbrush)…", Action::LoadBrush),
                        ("Save brush preset (.kbrush)…", Action::SaveBrush),
                        ("Export active set PBR textures…", Action::Export),
                        ("Export all texture sets…", Action::ExportAll),
                        ("Clear active channel", Action::ClearChannel),
                        ("Inspect SPP signature (experimental)…", Action::InspectSpp),
                    ] {
                        if ui.button(label).clicked() {
                            action = Some(a);
                            ui.close_menu();
                        }
                    }
                });
                ui.menu_button("View", |ui| {
                    if ui.button("Restore default workspace").clicked() {
                        self.dock = layout::default_workspace();
                        self.status = "Default workspace restored".into();
                        ui.close_menu();
                    }
                    if ui.button("Save workspace now").clicked() {
                        match layout::persist_workspace(&self.dock) {
                            Ok(_) => self.status = "Workspace saved".into(),
                            Err(err) => self.status = format!("Cannot save workspace: {err}"),
                        }
                        ui.close_menu();
                    }
                    ui.separator();
                    ui.checkbox(&mut self.show_tool_shelf, "Show tool shelf");
                    ui.label("Show / hide panels");
                    for pane in Pane::ALL {
                        let mut visible = self.dock.find_tab(&pane).is_some();
                        if ui.checkbox(&mut visible, pane.label()).changed() {
                            if visible {
                                self.dock.push_to_first_leaf(pane);
                            } else {
                                self.dock.retain_tabs(|tab| *tab != pane);
                            }
                        }
                    }
                });
                ui.separator();
                if ui.button("Undo").clicked() && self.history.undo(&mut self.doc) {
                    self.change();
                }
                if ui.button("Redo").clicked() && self.history.redo(&mut self.doc) {
                    self.change();
                }
                ui.menu_button("Layer", |ui| {
                    if ui.button("Add paint layer").clicked() {
                        self.layer_command(LayerCommand::Add);
                        ui.close_menu();
                    }
                    if ui.button("Duplicate selected layer").clicked() {
                        self.layer_command(LayerCommand::Duplicate);
                        ui.close_menu();
                    }
                    if ui.button("Remove selected layer").clicked() {
                        self.layer_command(LayerCommand::Remove);
                        ui.close_menu();
                    }
                    ui.separator();
                    if ui.button("Invert mask").clicked() {
                        action = Some(Action::InvertMask);
                        ui.close_menu();
                    }
                    if ui.button("Reset mask").clicked() {
                        action = Some(Action::ResetMask);
                        ui.close_menu();
                    }
                    if ui.button("Clear active channel").clicked() {
                        action = Some(Action::ClearChannel);
                        ui.close_menu();
                    }
                });
                ui.separator();
                ui.label(egui::RichText::new("AGPLv3 • Offline").small().weak());
            });
            ui.separator();
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("TOOLS").small().weak());
                for (label, mode) in [
                    ("Brush", BrushMode::Paint),
                    ("Eraser", BrushMode::Erase),
                    ("Sample", BrushMode::Sample),
                    ("Fill", BrushMode::Bucket),
                    ("Mask +", BrushMode::MaskWhite),
                    ("Mask −", BrushMode::MaskBlack),
                ] {
                    if ui
                        .selectable_label(self.brush.mode == mode, label)
                        .clicked()
                    {
                        self.finish_stroke();
                        self.brush.mode = mode;
                    }
                }
                ui.separator();
                ui.label(format!(
                    "{}  |  {} px",
                    self.channel.label(),
                    self.brush.radius
                ));
            });
        });
        if let Some(a) = action {
            self.perform(a);
        }
        let keys = ctx.input(|i| {
            let cmd = i.modifiers.command;
            if cmd && i.key_pressed(egui::Key::S) {
                Some(if i.modifiers.shift {
                    Action::SaveAs
                } else {
                    Action::Save
                })
            } else if cmd && i.key_pressed(egui::Key::O) {
                Some(Action::Open)
            } else {
                None
            }
        });
        if let Some(action) = keys {
            self.perform(action);
        }
        let (undo, redo) = ctx.input(|i| {
            (
                i.modifiers.command && i.key_pressed(egui::Key::Z) && !i.modifiers.shift,
                i.modifiers.command
                    && (i.key_pressed(egui::Key::Y)
                        || (i.modifiers.shift && i.key_pressed(egui::Key::Z))),
            )
        });
        if undo {
            self.finish_stroke();
            if self.history.undo(&mut self.doc) {
                self.change();
            }
        }
        if redo {
            self.finish_stroke();
            if self.history.redo(&mut self.doc) {
                self.change();
            }
        }
        // Painting shortcuts do not steal keystrokes while editing text in panel controls.
        if !ctx.wants_keyboard_input() {
            let shortcut = ctx.input(|i| {
                if i.key_pressed(egui::Key::B) {
                    Some(BrushMode::Paint)
                } else if i.key_pressed(egui::Key::E) {
                    Some(BrushMode::Erase)
                } else if i.key_pressed(egui::Key::I) {
                    Some(BrushMode::Sample)
                } else if i.key_pressed(egui::Key::G) {
                    Some(BrushMode::Bucket)
                } else {
                    None
                }
            });
            if let Some(mode) = shortcut {
                self.finish_stroke();
                self.brush.mode = mode;
            }
        }
    }

    fn export_options_dialog(&mut self, ctx: &egui::Context) {
        let Some(dir) = self.pending_export_dir.clone() else {
            return;
        };
        let mut choice = None;
        egui::Modal::new(egui::Id::new("export-options")).show(ctx, |ui| {
            ui.heading(if self.pending_export_all {
                "Export all texture sets"
            } else {
                "Export active texture set"
            });
            ui.label("Optional: copy nearest UV-covered texels into the gutter.");
            ui.add(
                egui::Slider::new(
                    &mut self.export_padding_radius,
                    0..=export::MAX_PADDING_RADIUS,
                )
                .suffix(" texels")
                .text("UV edge padding"),
            );
            egui::ComboBox::from_id_salt("normal-map-y-convention")
                .selected_text(self.export_normal_y.label())
                .show_ui(ui, |ui| {
                    for convention in export::NormalYConvention::ALL {
                        ui.selectable_value(
                            &mut self.export_normal_y,
                            convention,
                            convention.label(),
                        );
                    }
                });
            ui.horizontal(|ui| {
                if ui.button("Export").clicked() {
                    choice = Some(true);
                }
                if ui.button("Cancel").clicked() {
                    choice = Some(false);
                }
            });
        });
        match choice {
            Some(true) => {
                self.pending_export_dir = None;
                let options = export::ExportOptions {
                    padding_radius: self.export_padding_radius,
                    normal_y: self.export_normal_y,
                };
                let export_result = if self.pending_export_all {
                    export::export_all_texture_sets_with_options(&self.doc, &dir, options)
                } else {
                    export::export_textures_with_options(&self.doc, &dir, options)
                };
                match export_result {
                    Ok(count) => {
                        self.status = if self.pending_export_all {
                            format!(
                                "Exported {count} texture maps from {} sets into {}",
                                self.doc.texture_set_count(),
                                dir.display()
                            )
                        } else {
                            format!("Exported {count} texture maps into {}", dir.display())
                        };
                    }
                    Err(error) => self.status = format!("Export failed: {error:#}"),
                }
            }
            Some(false) => {
                self.pending_export_dir = None;
                self.status = "Texture-set export canceled".into();
            }
            None => {}
        }
    }
    fn tool_shelf(&mut self, ctx: &egui::Context) {
        if !self.show_tool_shelf {
            return;
        }
        egui::SidePanel::left("kalium-tool-shelf")
            .exact_width(48.0)
            .frame(egui::Frame::NONE.inner_margin(egui::Margin::symmetric(3, 5)))
            .show(ctx, |ui| {
                ui.vertical_centered(|ui| {
                    for (icon, mode, help) in [
                        ("✎", BrushMode::Paint, "Paint (B)"),
                        ("⌫", BrushMode::Erase, "Eraser (E)"),
                        ("◎", BrushMode::Sample, "Sample (I)"),
                        ("▧", BrushMode::Bucket, "Connected fill (G)"),
                        ("◐", BrushMode::MaskWhite, "Paint mask white"),
                        ("◑", BrushMode::MaskBlack, "Paint mask black"),
                    ] {
                        if ui
                            .add_sized(
                                [37.0, 34.0],
                                egui::Button::new(icon).selected(self.brush.mode == mode),
                            )
                            .on_hover_text(help)
                            .clicked()
                        {
                            self.finish_stroke();
                            self.brush.mode = mode;
                        }
                    }
                });
            });
    }
    fn tools(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().show(ui,|ui|{
            ui.heading("Brush Studio");
            ui.label(egui::RichText::new("PBR CHANNEL").small().weak());
            egui::ComboBox::from_id_salt("channel").selected_text(self.channel.label())
                .show_ui(ui,|ui|{
                    for channel in Channel::ALL {
                        if ui.selectable_value(&mut self.channel,channel,channel.label()).changed() {
                            self.uv_texture=None;
                            if self.solo_channel {self.model_dirty=true;}
                        }
                    }
                });
            ui.separator();
            ui.label(egui::RichText::new("TOOL").small().weak());
            egui::ComboBox::from_id_salt("mode").selected_text(self.brush.mode.label())
                .show_ui(ui,|ui|{
                    for mode in BrushMode::ALL {ui.selectable_value(&mut self.brush.mode,mode,mode.label());}
                });
            ui.add(egui::Slider::new(&mut self.brush.radius,1..=180).text("Radius (px)"));
            ui.add(egui::Slider::new(&mut self.brush.opacity,0.01..=1.0).text("Opacity"));
            ui.add(egui::Slider::new(&mut self.brush.hardness,0.0..=1.0).text("Hardness"));
            ui.add(egui::Slider::new(&mut self.brush.spacing,0.1..=1.5).text("Spacing"));
            if self.brush.mode==BrushMode::Bucket {
                ui.add(egui::Slider::new(&mut self.brush.tolerance,0..=255).text("Color tolerance"));
            }
            ui.checkbox(&mut self.brush.mirror_u,"Mirror across U axis");
            ui.separator();
            if self.channel.is_color() {
                ui.label("Paint color");
                let mut color=Color32::from_rgb(self.brush.color[0],self.brush.color[1],self.brush.color[2]);
                if ui.color_edit_button_srgba(&mut color).changed() {
                    self.brush.color=[color.r(),color.g(),color.b(),255];
                }
            } else {
                ui.label(format!("{} intensity",self.channel.label()));
                ui.add(egui::Slider::new(&mut self.brush.grayscale,0..=255));
            }
            if ui.button("Fill selected channel / mask").clicked() {self.perform(Action::Fill);}
            ui.horizontal(|ui| {
                if ui.button("Load brush…").clicked() {self.perform(Action::LoadBrush);}
                if ui.button("Save brush…").clicked() {self.perform(Action::SaveBrush);}
            });
            ui.separator();
            ui.heading("Display");
            if ui.checkbox(&mut self.solo_channel,"Show selected channel in 3D").changed(){self.model_dirty=true;}
            ui.checkbox(&mut self.show_uv_wire,"UV wireframe overlay");
            ui.add_enabled(self.uv_coverage.is_some(),egui::Checkbox::new(&mut self.uv_clipping,"Clip painting to UV polygons"));
            if let Some(coverage)=self.uv_coverage.as_ref() {
                ui.label(format!("UV atlas coverage: {:.1}%",100.0*coverage.covered_texels() as f32/(self.doc.size as f32).powi(2)));
            }
            ui.separator();
            ui.label(egui::RichText::new("TEXTURE RESOLUTION").small().weak());
            ui.label(format!("{} × {}",self.doc.size,self.doc.size));
            ui.horizontal_wrapped(|ui|{
                for size in [256,512,1024,2048,4096] {
                    if ui.selectable_label(self.doc.size==size,format!("{size}")).clicked() {
                        self.resize_document(size);
                    }
                }
            });
            ui.add_space(8.0);
            ui.label(egui::RichText::new("TIP").small().weak());
            ui.label("Left-drag paints on the 3D surface or UV image. Flood Fill uses one click. Right-drag rotates the model.");
            ui.label("Mouse wheel zooms. Ctrl+S saves. Ctrl+Z / Ctrl+Y undo or redo edits.");
        });
    }
    fn layer_command(&mut self, command: LayerCommand) {
        self.finish_stroke();
        let selected_before = self.doc.layers[self.doc.selected].id;
        let old_index = self.doc.selected;
        let set_id = self.doc.active_set_id;
        let mut recovery_events = Vec::new();
        let changed = match command {
            LayerCommand::Add => match self.doc.add_layer() {
                Ok(()) => {
                    let added_id = self.doc.layers[self.doc.selected].id;
                    self.history
                        .record_added_layer(&self.doc, added_id, selected_before, None);
                    true
                }
                Err(err) => {
                    self.status = err.to_string();
                    false
                }
            },
            LayerCommand::Duplicate => match self.doc.duplicate_selected() {
                Ok(()) => {
                    let duplicate_id = self.doc.layers[self.doc.selected].id;
                    self.history.record_added_layer(
                        &self.doc,
                        duplicate_id,
                        selected_before,
                        Some(selected_before),
                    );
                    true
                }
                Err(err) => {
                    self.status = err.to_string();
                    false
                }
            },
            LayerCommand::Remove => match self.history.remove_selected_layer(&mut self.doc) {
                Ok(changed) => changed,
                Err(error) => {
                    self.status = format!("Cannot delete this layer: {error}");
                    false
                }
            },
            LayerCommand::Up => {
                let layer_id = self.doc.layers[old_index].id;
                self.doc.move_selected(true);
                if self.doc.selected != old_index {
                    recovery_events.push(DocumentEvent::LayerReorder {
                        set_id,
                        layer_id,
                        to_index: self.doc.selected,
                    });
                    let selected_after = self.doc.layers[self.doc.selected].id;
                    self.history.record_moved_layer(
                        layer_id,
                        old_index,
                        self.doc.selected,
                        selected_before,
                        selected_after,
                    );
                    true
                } else {
                    false
                }
            }
            LayerCommand::Down => {
                let layer_id = self.doc.layers[old_index].id;
                self.doc.move_selected(false);
                if self.doc.selected != old_index {
                    recovery_events.push(DocumentEvent::LayerReorder {
                        set_id,
                        layer_id,
                        to_index: self.doc.selected,
                    });
                    let selected_after = self.doc.layers[self.doc.selected].id;
                    self.history.record_moved_layer(
                        layer_id,
                        old_index,
                        self.doc.selected,
                        selected_before,
                        selected_after,
                    );
                    true
                } else {
                    false
                }
            }
        };
        if changed {
            self.change_with_recovery_events(&recovery_events);
        }
    }
    fn move_layer_to(&mut self, from: usize, to: usize) -> bool {
        if from >= self.doc.layers.len() || to >= self.doc.layers.len() || from == to {
            return false;
        }
        self.finish_stroke();
        let selected_before = self.doc.layers[self.doc.selected].id;
        let moved_id = self.doc.layers[from].id;
        let layer = self.doc.layers.remove(from);
        self.doc.layers.insert(to, layer);
        self.doc.selected = self
            .doc
            .layers
            .iter()
            .position(|layer| layer.id == selected_before)
            .expect("reordering preserves the selected layer");
        let selected_after = self.doc.layers[self.doc.selected].id;
        self.history
            .record_moved_layer(moved_id, from, to, selected_before, selected_after);
        self.change_with_recovery_events(&[DocumentEvent::LayerReorder {
            set_id: self.doc.active_set_id,
            layer_id: moved_id,
            to_index: to,
        }]);
        true
    }
    fn resize_document(&mut self, size: u32) {
        if size == self.doc.size {
            return;
        }
        self.finish_stroke();
        match self.doc.try_resize(size) {
            Ok(()) => {
                self.rebuild_coverage();
                self.history.clear();
                self.change();
            }
            Err(err) => self.status = format!("Cannot resize textures: {err}"),
        }
    }
    fn layers(&mut self, ui: &mut egui::Ui) {
        let before_properties: Vec<_> = self
            .doc
            .layers
            .iter()
            .map(|layer| {
                (
                    layer.id,
                    LayerProperties::capture(layer),
                    LayerRecoveryProperties::capture(layer),
                )
            })
            .collect();
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("LAYERS").strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        egui::RichText::new(format!("{} layers", self.doc.layers.len()))
                            .small()
                            .weak(),
                    );
                });
            });
            let mut changed = false;
            ui.horizontal_wrapped(|ui| {
                for (label, help, command) in [
                    ("+", "Add paint layer", LayerCommand::Add),
                    ("⧉", "Duplicate selected layer", LayerCommand::Duplicate),
                    ("−", "Delete layer", LayerCommand::Remove),
                    ("↑", "Move up", LayerCommand::Up),
                    ("↓", "Move down", LayerCommand::Down),
                ] {
                    if ui.small_button(label).on_hover_text(help).clicked() {
                        self.layer_command(command);
                    }
                }
                if ui
                    .small_button("◐")
                    .on_hover_text("Invert selected mask")
                    .clicked()
                {
                    self.perform(Action::InvertMask);
                }
            });
            ui.separator();
            ui.horizontal(|ui| {
                egui::ComboBox::from_id_salt("layers-channel")
                    .selected_text(self.channel.label())
                    .show_ui(ui, |ui| {
                        for channel in Channel::ALL {
                            if ui
                                .selectable_value(&mut self.channel, channel, channel.label())
                                .changed()
                            {
                                self.uv_texture = None;
                                if self.solo_channel {
                                    self.model_dirty = true;
                                }
                            }
                        }
                    });
            });
            egui::ScrollArea::vertical()
                .max_height(360.0)
                .show(ui, |ui| {
                    for index in (0..self.doc.layers.len()).rev() {
                        let layer = &mut self.doc.layers[index];
                        let layer_id = layer.id;
                        let is_selected = self.doc.selected == index;
                        let title = if layer.pixels.contains_key(&self.channel) {
                            format!("▣ {}", layer.name)
                        } else {
                            format!("□ {}", layer.name)
                        };
                        let row = ui.dnd_drag_source(
                            egui::Id::new(("layer-row-drag", layer_id)),
                            index,
                            |ui| {
                                ui.horizontal(|ui| {
                                    if ui
                                        .checkbox(&mut layer.visible, "")
                                        .on_hover_text("Toggle visibility")
                                        .changed()
                                    {
                                        changed = true;
                                    }
                                    if ui.selectable_label(is_selected, title).clicked() {
                                        self.doc.selected = index;
                                    }
                                    if !layer.mask.pixels().all(|p| p.0[0] == 255) {
                                        ui.label(egui::RichText::new("◐").small())
                                            .on_hover_text("Masked layer");
                                    }
                                })
                            },
                        );
                        let drop_target = ui.interact(
                            row.response.rect,
                            egui::Id::new(("layer-row-drop", layer_id)),
                            egui::Sense::hover(),
                        );
                        if let Some(from) = drop_target.dnd_release_payload::<usize>() {
                            self.move_layer_to(*from, index);
                        }
                        if let Some(from) = drop_target.dnd_hover_payload::<usize>() {
                            if *from != index {
                                ui.painter().hline(
                                    drop_target.rect.x_range(),
                                    drop_target.rect.bottom(),
                                    ui.visuals().widgets.active.bg_stroke,
                                );
                            }
                        }
                    }
                });
            ui.separator();
            ui.label("Selected layer");
            if ui
                .text_edit_singleline(&mut self.doc.selected_layer_mut().name)
                .changed()
            {
                changed = true;
            }
            let layer = self.doc.selected_layer_mut();
            if ui
                .add(egui::Slider::new(&mut layer.opacity, 0.0..=1.0).text("Opacity"))
                .changed()
            {
                changed = true;
            }
            egui::ComboBox::from_id_salt("blend_mode")
                .selected_text(layer.blend.label())
                .show_ui(ui, |ui| {
                    for blend in BlendMode::ALL {
                        if ui
                            .selectable_value(&mut layer.blend, blend, blend.label())
                            .changed()
                        {
                            changed = true;
                        }
                    }
                });
            ui.separator();
            ui.label(
                egui::RichText::new("Layer channels (Painter-style toggles):")
                    .small()
                    .weak(),
            );
            ui.label(
                egui::RichText::new("Disabled channels keep their pixels for later reactivation.")
                    .small()
                    .weak(),
            );
            ui.horizontal_wrapped(|ui| {
                for c in Channel::ALL {
                    let mut enabled = !layer.disabled_channels.contains(&c);
                    let has_pixels = layer.pixels.contains_key(&c);
                    let label = format!("{}{}", if has_pixels { "● " } else { "○ " }, c.label());
                    if ui.checkbox(&mut enabled, label).changed() {
                        if enabled {
                            layer.disabled_channels.remove(&c);
                        } else {
                            layer.disabled_channels.insert(c);
                        }
                        changed = true;
                    }
                }
            });
            ui.horizontal_wrapped(|ui| {
                if ui.small_button("Invert mask").clicked() {
                    self.perform(Action::InvertMask);
                }
                if ui.small_button("Reset mask").clicked() {
                    self.perform(Action::ResetMask);
                }
                if ui.small_button("Clear channel").clicked() {
                    self.perform(Action::ClearChannel);
                }
            });
            ui.add_space(10.0);
            ui.label(
                egui::RichText::new("Layer masks are editable with Mask + and Mask − brush modes.")
                    .small()
                    .weak(),
            );
            if changed {
                let set_id = self.doc.active_set_id;
                let mut recovery_events = Vec::new();
                for (layer_id, before, recovery_before) in before_properties {
                    if let Some(layer) = self.doc.layers.iter().find(|layer| layer.id == layer_id) {
                        self.history.record_properties(
                            layer_id,
                            before,
                            LayerProperties::capture(layer),
                        );
                        recovery_events.extend(recovery_before.events_since(set_id, layer));
                    }
                }
                self.change_with_recovery_events(&recovery_events);
            }
        });
    }

    fn assets(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("Library");
            if ui.small_button("+ Import").clicked() {
                self.perform(Action::ImportTexture);
            }
        });
        ui.add(
            egui::TextEdit::singleline(&mut self.asset_search)
                .hint_text("Search brushes and materials…"),
        );
        ui.separator();
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.label(egui::RichText::new("BRUSH PRESETS").small().weak());
            let presets = [
                (
                    "Soft Painter",
                    42,
                    0.50,
                    0.15,
                    Color32::from_rgb(150, 168, 178),
                ),
                (
                    "Hard Detail",
                    8,
                    1.0,
                    0.95,
                    Color32::from_rgb(202, 178, 156),
                ),
                (
                    "Surface Color",
                    32,
                    0.85,
                    0.65,
                    Color32::from_rgb(171, 108, 88),
                ),
            ];
            ui.columns(2, |cols| {
                for (index, (label, radius, opacity, hardness, swatch)) in
                    presets.into_iter().enumerate()
                {
                    if !label
                        .to_ascii_lowercase()
                        .contains(&self.asset_search.to_ascii_lowercase())
                    {
                        continue;
                    }
                    cols[index % 2].vertical_centered(|ui| {
                        let (rect, clicked) =
                            ui.allocate_exact_size(egui::vec2(76.0, 72.0), egui::Sense::click());
                        ui.painter().circle_filled(rect.center(), 26.0, swatch);
                        ui.painter().circle_stroke(
                            rect.center(),
                            26.0,
                            egui::Stroke::new(1.2_f32, Color32::from_gray(204)),
                        );
                        if clicked.clicked() {
                            self.brush.radius = radius;
                            self.brush.opacity = opacity;
                            self.brush.hardness = hardness;
                            self.brush.mode = BrushMode::Paint;
                            self.status = format!("Selected brush: {label}");
                        }
                        ui.label(egui::RichText::new(label).small());
                    });
                }
            });
            ui.separator();
            ui.label(egui::RichText::new("LOCAL ASSETS").small().weak());
            if ui.button("Load .kbrush…").clicked() {
                self.perform(Action::LoadBrush);
            }
            if ui.button("Import texture as layer…").clicked() {
                self.perform(Action::ImportTexture);
            }
            ui.label(
                egui::RichText::new(
                    "Substance .sppr / .spsm assets are not yet decoded. No AI-generated content.",
                )
                .weak()
                .small(),
            );
        });
    }

    fn texture_sets(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("Texture Set List");
            ui.label(
                egui::RichText::new(format!("{} sets", self.doc.texture_set_count()))
                    .small()
                    .weak(),
            );
        });
        ui.separator();
        let mut switch_to = None;
        for (id, name, size, active) in self.doc.texture_set_list() {
            if ui
                .selectable_label(
                    active,
                    format!(
                        "{}  {}  ·  {}×{}",
                        if active { "◉" } else { "○" },
                        name,
                        size,
                        size
                    ),
                )
                .clicked()
                && !active
            {
                switch_to = Some(id);
            }
        }
        if let Some(id) = switch_to {
            self.finish_stroke();
            match self.doc.activate_texture_set(id) {
                Ok(_) => {
                    self.texture_set_name_input = self.doc.active_set_name.clone();
                    self.history.clear();
                    self.rebuild_coverage();
                    self.change();
                    self.status = format!("Selected texture set: {}", self.doc.active_set_name);
                }
                Err(err) => self.status = format!("Cannot select texture set: {err}"),
            }
        }
        ui.separator();
        ui.horizontal_wrapped(|ui| {
            if ui.small_button("+ Add set").clicked() {
                self.finish_stroke();
                let name = format!("Texture Set {}", self.doc.next_set_id);
                match self.doc.add_texture_set(&name) {
                    Ok(_) => {
                        self.texture_set_name_input = name;
                        self.history.clear();
                        self.rebuild_coverage();
                        self.change();
                        self.status = "Created independent texture set".into();
                    }
                    Err(err) => self.status = format!("Cannot create texture set: {err}"),
                }
            }
            if ui
                .add_enabled(
                    self.doc.texture_set_count() > 1,
                    egui::Button::new("− Remove set"),
                )
                .clicked()
            {
                self.finish_stroke();
                match self.doc.remove_active_texture_set() {
                    Ok(()) => {
                        self.texture_set_name_input = self.doc.active_set_name.clone();
                        self.history.clear();
                        self.rebuild_coverage();
                        self.change();
                        self.status = "Removed texture set".into();
                    }
                    Err(err) => self.status = format!("Cannot remove texture set: {err}"),
                }
            }
        });
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.texture_set_name_input).desired_width(140.0),
            );
            if ui.small_button("Rename").clicked() {
                match self
                    .doc
                    .rename_active_texture_set(&self.texture_set_name_input)
                {
                    Ok(()) => {
                        self.texture_set_name_input = self.doc.active_set_name.clone();
                        self.change();
                    }
                    Err(err) => self.status = format!("Cannot rename texture set: {err}"),
                }
            }
        });
        ui.separator();
        ui.label(
            egui::RichText::new(format!("Mesh: {}", self.doc.mesh.name))
                .weak()
                .small(),
        );
        ui.label(egui::RichText::new("Mesh material bindings").strong());
        ui.checkbox(
            &mut self.auto_switch_on_paint,
            "Activate bound set when painting in 3D",
        );
        ui.label(egui::RichText::new("Choose a Texture Set for each imported material. Unbound materials use the active set.").weak().small());
        let options = self.doc.texture_set_list();
        for slot in 0..self.doc.mesh.material_count() {
            let name = self.doc.mesh.material_name(slot).to_owned();
            let mut binding = self.doc.material_bindings.get(slot).copied().flatten();
            ui.horizontal(|ui| {
                ui.label(format!("{}: {}", slot, name));
                egui::ComboBox::from_id_salt(("mesh-slot", slot))
                    .selected_text(
                        binding
                            .and_then(|id| options.iter().find(|s| s.0 == id).map(|s| s.1.as_str()))
                            .unwrap_or("Active set (unbound)"),
                    )
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut binding, None, "Active set (unbound)");
                        for (id, label, _, _) in &options {
                            ui.selectable_value(&mut binding, Some(*id), label.as_str());
                        }
                    });
            });
            let old = self.doc.material_bindings.get(slot).copied().flatten();
            if binding != old {
                self.finish_stroke();
                match self.doc.bind_material(slot, binding) {
                    Ok(()) => {
                        self.rebuild_coverage();
                        self.change();
                        self.status = format!("Material slot {slot} binding updated");
                    }
                    Err(err) => self.status = format!("Cannot bind material: {err}"),
                }
            }
        }
        ui.separator();
        ui.label(
            egui::RichText::new(
                "CPU reference renderer · linked Texture Sets · GPU PBR viewport pending",
            )
            .weak()
            .small(),
        );
        if ui.small_button("Import mesh…").clicked() {
            self.perform(Action::ImportMesh);
        }
    }

    fn texture_settings(&mut self, ui: &mut egui::Ui) {
        ui.heading("Texture Set Settings");
        ui.label("Texture resolution");
        ui.horizontal_wrapped(|ui| {
            for size in [256, 512, 1024, 2048, 4096] {
                if ui
                    .selectable_label(size == self.doc.size, size.to_string())
                    .clicked()
                    && size != self.doc.size
                {
                    self.resize_document(size);
                }
            }
        });
        ui.separator();
        if ui
            .checkbox(&mut self.show_uv_wire, "UV wireframe overlay")
            .changed()
        {
            ui.ctx().request_repaint();
        }
        ui.add_enabled(
            self.uv_coverage.is_some(),
            egui::Checkbox::new(&mut self.uv_clipping, "Limit brush strokes to UV polygons"),
        );
        if let Some(map) = &self.uv_coverage {
            ui.label(format!(
                "Coverage: {:.1}%",
                100.0 * map.covered_texels() as f32 / (self.doc.size as f32).powi(2)
            ));
        }
        ui.separator();
        ui.label(egui::RichText::new("Mesh material bindings are available in Texture Set List. Baking and UDIM are pending.").weak().small());
    }

    fn properties(&mut self, ui: &mut egui::Ui) {
        ui.heading("Properties — Paint / Fill");
        ui.label("MATERIAL CHANNEL");
        egui::ComboBox::from_id_salt("properties_channel")
            .selected_text(self.channel.label())
            .show_ui(ui, |ui| {
                for channel in Channel::ALL {
                    if ui
                        .selectable_value(&mut self.channel, channel, channel.label())
                        .changed()
                    {
                        self.uv_texture = None;
                        if self.solo_channel {
                            self.model_dirty = true;
                        }
                    }
                }
            });
        ui.horizontal_wrapped(|ui| {
            for channel in Channel::ALL {
                if ui.small_button(channel.label()).clicked() {
                    self.channel = channel;
                    self.uv_texture = None;
                    if self.solo_channel {
                        self.model_dirty = true;
                    }
                }
            }
        });
        ui.separator();
        ui.label("Active brush / fill value");
        if self.channel.is_color() {
            let mut color = Color32::from_rgb(
                self.brush.color[0],
                self.brush.color[1],
                self.brush.color[2],
            );
            if ui.color_edit_button_srgba(&mut color).changed() {
                self.brush.color = [color.r(), color.g(), color.b(), 255];
            }
        } else {
            ui.add(
                egui::Slider::new(&mut self.brush.grayscale, 0..=255).text(self.channel.label()),
            );
        }
        ui.add(egui::Slider::new(&mut self.brush.opacity, 0.01..=1.0).text("Opacity"));
        ui.add(egui::Slider::new(&mut self.brush.radius, 1..=180).text("Size"));
        if ui.button("Fill selected layer channel / mask").clicked() {
            self.perform(Action::Fill);
        }
        ui.separator();
        ui.label(
            egui::RichText::new("Smart Materials and node generators: pending implementation.")
                .weak()
                .small(),
        );
    }

    /// Persist the docking layout only when it changes.
    ///
    /// Keep this deliberately flat: it is called from the main UI loop, and
    /// a failure to write user preferences must never interrupt painting.
    fn autosave_workspace(&mut self) {
        if self.last_layout_save.elapsed() < Duration::from_secs(2) {
            return;
        }
        self.last_layout_save = Instant::now();

        let encoded = match layout::encode_workspace(&self.dock) {
            Ok(encoded) => encoded,
            Err(err) => {
                self.status = format!("Workspace could not be encoded: {err}");
                return;
            }
        };

        if encoded == self.saved_layout {
            return;
        }

        match layout::persist_workspace(&self.dock) {
            Ok(_) => {
                self.saved_layout = encoded;
            }
            Err(err) => {
                self.status = format!("Workspace was not saved: {err}");
            }
        }
    }

    fn begin_stroke(&mut self) {
        if self.pending_stroke.is_some() {
            return;
        }
        self.pending_stroke = Some(PendingStroke {
            capture: History::begin_stroke(&self.doc, self.channel, self.brush.mode),
            was_unsaved: self.unsaved,
        });
    }
    fn finish_stroke(&mut self) {
        self.previous_uv = None;
        let Some(pending) = self.pending_stroke.take() else {
            return;
        };
        let recovery_events = self.recovery_journal.as_ref().map(|_| {
            History::stroke_recovery_events(&pending.capture, &self.doc, self.doc.active_set_id)
        });
        match self.history.commit_stroke(pending.capture, &mut self.doc) {
            StrokeCommitResult::Committed => match recovery_events {
                Some(Some(events)) => self.append_recovery_batch(&events),
                Some(None) => {
                    self.status =
                        "Committed stroke could not be recorded for crash recovery".into();
                    self.recovery_journal = None;
                }
                None => {}
            },
            StrokeCommitResult::Unchanged => {}
            StrokeCommitResult::OverLimit => {
                self.unsaved = pending.was_unsaved;
                self.paint_dirty = true;
                self.model_dirty = true;
                self.uv_texture = None;
                self.status =
                    "Stroke canceled: its undo capture exceeds the available history budget".into();
            }
        }
    }
    fn fill_uv(&mut self, uv: [f32; 2]) {
        self.finish_stroke();
        if self.uv_clipping
            && self
                .uv_coverage
                .as_ref()
                .is_some_and(|c| !c.contains_uv(uv))
        {
            self.status = "Flood fill skipped: outside the UV polygons".into();
            return;
        }
        let before = History::capture(&mut self.doc, self.channel, self.brush.mode);
        let layer_id = self.doc.selected_layer().id;
        let coverage = if self.uv_clipping {
            self.uv_coverage.as_ref()
        } else {
            None
        };
        match ops::flood_fill(&mut self.doc, self.channel, uv, &self.brush, coverage) {
            Ok(true) => {
                let after = History::capture(&mut self.doc, self.channel, self.brush.mode);
                self.commit_image_snapshot(Snapshot {
                    layer_id,
                    channel: self.channel,
                    mask: false,
                    before,
                    after,
                });
                self.change();
                self.status = format!(
                    "Flood filled {} at UV ({:.3}, {:.3})",
                    self.channel.label(),
                    uv[0],
                    uv[1]
                );
            }
            Ok(false) => self.status = "No matching pixels to fill at selected UV".into(),
            Err(err) => self.status = format!("Flood fill failed: {err}"),
        }
    }
    fn paint_uv(&mut self, uv: [f32; 2]) {
        if uv.iter().any(|v| !v.is_finite() || *v < 0.0 || *v > 1.0) {
            return;
        }
        if self.brush.mode == BrushMode::Sample {
            let Some(material) = self.material.as_ref() else {
                return;
            };
            let image = material.active().image(self.channel);
            let x = (uv[0] * (image.width() - 1) as f32).round() as u32;
            let y = (uv[1] * (image.height() - 1) as f32).round() as u32;
            let sampled = image.get_pixel(x, y).0;
            if self.channel.is_color() {
                self.brush.color = sampled;
            } else {
                self.brush.grayscale = sampled[0];
            }
            self.status = format!("Sampled {} at ({x},{y})", self.channel.label());
            return;
        }
        self.begin_stroke();
        let coverage = if self.uv_clipping {
            self.uv_coverage.as_ref()
        } else {
            None
        };
        let capture = &mut self
            .pending_stroke
            .as_mut()
            .expect("begin_stroke initializes capture")
            .capture;
        let mut touched_tiles = HashSet::new();
        let size = self.doc.size;
        let tile_columns = size.div_ceil(STROKE_TILE_SIZE) as usize;
        self.brush.stroke_masked_recording(
            &mut self.doc,
            self.channel,
            self.previous_uv,
            uv,
            coverage,
            |doc, min_x, min_y, max_x, max_y| {
                if !History::capture_stroke_rect(capture, doc, min_x, min_y, max_x, max_y) {
                    return false;
                }
                let first_tile_x = min_x / STROKE_TILE_SIZE;
                let first_tile_y = min_y / STROKE_TILE_SIZE;
                let last_tile_x = max_x / STROKE_TILE_SIZE;
                let last_tile_y = max_y / STROKE_TILE_SIZE;
                for tile_y in first_tile_y..=last_tile_y {
                    for tile_x in first_tile_x..=last_tile_x {
                        touched_tiles.insert(tile_y as usize * tile_columns + tile_x as usize);
                    }
                }
                true
            },
        );
        self.previous_uv = Some(uv);
        self.update_painted_tiles(self.channel, &touched_tiles);
    }
    fn view_pane(&mut self, ui: &mut egui::Ui, canvas: Canvas) {
        let ctx = ui.ctx().clone();
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(&self.doc.mesh.name).weak().small());
            ui.separator();
            ui.label(if canvas == Canvas::Model {
                "Material · 3D"
            } else {
                "Material · UV"
            });
        });
        ui.separator();
        let available = ui.available_size();
        let size = if canvas == Canvas::UV {
            Vec2::splat(available.x.min(available.y).clamp(128.0, 850.0))
        } else {
            egui::vec2(
                available.x.clamp(128.0, 1100.0),
                available.y.clamp(128.0, 900.0),
            )
        };
        if canvas == Canvas::Model {
            let desired = (size.x.round() as usize, size.y.round() as usize);
            if (self.view.width, self.view.height) != desired {
                self.view.resize(desired.0, desired.1);
                self.model_dirty = true;
                self.refresh_textures(&ctx);
            }
        }
        ui.vertical_centered(|ui| {
            let texture = match canvas {
                Canvas::Model => self.model_texture.as_ref(),
                Canvas::UV => self.uv_texture.as_ref(),
            };
            let Some(texture) = texture else {
                return;
            };
            let response =
                ui.add(egui::Image::new((texture.id(), size)).sense(egui::Sense::click_and_drag()));
            if canvas == Canvas::UV && self.show_uv_wire {
                let painter = ui.painter_at(response.rect);
                for (index, triangle) in
                    self.doc.mesh.indices.chunks_exact(3).enumerate().take(2500)
                {
                    let material_slot = self.doc.mesh.triangle_material(index);
                    if self.doc.texture_set_for_material(material_slot) != self.doc.active_set_id {
                        continue;
                    }
                    let uv = [triangle[0], triangle[1], triangle[2]]
                        .map(|idx| self.doc.mesh.vertices[idx as usize].uv);
                    for i in 0..3 {
                        let a = uv[i];
                        let b = uv[(i + 1) % 3];
                        painter.line_segment(
                            [
                                egui::pos2(
                                    response.rect.left() + a[0] * size.x,
                                    response.rect.top() + a[1] * size.y,
                                ),
                                egui::pos2(
                                    response.rect.left() + b[0] * size.x,
                                    response.rect.top() + b[1] * size.y,
                                ),
                            ],
                            egui::Stroke::new(
                                0.65_f32,
                                Color32::from_rgba_unmultiplied(245, 245, 250, 115),
                            ),
                        );
                    }
                }
            }
            // The cursor overlay is a non-destructive UI guide, not part of the exported texture.
            if response.hovered()
                && !matches!(self.brush.mode, BrushMode::Sample | BrushMode::Bucket)
            {
                if let Some(pointer) = ctx.input(|i| i.pointer.hover_pos()) {
                    let radius =
                        self.brush.radius as f32 * response.rect.width() / self.doc.size as f32;
                    ui.painter().circle_stroke(
                        pointer,
                        radius.max(1.0),
                        egui::Stroke::new(1.0_f32, Color32::from_gray(235)),
                    );
                }
            }
            if canvas == Canvas::Model && response.hovered() {
                let (secondary, delta, scroll) = ctx.input(|i| {
                    (
                        i.pointer.secondary_down(),
                        i.pointer.delta(),
                        i.raw_scroll_delta.y,
                    )
                });
                if secondary {
                    self.view.orbit(delta.x, delta.y);
                    self.model_dirty = true;
                }
                if scroll != 0.0 {
                    self.view.zoom(scroll);
                    self.model_dirty = true;
                }
            }
            let (down, pressed, secondary, pointer) = ctx.input(|i| {
                (
                    i.pointer.primary_down(),
                    i.pointer.primary_pressed(),
                    i.pointer.secondary_down(),
                    i.pointer.interact_pos(),
                )
            });
            let bucket = self.brush.mode == BrushMode::Bucket;
            if response.hovered() && (if bucket { pressed } else { down }) && !secondary {
                if let Some(pos) = pointer {
                    let local = pos - response.rect.min;
                    let x = (local.x / response.rect.width()).clamp(0.0, 0.999999);
                    let y = (local.y / response.rect.height()).clamp(0.0, 0.999999);
                    let uv = match canvas {
                        Canvas::UV => Some([x, y]),
                        Canvas::Model => self.view.pick(
                            (x * self.view.width as f32) as usize,
                            (y * self.view.height as f32) as usize,
                        ),
                    };
                    if let Some(uv) = uv {
                        if canvas == Canvas::Model && self.auto_switch_on_paint {
                            let hit_x = (x * self.view.width as f32) as usize;
                            let hit_y = (y * self.view.height as f32) as usize;
                            if let Some(slot) = self.view.pick_material_slot(hit_x, hit_y) {
                                let desired = self.doc.texture_set_for_material(slot);
                                if desired != self.doc.active_set_id {
                                    self.finish_stroke();
                                    if self.doc.activate_texture_set(desired).is_ok() {
                                        self.texture_set_name_input =
                                            self.doc.active_set_name.clone();
                                        self.history.clear();
                                        self.rebuild_coverage();
                                        self.change();
                                        self.status = format!(
                                            "Painting material {} → {}",
                                            self.doc.mesh.material_name(slot),
                                            self.doc.active_set_name
                                        );
                                    }
                                }
                            }
                        }
                        if bucket {
                            self.fill_uv(uv);
                        } else {
                            self.paint_uv(uv);
                        }
                    } else {
                        self.previous_uv = None;
                    }
                }
            }
        });
    }
}

impl eframe::App for KaliumApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let close_requested = ctx.input(|input| input.viewport().close_requested());
        if close_requested && !self.allow_close {
            self.finish_stroke();
            if self.unsaved {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.pending_destructive
                    .get_or_insert(DestructiveAction::Close);
            } else if self.pending_destructive.is_some() {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            }
        }
        let down = ctx.input(|i| i.pointer.primary_down());
        if !down {
            self.finish_stroke();
        }
        self.menu(ctx);
        self.unsaved_changes_dialog(ctx);
        self.journal_recovery_dialog(ctx);
        self.export_options_dialog(ctx);
        self.tool_shelf(ctx);
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    egui::RichText::new(format!(
                        "KALIUM {} α · DrapNard",
                        env!("CARGO_PKG_VERSION")
                    ))
                    .color(Color32::from_rgb(156, 201, 248)),
                );
                ui.separator();
                ui.label(&self.status);
                if self.unsaved {
                    ui.label(egui::RichText::new("● Unsaved").color(Color32::YELLOW));
                }
                if self
                    .recovery_journal_path
                    .as_deref()
                    .is_some_and(|path| recovery_journal_disabled_path(path).exists())
                {
                    ui.label(
                        egui::RichText::new("Crash recovery paused · Save to restart")
                            .color(Color32::YELLOW),
                    );
                }
            });
        });
        self.refresh_textures(ctx);
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show(ctx, |ui| {
                // Temporarily take the tree: TabViewer gets exclusive access to app state.
                let mut dock = std::mem::replace(&mut self.dock, DockState::new(vec![]));
                let style = Style::from_egui(ui.style().as_ref());
                DockArea::new(&mut dock).style(style).show_inside(ui, self);
                self.dock = dock;
            });
        self.autosave_workspace();
        ctx.request_repaint_after(Duration::from_secs(2));
        if self.paint_dirty || self.model_dirty {
            ctx.request_repaint();
        }
    }
}

impl TabViewer for KaliumApp {
    type Tab = Pane;
    fn title(&mut self, pane: &mut Pane) -> egui::WidgetText {
        pane.label().into()
    }
    fn ui(&mut self, ui: &mut egui::Ui, pane: &mut Pane) {
        match pane {
            Pane::Assets => self.assets(ui),
            Pane::Viewport3D => self.view_pane(ui, Canvas::Model),
            Pane::Viewport2D => self.view_pane(ui, Canvas::UV),
            Pane::TextureSets => self.texture_sets(ui),
            Pane::Layers => self.layers(ui),
            Pane::TextureSettings => self.texture_settings(ui),
            Pane::Properties => self.properties(ui),
            Pane::BrushStudio => self.tools(ui),
        }
    }
}

#[cfg(test)]
mod unsaved_change_tests {
    use super::{
        egui, paths_refer_to_same_file, project_id_for_save_target, recovery_journal_disabled_path,
        recovery_journal_path, should_continue_after_choice, Action, DestructiveChoice,
        JournalRecoveryChoice, KaliumApp, LayerRecoveryProperties, PendingStroke,
    };
    use crate::{
        document::{BlendMode, Channel, Document},
        history::History,
        layout,
        paint::{Brush, BrushMode},
        project,
        renderer::Viewport,
    };
    use std::{
        collections::HashSet,
        fs,
        path::PathBuf,
        time::{Instant, SystemTime, UNIX_EPOCH},
    };

    struct TempProject(PathBuf);

    impl TempProject {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos();
            Self(std::env::temp_dir().join(format!(
                "kalium-app-recovery-{}-{nonce}.kalium",
                std::process::id()
            )))
        }
    }

    impl Drop for TempProject {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
            let journal_path = recovery_journal_path(&self.0);
            let _ = fs::remove_file(recovery_journal_disabled_path(&journal_path));
            let _ = fs::remove_file(journal_path);
        }
    }

    fn test_app() -> KaliumApp {
        KaliumApp {
            doc: Document::new(64),
            brush: Brush::default(),
            channel: Channel::BaseColor,
            solo_channel: false,
            show_uv_wire: true,
            view: Viewport::new(128, 128),
            uv_coverage: None,
            uv_clipping: true,
            material: None,
            model_texture: None,
            uv_texture: None,
            paint_dirty: true,
            model_dirty: true,
            history: History::new(),
            previous_uv: None,
            pending_stroke: None,
            file_path: None,
            recovery_journal: None,
            recovery_journal_path: None,
            pending_journal_recovery: None,
            allow_recovery_change: false,
            unsaved: false,
            pending_destructive: None,
            allow_close: false,
            status: "Ready".into(),
            dock: layout::default_workspace(),
            last_layout_save: Instant::now(),
            saved_layout: Vec::new(),
            asset_search: String::new(),
            show_tool_shelf: true,
            texture_set_name_input: "Default".into(),
            auto_switch_on_paint: true,
            pending_export_dir: None,
            pending_export_all: false,
            export_padding_radius: 0,
            export_normal_y: crate::export::NormalYConvention::OpenGl,
        }
    }

    #[test]
    fn save_continues_only_after_successful_save() {
        assert!(should_continue_after_choice(DestructiveChoice::Save, true));
        assert!(!should_continue_after_choice(
            DestructiveChoice::Save,
            false
        ));
    }

    #[test]
    fn checkpoint_path_collision_detects_canonical_aliases() {
        let current_dir = std::env::current_dir().unwrap();
        assert!(paths_refer_to_same_file(
            std::path::Path::new("."),
            &current_dir
        ));
        assert!(!paths_refer_to_same_file(
            std::path::Path::new("missing-checkpoint-path-a"),
            std::path::Path::new("missing-checkpoint-path-b")
        ));
    }

    #[test]
    fn save_as_to_a_different_file_starts_a_new_project_identity() {
        let project_id = uuid::Uuid::new_v4();
        let source = std::env::temp_dir().join("kalium-source.kalium");
        let same = project_id_for_save_target(project_id, Some(&source), &source);
        let fork = project_id_for_save_target(
            project_id,
            Some(&source),
            &std::env::temp_dir().join("kalium-fork.kalium"),
        );
        let first_save = project_id_for_save_target(
            project_id,
            None,
            &std::env::temp_dir().join("kalium-first-save.kalium"),
        );
        assert_eq!(same, project_id);
        assert_ne!(fork, project_id);
        assert_eq!(first_save, project_id);
    }

    #[test]
    fn discard_continues_and_cancel_keeps_current_project() {
        assert!(should_continue_after_choice(
            DestructiveChoice::Discard,
            false
        ));
        assert!(!should_continue_after_choice(
            DestructiveChoice::Cancel,
            true
        ));
    }

    #[test]
    fn recovered_project_requires_save_as_and_stays_marked_unsaved() {
        let source_path = PathBuf::from("damaged.kalium");
        let mut app = test_app();
        app.install_recovered_project(Document::new(64), source_path.clone());

        assert!(app.file_path.is_none());
        assert!(app.unsaved);
        assert!(app.status.contains("Save As"));
        assert!(app.status.contains("damaged.kalium"));
    }

    #[test]
    fn primary_project_open_keeps_normal_save_path() {
        let path = PathBuf::from("valid.kalium");
        let mut app = test_app();
        app.install_open_project(Document::new(64), path.clone(), false);

        assert_eq!(app.file_path.as_ref(), Some(&path));
        assert!(!app.unsaved);
        assert!(app.status.starts_with("Opened "));
    }

    #[test]
    fn committed_channel_and_mask_strokes_recover_after_reopen() {
        let temp = TempProject::new();
        let baseline = Document::new(64);
        let project_id = baseline.project_id;
        project::save(&baseline, &temp.0).unwrap();
        let baseline_digest = project::file_digest(&temp.0).unwrap();

        let mut app = test_app();
        app.install_open_project(baseline, temp.0.clone(), false);
        assert!(app.recovery_journal.is_some());

        let mut channel_capture =
            History::begin_stroke(&app.doc, Channel::BaseColor, BrushMode::Paint);
        assert!(History::capture_stroke_rect(
            &mut channel_capture,
            &app.doc,
            4,
            5,
            4,
            5
        ));
        let texture_size = app.doc.size;
        app.doc
            .selected_layer_mut()
            .pixels_mut(Channel::BaseColor, texture_size)
            .put_pixel(4, 5, image::Rgba([12, 34, 56, 255]));
        app.pending_stroke = Some(PendingStroke {
            capture: channel_capture,
            was_unsaved: false,
        });
        app.finish_stroke();

        let mut mask_capture =
            History::begin_stroke(&app.doc, Channel::BaseColor, BrushMode::MaskWhite);
        assert!(History::capture_stroke_rect(
            &mut mask_capture,
            &app.doc,
            8,
            9,
            8,
            9
        ));
        app.doc
            .selected_layer_mut()
            .mask
            .put_pixel(8, 9, image::Luma([73]));
        app.pending_stroke = Some(PendingStroke {
            capture: mask_capture,
            was_unsaved: true,
        });
        app.finish_stroke();

        app.brush.color = [8, 19, 27, 255];
        app.perform(Action::Fill);
        app.brush.mode = BrushMode::MaskWhite;
        app.perform(Action::Fill);

        let no_op_capture = History::begin_stroke(&app.doc, Channel::BaseColor, BrushMode::Paint);
        app.pending_stroke = Some(PendingStroke {
            capture: no_op_capture,
            was_unsaved: true,
        });
        app.finish_stroke();
        let expected_channel = app.doc.selected_layer().pixels[&Channel::BaseColor].clone();
        let expected_mask = app.doc.selected_layer().mask.clone();
        drop(app);

        let records = crate::recovery::scan(
            recovery_journal_path(&temp.0),
            project_id,
            baseline_digest,
            1,
        )
        .unwrap();
        assert_eq!(
            records.len(),
            4,
            "channel/mask strokes and fills append one atomic record each; unchanged strokes append none"
        );

        let saved_base = project::load(&temp.0).unwrap();
        let mut reopened = test_app();
        reopened.install_open_project(saved_base, temp.0.clone(), false);
        assert!(reopened.pending_journal_recovery.is_some());
        assert!(reopened.recovery_journal.is_none());
        reopened.resolve_journal_recovery(JournalRecoveryChoice::Restore);

        assert_eq!(
            reopened.doc.selected_layer().pixels[&Channel::BaseColor],
            expected_channel
        );
        assert_eq!(reopened.doc.selected_layer().mask, expected_mask);
        assert!(reopened.unsaved);
        assert!(reopened.recovery_journal.is_some());
    }

    #[test]
    fn unsupported_project_edit_blocks_partial_journal_recovery() {
        let temp = TempProject::new();
        let baseline = Document::new(64);
        project::save(&baseline, &temp.0).unwrap();

        let mut app = test_app();
        app.install_open_project(baseline, temp.0.clone(), false);
        app.doc.add_layer().unwrap();
        app.change();
        assert!(app.recovery_journal.is_none());
        assert!(recovery_journal_disabled_path(&recovery_journal_path(&temp.0)).exists());
        drop(app);

        let mut reopened = test_app();
        reopened.install_open_project(project::load(&temp.0).unwrap(), temp.0.clone(), false);
        assert!(reopened.pending_journal_recovery.is_none());
        assert!(reopened.recovery_journal.is_none());
        assert!(reopened.status.contains("recovery is paused"));
    }

    #[test]
    fn layer_properties_and_order_replay_from_journal() {
        let temp = TempProject::new();
        let mut baseline = Document::new(64);
        baseline.add_layer().unwrap();
        let ids_before = baseline
            .layers
            .iter()
            .map(|layer| layer.id)
            .collect::<Vec<_>>();
        project::save(&baseline, &temp.0).unwrap();

        let mut app = test_app();
        app.install_open_project(baseline, temp.0.clone(), false);
        let set_id = app.doc.active_set_id;
        let layer_id = app.doc.layers[0].id;
        let before = LayerRecoveryProperties::capture(&app.doc.layers[0]);
        {
            let layer = &mut app.doc.layers[0];
            layer.name = "Journaled layer".into();
            layer.visible = false;
            layer.opacity = 0.375;
            layer.blend = BlendMode::Multiply;
            layer.disabled_channels.insert(Channel::Metallic);
        }
        let property_events = before.events_since(set_id, &app.doc.layers[0]);
        assert_eq!(property_events.len(), 5);
        app.change_with_recovery_events(&property_events);
        assert!(app.move_layer_to(0, 1));
        drop(app);

        let mut reopened = test_app();
        reopened.install_open_project(project::load(&temp.0).unwrap(), temp.0.clone(), false);
        assert!(reopened.pending_journal_recovery.is_some());
        reopened.resolve_journal_recovery(JournalRecoveryChoice::Restore);

        assert_eq!(reopened.doc.layers[0].id, ids_before[1]);
        assert_eq!(reopened.doc.layers[1].id, layer_id);
        let restored = &reopened.doc.layers[1];
        assert_eq!(restored.name, "Journaled layer");
        assert!(!restored.visible);
        assert_eq!(restored.opacity, 0.375);
        assert_eq!(restored.blend, BlendMode::Multiply);
        assert!(restored.disabled_channels.contains(&Channel::Metallic));
        assert!(reopened.unsaved);
    }

    #[test]
    fn discarding_pending_journal_clears_records_and_keeps_saved_baseline() {
        let temp = TempProject::new();
        let baseline = Document::new(64);
        let project_id = baseline.project_id;
        project::save(&baseline, &temp.0).unwrap();
        let baseline_digest = project::file_digest(&temp.0).unwrap();

        let mut app = test_app();
        app.install_open_project(baseline, temp.0.clone(), false);
        let event = crate::recovery::DocumentEvent::LayerImagePatch {
            set_id: app.doc.active_set_id,
            layer_id: app.doc.layers[0].id,
            target: crate::recovery::LayerImageTarget::Channel(Channel::BaseColor),
            x: 0,
            y: 0,
            width: 1,
            height: 1,
            bytes: vec![23, 42, 61, 255],
        };
        app.append_recovery_batch(&[event]);
        drop(app);

        let mut reopened = test_app();
        reopened.install_open_project(project::load(&temp.0).unwrap(), temp.0.clone(), false);
        assert!(reopened.pending_journal_recovery.is_some());
        reopened.resolve_journal_recovery(JournalRecoveryChoice::Discard);
        assert!(reopened.pending_journal_recovery.is_none());
        assert!(reopened.recovery_journal.is_some());
        assert!(crate::recovery::scan(
            recovery_journal_path(&temp.0),
            project_id,
            baseline_digest,
            1,
        )
        .unwrap()
        .is_empty());
        assert!(!recovery_journal_disabled_path(&recovery_journal_path(&temp.0)).exists());
    }

    #[test]
    fn layer_drop_reorders_stack_and_undo_redo_preserve_selection() {
        let mut app = test_app();
        app.doc.add_layer().unwrap();
        app.doc.add_layer().unwrap();
        let original: Vec<_> = app.doc.layers.iter().map(|layer| layer.id).collect();
        let selected_id = app.doc.selected_layer().id;
        app.unsaved = false;
        app.paint_dirty = false;
        app.model_dirty = false;

        assert!(app.move_layer_to(0, 2));
        assert_eq!(
            app.doc
                .layers
                .iter()
                .map(|layer| layer.id)
                .collect::<Vec<_>>(),
            vec![original[1], original[2], original[0]]
        );
        assert_eq!(app.doc.selected_layer().id, selected_id);
        assert!(app.unsaved && app.paint_dirty && app.model_dirty);

        assert!(app.history.undo(&mut app.doc));
        assert_eq!(
            app.doc
                .layers
                .iter()
                .map(|layer| layer.id)
                .collect::<Vec<_>>(),
            original
        );
        assert_eq!(app.doc.selected_layer().id, selected_id);
        assert!(app.history.redo(&mut app.doc));
        assert_eq!(app.doc.layers.last().unwrap().id, original[0]);
        assert_eq!(app.doc.selected_layer().id, selected_id);
    }

    #[test]
    fn layer_drop_self_and_out_of_range_targets_are_no_ops() {
        let mut app = test_app();
        let original: Vec<_> = app.doc.layers.iter().map(|layer| layer.id).collect();

        assert!(!app.move_layer_to(0, 0));
        assert!(!app.move_layer_to(0, 1));
        assert!(!app.move_layer_to(1, 0));
        assert_eq!(
            app.doc
                .layers
                .iter()
                .map(|layer| layer.id)
                .collect::<Vec<_>>(),
            original
        );
        assert!(!app.unsaved);
        assert!(!app.history.undo(&mut app.doc));
    }

    #[test]
    fn egui_layer_row_drag_drops_on_another_row() {
        let mut app = test_app();
        app.doc.add_layer().unwrap();
        app.doc.add_layer().unwrap();
        let original: Vec<_> = app.doc.layers.iter().map(|layer| layer.id).collect();
        let source_id = original[0];
        let target_id = original[2];
        let selected_id = app.doc.selected_layer().id;
        let ctx = egui::Context::default();
        let screen_rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(480.0, 640.0));

        let mut run = |events| {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(screen_rect),
                    events,
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| app.layers(ui));
                },
            )
        };
        run(Vec::new());
        let source_rect = ctx
            .read_response(egui::Id::new(("layer-row-drag", source_id)))
            .unwrap()
            .rect;
        let target_rect = ctx
            .read_response(egui::Id::new(("layer-row-drag", target_id)))
            .unwrap()
            .rect;
        let source_pos = source_rect.center();
        let target_pos = target_rect.center();

        run(vec![
            egui::Event::PointerMoved(source_pos),
            egui::Event::PointerButton {
                pos: source_pos,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            },
        ]);
        run(vec![egui::Event::PointerMoved(target_pos)]);
        run(vec![
            egui::Event::PointerMoved(target_pos),
            egui::Event::PointerButton {
                pos: target_pos,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            },
        ]);

        assert_eq!(
            app.doc
                .layers
                .iter()
                .map(|layer| layer.id)
                .collect::<Vec<_>>(),
            vec![original[1], original[2], original[0]]
        );
        assert_eq!(app.doc.selected_layer().id, selected_id);
        assert!(app.unsaved);
        assert!(app.history.undo(&mut app.doc));
        assert_eq!(
            app.doc
                .layers
                .iter()
                .map(|layer| layer.id)
                .collect::<Vec<_>>(),
            original
        );
        assert!(app.history.redo(&mut app.doc));
        assert_eq!(app.doc.layers.last().unwrap().id, source_id);
    }

    #[test]
    fn painted_tile_updates_cached_material_without_full_invalidation() {
        let mut app = test_app();
        app.material = Some(crate::renderer::MaterialLibrary::from_document(&app.doc));
        app.paint_dirty = false;
        app.model_dirty = false;
        let set_id = app.doc.active_set_id;
        let size = app.doc.size;
        app.doc
            .selected_layer_mut()
            .pixels_mut(Channel::BaseColor, size)
            .put_pixel(10, 11, image::Rgba([210, 30, 70, 255]));
        let mut touched = HashSet::new();
        touched.insert(0);

        app.update_painted_tiles(Channel::BaseColor, &touched);

        let cached = app
            .material
            .as_ref()
            .unwrap()
            .active()
            .image(Channel::BaseColor);
        let current = app
            .doc
            .composite_for_set(set_id, Channel::BaseColor)
            .unwrap();
        assert_eq!(cached.as_raw(), current.as_raw());
        assert!(!app.paint_dirty);
        assert!(app.model_dirty);
        assert!(app.unsaved);
    }
}
