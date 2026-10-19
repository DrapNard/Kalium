use crate::{
    activity_log::{ActivityLevel, ActivityLog},
    bake, brush_preset,
    document::{BlendMode, Channel, Document, Layer},
    export,
    gpu_viewport::GpuViewport,
    history::{
        History, HistoryDirection, LayerProperties, Snapshot, StrokeCapture, StrokeCommitResult,
        STROKE_TILE_SIZE,
    },
    journal_writer::{JournalTicket, JournalWriter},
    layer_tree::LayerNode,
    layout::{self, Pane},
    mesh::Mesh,
    ops,
    paint::{Brush, BrushMode},
    project,
    recent_projects::RecentProjects,
    recovery::{self, DocumentEvent, Journal, LayerImageTarget, Record},
    recovery_store::{RecoveryGeneration, RecoveryGenerationStore},
    renderer::{EnvironmentPreset, MaterialLibrary, PickHit, ScreenRect, ScreenRefresh, Viewport},
    spp,
    uv::UvCoverage,
};
use anyhow::Context;
use eframe::egui::{self, Color32, ColorImage, TextureHandle, TextureOptions, Vec2};
use egui_dock::{DockArea, DockState, TabViewer};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::io::{self, Write};
use std::path::PathBuf;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const RECOVERY_IMAGE_PATCH_BYTES: usize = 256 * 1024;
const GENERATED_PATCHES_PER_RECORD: usize = 16;
// Stay below the journal's 8 MiB frame limit while reducing durable writes
// for large retained-layer snapshots.
const HISTORY_RECOVERY_BATCH_BYTES: usize = 7 * 1024 * 1024;
const RECOVERY_MESH_BYTES: usize = 32 * 1024 * 1024;
const RECOVERY_MESH_CHUNK_BYTES: usize = 4 * 1024 * 1024;
const RECOVERY_WRITER_QUEUE_ITEMS: usize = 128;
const RECOVERY_WRITER_QUEUE_BYTES: usize = 256 * 1024 * 1024;
// The smallest frame is 20 bytes, so this threshold requests a checkpoint
// before the one-million-record scanner cap can be reached by tiny events.
const RECOVERY_COMPACTION_THRESHOLD_BYTES: u64 = 16 * 1024 * 1024;
const MAX_MODEL_SEGMENT_SAMPLES: usize = 2048;
const MAX_PENDING_SCREEN_RECTS: usize = 64;

fn brush_asset_tile(
    ui: &mut egui::Ui,
    name: &str,
    brush: &Brush,
    selected: bool,
) -> egui::Response {
    let response = ui.allocate_response(egui::vec2(76.0, 82.0), egui::Sense::click());
    let rect = response.rect;
    use crate::painter_classic::PainterClassic as Theme;
    let fill = if selected {
        Theme::SELECTED
    } else if response.hovered() {
        Theme::RAISED
    } else {
        Theme::TAB
    };
    let border = if selected {
        Theme::FOCUS
    } else if response.hovered() {
        Theme::BORDER
    } else {
        Theme::HAIRLINE
    };
    let painter = ui.painter().with_clip_rect(rect);
    painter.rect_filled(rect, egui::CornerRadius::ZERO, fill);
    painter.rect_stroke(
        rect,
        egui::CornerRadius::ZERO,
        egui::Stroke::new(1.0_f32, border),
        egui::StrokeKind::Inside,
    );
    let preview = egui::Rect::from_min_size(
        rect.min + egui::vec2(4.0, 4.0),
        egui::vec2(rect.width() - 8.0, 48.0),
    );
    painter.rect_filled(preview, egui::CornerRadius::ZERO, Theme::INPUT);
    match &brush.tip {
        crate::paint::BrushTip::Round => {
            let center = preview.center();
            let radius = (brush.radius as f32 / 32.0 * 17.0).clamp(2.5, 17.0);
            let alpha = (brush.opacity * 175.0).round() as u8;
            for offset in [-8.0_f32, -4.0, 0.0, 4.0, 8.0] {
                painter.circle_filled(
                    egui::pos2(center.x + offset, center.y - offset * 0.2),
                    radius,
                    Color32::from_gray(90_u8.saturating_add(alpha / 2)),
                );
            }
            painter.circle_stroke(
                center,
                radius,
                egui::Stroke::new(1.0_f32, Color32::from_gray(188)),
            );
        }
        crate::paint::BrushTip::Alpha {
            width,
            height,
            mask,
        } => {
            let columns = 24_u32.min(*width).max(1);
            let rows = 18_u32.min(*height).max(1);
            let cell = egui::vec2(
                preview.width() / columns as f32,
                preview.height() / rows as f32,
            );
            for y in 0..rows {
                for x in 0..columns {
                    let sx = (x * *width / columns) as usize;
                    let sy = (y * *height / rows) as usize;
                    let value = mask[sy * *width as usize + sx];
                    if value > 4 {
                        let gray = ((f32::from(value) * brush.opacity).round() as u8).max(24);
                        let cell_rect = egui::Rect::from_min_size(
                            preview.min + egui::vec2(x as f32 * cell.x, y as f32 * cell.y),
                            cell + egui::vec2(0.3, 0.3),
                        );
                        painter.rect_filled(
                            cell_rect,
                            egui::CornerRadius::ZERO,
                            Color32::from_gray(gray),
                        );
                    }
                }
            }
        }
    }
    let label_y = preview.bottom() + 3.0;
    painter.text(
        egui::pos2(rect.center().x, label_y),
        egui::Align2::CENTER_TOP,
        name,
        egui::FontId::proportional(10.0),
        Theme::TEXT,
    );
    painter.text(
        egui::pos2(rect.center().x, label_y + 12.0),
        egui::Align2::CENTER_TOP,
        format!("{} px · {:02.0}%", brush.radius, brush.opacity * 100.0),
        egui::FontId::proportional(9.0),
        Theme::MUTED,
    );
    response.on_hover_text(format!("{name}\n{} px brush", brush.radius))
}

fn layer_mask_preview(mask: &image::GrayImage) -> [u8; 16] {
    let mut preview = [0; 16];
    let width = mask.width().max(1);
    let height = mask.height().max(1);
    for y in 0..4 {
        for x in 0..4 {
            let sx = ((x * width) / 4).min(width - 1);
            let sy = ((y * height) / 4).min(height - 1);
            preview[(y * 4 + x) as usize] = mask.get_pixel(sx, sy).0[0];
        }
    }
    preview
}

fn painter_channel_label(channel: Channel) -> &'static str {
    match channel {
        Channel::BaseColor => "Color",
        Channel::Roughness => "Rough",
        Channel::Metallic => "Metal",
        Channel::Normal => "Norm",
        Channel::Height => "Height",
        Channel::Emissive => "Emissive",
        Channel::Opacity => "Opacity",
        Channel::Occlusion => "AO",
    }
}

fn format_activity_timestamp(timestamp_ms: u64) -> String {
    let seconds = timestamp_ms / 1000;
    let days = (seconds / 86_400) as i64;
    let day_seconds = seconds % 86_400;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    let hour = day_seconds / 3_600;
    let minute = day_seconds % 3_600 / 60;
    let second = day_seconds % 60;
    let millis = timestamp_ms % 1000;
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}.{millis:03} UTC")
}

fn union_screen_rect(first: ScreenRect, second: ScreenRect) -> ScreenRect {
    let x = first.x.min(second.x);
    let y = first.y.min(second.y);
    let right = first
        .x
        .saturating_add(first.width)
        .max(second.x.saturating_add(second.width));
    let bottom = first
        .y
        .saturating_add(first.height)
        .max(second.y.saturating_add(second.height));
    ScreenRect {
        x,
        y,
        width: right.saturating_sub(x),
        height: bottom.saturating_sub(y),
    }
}

fn screen_rects_overlap(first: ScreenRect, second: ScreenRect) -> bool {
    first.x < second.x.saturating_add(second.width)
        && second.x < first.x.saturating_add(first.width)
        && first.y < second.y.saturating_add(second.height)
        && second.y < first.y.saturating_add(first.height)
}

fn append_screen_damage(
    pending: &mut Vec<ScreenRect>,
    incoming: impl IntoIterator<Item = ScreenRect>,
) -> bool {
    for mut rect in incoming {
        let mut index = 0;
        while index < pending.len() {
            if screen_rects_overlap(rect, pending[index]) {
                rect = union_screen_rect(rect, pending.remove(index));
                index = 0;
            } else {
                index += 1;
            }
        }
        pending.push(rect);
        if pending.len() > MAX_PENDING_SCREEN_RECTS {
            return false;
        }
    }
    true
}

fn ao_settings_for_active_set(doc: &Document) -> bake::AmbientOcclusionBakeSettings {
    doc.active_set_extensions
        .get(bake::AO_SETTINGS_EXTENSION_KEY)
        .cloned()
        .and_then(|value| serde_json::from_value::<bake::AmbientOcclusionBakeSettings>(value).ok())
        .unwrap_or_default()
        .clamped_for_size(doc.size)
}

fn default_fill_value(channel: Channel) -> [u8; 4] {
    match channel {
        Channel::Normal => [128, 128, 255, 255],
        Channel::BaseColor => [255, 255, 255, 255],
        _ => [255, 255, 255, 255],
    }
}

fn import_brush_preset_to_shelf(
    source: &std::path::Path,
    shelf: &std::path::Path,
) -> anyhow::Result<PathBuf> {
    let (name, brush) = brush_preset::load(source)?;
    std::fs::create_dir_all(shelf)?;
    let source_stem = source
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or(&name);
    let safe_stem = source_stem
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '-'
            }
        })
        .take(80)
        .collect::<String>()
        .trim_matches('-')
        .to_owned();
    let safe_stem = if safe_stem.is_empty() {
        "Brush"
    } else {
        &safe_stem
    };
    for suffix in 0..=999 {
        let filename = if suffix == 0 {
            format!("{safe_stem}.kbrush")
        } else {
            format!("{safe_stem}-{suffix}.kbrush")
        };
        let target = shelf.join(filename);
        match brush_preset::save_new(&brush, &name, &target) {
            Ok(()) => return Ok(target),
            Err(error)
                if error
                    .root_cause()
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|io_error| {
                        io_error.kind() == std::io::ErrorKind::AlreadyExists
                    }) =>
            {
                continue;
            }
            Err(error) => return Err(error),
        }
    }
    anyhow::bail!("Could not choose a free filename for this brush preset")
}

#[cfg(test)]
fn capture_control_rect(ui: &egui::Ui, key: &str, rect: egui::Rect) {
    ui.ctx()
        .data_mut(|data| data.insert_temp(egui::Id::new(key), rect));
}

struct LimitedMeshHasher {
    length: usize,
    hasher: Sha256,
}

impl LimitedMeshHasher {
    fn new() -> Self {
        Self {
            length: 0,
            hasher: Sha256::new(),
        }
    }

    fn finish(self) -> (usize, [u8; 32]) {
        (self.length, self.hasher.finalize().into())
    }
}

impl Write for LimitedMeshHasher {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let Some(length) = self.length.checked_add(bytes.len()) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "serialized mesh length overflowed",
            ));
        };
        if length > RECOVERY_MESH_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                "serialized mesh exceeds the 32 MiB recovery limit",
            ));
        }
        self.hasher.update(bytes);
        self.length = length;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn mesh_recovery_fingerprint(mesh: &Mesh) -> anyhow::Result<(usize, [u8; 32])> {
    let mut hasher = LimitedMeshHasher::new();
    serde_json::to_writer(&mut hasher, mesh)?;
    Ok(hasher.finish())
}

struct MeshChunkJournalWriter<'a> {
    journal: &'a mut Option<JournalWriter>,
    tickets: &'a mut Vec<JournalTicket>,
    wait_for_durability: bool,
    transaction_id: uuid::Uuid,
    offset: u64,
    written: usize,
    hasher: Sha256,
    buffer: Vec<u8>,
}

impl<'a> MeshChunkJournalWriter<'a> {
    fn new(
        journal: &'a mut Option<JournalWriter>,
        tickets: &'a mut Vec<JournalTicket>,
        wait_for_durability: bool,
        transaction_id: uuid::Uuid,
    ) -> io::Result<Self> {
        let mut buffer = Vec::new();
        buffer
            .try_reserve_exact(RECOVERY_MESH_CHUNK_BYTES)
            .map_err(|_| io::Error::other("could not reserve a bounded mesh recovery chunk"))?;
        Ok(Self {
            journal,
            tickets,
            wait_for_durability,
            transaction_id,
            offset: 0,
            written: 0,
            hasher: Sha256::new(),
            buffer,
        })
    }

    fn emit_chunk(&mut self) -> io::Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let bytes = std::mem::take(&mut self.buffer);
        let length = bytes.len();
        let event = DocumentEvent::MeshReplaceChunk {
            transaction_id: self.transaction_id,
            offset: self.offset,
            bytes,
        };
        #[allow(unused_mut)]
        let mut ticket = append_recovery_events_to_journal(
            self.journal,
            std::slice::from_ref(&event),
            self.wait_for_durability,
        )
        .map_err(io::Error::other)?;
        #[cfg(test)]
        if self.offset == 0 {
            if let Some(ready_path) = std::env::var_os("KALIUM_MESH_RECOVERY_PROCESS_READY") {
                wait_for_journal_ticket(&mut ticket).map_err(io::Error::other)?;
                std::fs::write(
                    PathBuf::from(ready_path),
                    b"first async mesh recovery chunk synced",
                )?;
                loop {
                    std::thread::park_timeout(Duration::from_secs(30));
                }
            }
        }
        self.tickets.push(ticket);
        self.offset = self
            .offset
            .checked_add(length as u64)
            .ok_or_else(|| io::Error::other("mesh recovery chunk offset overflowed"))?;
        self.buffer
            .try_reserve_exact(RECOVERY_MESH_CHUNK_BYTES)
            .map_err(|_| io::Error::other("could not reserve the next mesh recovery chunk"))?;
        Ok(())
    }

    fn finish(mut self) -> io::Result<(u64, [u8; 32])> {
        self.emit_chunk()?;
        Ok((self.offset, self.hasher.finalize().into()))
    }
}

impl Write for MeshChunkJournalWriter<'_> {
    fn write(&mut self, mut bytes: &[u8]) -> io::Result<usize> {
        let input_length = bytes.len();
        let Some(written) = self.written.checked_add(input_length) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "serialized mesh length overflowed",
            ));
        };
        if written > RECOVERY_MESH_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                "serialized mesh exceeds the 32 MiB recovery limit",
            ));
        }
        self.hasher.update(bytes);
        self.written = written;
        while !bytes.is_empty() {
            let remaining = RECOVERY_MESH_CHUNK_BYTES - self.buffer.len();
            let copied = remaining.min(bytes.len());
            self.buffer.extend_from_slice(&bytes[..copied]);
            bytes = &bytes[copied..];
            if self.buffer.len() == RECOVERY_MESH_CHUNK_BYTES {
                self.emit_chunk()?;
            }
        }
        Ok(input_length)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn append_recovery_events_to_journal(
    journal: &mut Option<JournalWriter>,
    events: &[DocumentEvent],
    wait_for_durability: bool,
) -> Result<JournalTicket, String> {
    if events.is_empty() {
        return Err("cannot append an empty recovery event batch".to_owned());
    }
    let payload = recovery::encode_event_batch_bounded(events)
        .map_err(|error| format!("could not encode an undo or redo journal batch: {error}"))?;
    enqueue_recovery_payload_to(journal, payload, wait_for_durability)
}

fn enqueue_recovery_payload_to(
    journal: &mut Option<JournalWriter>,
    payload: Vec<u8>,
    wait_for_durability: bool,
) -> Result<JournalTicket, String> {
    #[allow(unused_mut)]
    let mut ticket = journal
        .as_mut()
        .ok_or_else(|| "the recovery journal is no longer available".to_owned())?
        .try_append(payload)
        .map_err(|error| format!("journal append could not be queued: {error}"))?;
    #[cfg(test)]
    if wait_for_durability {
        wait_for_journal_ticket(&mut ticket)?;
    }
    #[cfg(not(test))]
    let _ = wait_for_durability;
    Ok(ticket)
}

#[cfg(test)]
fn wait_for_journal_ticket(ticket: &mut JournalTicket) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(result) = ticket.try_result()? {
            return Ok(result);
        }
        if Instant::now() >= deadline {
            return Err("timed out waiting for a durable recovery append".to_owned());
        }
        std::thread::yield_now();
    }
}

#[cfg(test)]
fn pause_after_async_snapshot_batch(ticket: &mut JournalTicket) -> Result<(), String> {
    let Some(ready_path) = std::env::var_os("KALIUM_SNAPSHOT_RECOVERY_PROCESS_READY") else {
        return Ok(());
    };
    wait_for_journal_ticket(ticket)?;
    std::fs::write(
        PathBuf::from(ready_path),
        b"first async snapshot recovery batch synced",
    )
    .map_err(|error| error.to_string())?;
    loop {
        std::thread::park_timeout(Duration::from_secs(30));
    }
}

struct PendingHistoryChange {
    direction: HistoryDirection,
    is_mesh_history: bool,
    tickets: Vec<JournalTicket>,
}

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
    ImportBrushPreset,
    Export,
    ExportMaterialId,
    InspectSpp,
    LoadBrush,
    LoadBrushTip,
    SaveBrush,
    Fill,
    ClearChannel,
    InvertMask,
    ResetMask,
    ExportAll,
    BakeAmbientOcclusion,
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

fn autosave_session_lock_path(session_path: &std::path::Path) -> PathBuf {
    let mut name = session_path
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("autosave-session.kalium"))
        .to_os_string();
    name.push(".lock");
    session_path.with_file_name(name)
}

fn project_recovery_lock_path(project_path: &std::path::Path) -> anyhow::Result<PathBuf> {
    let canonical_project = if project_path.exists() {
        std::fs::canonicalize(project_path)?
    } else {
        let parent = project_path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| std::path::Path::new("."));
        std::fs::create_dir_all(parent)?;
        std::fs::canonicalize(parent)?.join(
            project_path
                .file_name()
                .ok_or_else(|| anyhow::anyhow!("project path has no filename"))?,
        )
    };
    let mut filename = canonical_project
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("project path has no filename"))?
        .to_os_string();
    filename.push(".recovery.lock");
    Ok(canonical_project.with_file_name(filename))
}

fn open_project_recovery_lock(
    project_path: &std::path::Path,
) -> anyhow::Result<(PathBuf, std::fs::File)> {
    let lock_path = project_recovery_lock_path(project_path)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)?;
    if let Err(error) = lock.try_lock() {
        anyhow::bail!(
            "another Kalium instance owns this project's recovery state ({}): {error}",
            lock_path.display()
        );
    }
    Ok((lock_path, lock))
}

fn recovery_journal_disabled_path(journal_path: &std::path::Path) -> PathBuf {
    let mut name = journal_path.as_os_str().to_os_string();
    name.push(".disabled");
    PathBuf::from(name)
}

fn invalidate_recovery_journal_file(journal_path: &std::path::Path) -> io::Result<()> {
    let mut file = std::fs::OpenOptions::new().write(true).open(journal_path)?;
    std::io::Seek::seek(&mut file, std::io::SeekFrom::Start(0))?;
    file.write_all(&[0])?;
    file.sync_all()
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
    AddGroup,
    AddFill,
    Duplicate,
    Remove,
    Up,
    Down,
}

#[derive(Clone, PartialEq)]
struct LayerRecoveryProperties {
    name: String,
    visible: bool,
    opacity: f32,
    blend: BlendMode,
    disabled_channels: HashSet<Channel>,
    procedural_graph: Option<crate::procedural::ProceduralGraph>,
}

impl LayerRecoveryProperties {
    fn capture(layer: &Layer) -> Self {
        Self {
            name: layer.name.clone(),
            visible: layer.visible,
            opacity: layer.opacity,
            blend: layer.blend,
            disabled_channels: layer.disabled_channels.clone(),
            procedural_graph: layer.procedural_graph.clone(),
        }
    }

    fn capture_node(node: &LayerNode) -> Self {
        match node {
            LayerNode::Paint(layer) => Self::capture(layer),
            LayerNode::Fill(fill) => Self::capture_common(&fill.properties),
            LayerNode::Group(group) => Self::capture_common(&group.properties),
        }
    }

    fn capture_common(properties: &crate::layer_tree::LayerNodeProperties) -> Self {
        Self {
            name: properties.name.clone(),
            visible: properties.visible,
            opacity: properties.opacity,
            blend: properties.blend,
            disabled_channels: properties.disabled_channels.clone(),
            procedural_graph: None,
        }
    }

    fn tree_event_since(&self, set_id: u32, node: &LayerNode) -> Option<DocumentEvent> {
        let current = Self::capture_node(node);
        if self == &current {
            return None;
        }
        Some(DocumentEvent::LayerTreeNodePropertiesSet {
            set_id,
            node_id: node.id(),
            name: current.name,
            visible: current.visible,
            opacity: current.opacity,
            blend: current.blend,
            disabled_channels: current.disabled_channels.into_iter().collect(),
            procedural_graph: current.procedural_graph,
        })
    }

    #[cfg(test)]
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
        if self.procedural_graph != layer.procedural_graph {
            events.push(DocumentEvent::LayerProceduralGraphSet {
                set_id,
                layer_id: layer.id,
                graph: layer.procedural_graph.clone(),
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
    baseline_path: PathBuf,
    journal_path: PathBuf,
    sequence_high_water: u64,
    records: Vec<Record>,
    journal: Journal,
}

struct RecoveryCompactionInput {
    doc: Document,
    current_writer: Option<JournalWriter>,
    retired_writers: Vec<JournalWriter>,
    old_journal_path: PathBuf,
    origin_path: PathBuf,
    store: RecoveryGenerationStore,
    #[cfg(test)]
    start_gate: Option<std::sync::mpsc::Receiver<()>>,
    #[cfg(test)]
    fail_before_publish: bool,
}

struct RecoveryCompactionOutput {
    doc: Document,
    result: anyhow::Result<RecoveryGeneration>,
}

fn run_recovery_compaction(mut input: RecoveryCompactionInput) -> RecoveryCompactionOutput {
    const JOURNAL_SCHEMA: u32 = 1;
    #[cfg(test)]
    if let Some(gate) = input.start_gate.as_ref() {
        let _ = gate.recv();
    }
    let project_id = input.doc.project_id;
    let result = (|| -> anyhow::Result<RecoveryGeneration> {
        let mut writer_error = input
            .current_writer
            .take()
            .and_then(|writer| writer.shutdown().err());
        for writer in input.retired_writers.drain(..) {
            if let Err(error) = writer.shutdown() {
                writer_error.get_or_insert(error);
            }
        }
        if let Some(error) = writer_error {
            anyhow::bail!("recovery writer drain failed: {error}");
        }
        let active = input.store.read_active(project_id, JOURNAL_SCHEMA)?;
        let (baseline_digest, sequence_anchor) = match active.as_ref() {
            Some(generation) => (generation.baseline_digest, generation.sequence_high_water),
            None => (project::file_digest(&input.origin_path)?, 0),
        };
        let records = recovery::scan(
            &input.old_journal_path,
            project_id,
            baseline_digest,
            JOURNAL_SCHEMA,
        )?;
        let committed_count = recovery::committed_record_count(&records)?;
        anyhow::ensure!(
            committed_count == records.len(),
            "journal ends with an incomplete edit transaction"
        );
        let sequence_high_water = sequence_anchor
            .checked_add(committed_count as u64)
            .ok_or_else(|| anyhow::anyhow!("recovery sequence number overflow"))?;

        #[cfg(test)]
        if input.fail_before_publish {
            anyhow::bail!("injected checkpoint publication failure");
        }

        match input.store.publish_snapshot(
            project_id,
            JOURNAL_SCHEMA,
            sequence_high_water,
            |stage_path| project::save_checkpoint(&input.doc, stage_path),
        ) {
            Ok(generation) => Ok(generation),
            Err(publish_error) => {
                // The manifest may already point at this complete pair if
                // only its final directory sync failed.
                match input.store.read_active(project_id, JOURNAL_SCHEMA) {
                    Ok(Some(generation))
                        if generation.sequence_high_water == sequence_high_water =>
                    {
                        Ok(generation)
                    }
                    _ => Err(publish_error),
                }
            }
        }
    })();
    RecoveryCompactionOutput {
        doc: input.doc,
        result,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum JournalRecoveryChoice {
    Restore,
    Discard,
}

enum RecentProjectAction {
    Open(PathBuf),
    Remove(PathBuf),
    Clear,
}

pub struct KaliumApp {
    doc: Document,
    brush: Brush,
    channel: Channel,
    solo_channel: bool,
    solo_layer: bool,
    collapsed_layer_groups: HashSet<u32>,
    show_uv_wire: bool,
    gpu_preview_enabled: bool,
    gpu_viewport: Option<GpuViewport>,
    gpu_scene_dirty: bool,
    view: Viewport,
    uv_zoom: f32,
    uv_pan: Vec2,
    uv_coverage: Option<UvCoverage>,
    uv_clipping: bool,
    material: Option<MaterialLibrary>,
    model_texture: Option<TextureHandle>,
    uv_texture: Option<TextureHandle>,
    paint_dirty: bool,
    model_dirty: bool,
    model_partial_dirty: Vec<ScreenRect>,
    history: History,
    previous_uv: Option<[f32; 2]>,
    previous_model_hit: Option<PickHit>,
    previous_model_position: Option<egui::Pos2>,
    pending_stroke: Option<PendingStroke>,
    file_path: Option<PathBuf>,
    recovery_journal: Option<JournalWriter>,
    retired_recovery_writers: Vec<JournalWriter>,
    recovery_journal_path: Option<PathBuf>,
    recovery_compaction_retry_after: Option<Instant>,
    recovery_compaction_job: Option<JoinHandle<RecoveryCompactionOutput>>,
    pending_journal_recovery: Option<PendingJournalRecovery>,
    autosave_session_path: Option<PathBuf>,
    autosave_session_lock: Option<std::fs::File>,
    autosave_session_lock_path: Option<PathBuf>,
    project_recovery_lock: Option<std::fs::File>,
    project_recovery_lock_path: Option<PathBuf>,
    pending_open_project_lock: Option<(PathBuf, std::fs::File)>,
    recovery_unavailable: Option<String>,
    recovery_data_rejected: bool,
    rejected_recovery_sources: HashSet<PathBuf>,
    pending_history_change: Option<PendingHistoryChange>,
    allow_recovery_change: bool,
    unsaved: bool,
    pending_destructive: Option<DestructiveAction>,
    allow_close: bool,
    status: String,
    activity_log: Option<ActivityLog>,
    activity_log_error: Option<String>,
    recent_projects: Option<RecentProjects>,
    recent_projects_error: Option<String>,
    dock: DockState<Pane>,
    workspace_path: Option<PathBuf>,
    last_layout_save: Instant,
    saved_layout: Vec<u8>,
    asset_search: String,
    shelf_presets: Vec<brush_preset::ShelfPreset>,
    shelf_skipped_files: usize,
    shelf_error: Option<String>,
    show_tool_shelf: bool,
    texture_set_name_input: String,
    auto_switch_on_paint: bool,
    pending_export_dir: Option<PathBuf>,
    pending_export_all: bool,
    export_job: Option<export::ExportJob>,
    export_job_dir: Option<PathBuf>,
    export_job_progress: Option<export::ExportJobProgress>,
    export_job_all: bool,
    material_id_bake_job: Option<bake::MaterialIdBakeWorker>,
    material_id_bake_set_id: Option<u32>,
    material_id_bake_set_name: Option<String>,
    material_id_bake_output: Option<PathBuf>,
    material_id_bake_metadata: Option<serde_json::Value>,
    pending_material_id_export_dialog: bool,
    material_id_bake_cancel_requested: bool,
    ao_bake_job: Option<bake::AmbientOcclusionBakeWorker>,
    ao_bake_progress: Option<bake::AmbientOcclusionBakeProgress>,
    ao_bake_set_id: Option<u32>,
    ao_settings: bake::AmbientOcclusionBakeSettings,
    export_padding_radius: u32,
    export_normal_y: export::NormalYConvention,
    export_profile: export::EngineExportProfile,
    export_output_size: Option<u32>,
}

impl KaliumApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        crate::painter_classic::PainterClassic::apply(&cc.egui_ctx);
        let doc = Document::new(512);
        let ao_settings = ao_settings_for_active_set(&doc);
        let uv_coverage = UvCoverage::generate(&doc.mesh, doc.size).ok();
        let (activity_log, activity_log_error) = match layout::activity_log_file() {
            Some(path) => match ActivityLog::load(&path) {
                Ok(log) => (Some(log), None),
                Err(error) => (
                    Some(ActivityLog::open_or_default(path)),
                    Some(format!("Activity history could not be loaded: {error:#}")),
                ),
            },
            None => (
                None,
                Some("Activity history is unavailable: no user config directory".into()),
            ),
        };
        let (recent_projects, recent_projects_error) = match layout::recent_projects_file() {
            Some(path) => match RecentProjects::load(path.clone()) {
                Ok(recent) => (Some(recent), None),
                Err(error) => (
                    Some(RecentProjects::empty(path)),
                    Some(format!("Recent projects could not be loaded: {error:#}")),
                ),
            },
            None => (
                None,
                Some("Recent projects are unavailable: no user config directory".into()),
            ),
        };
        let mut app = Self {
            doc,
            brush: Brush::default(),
            channel: Channel::BaseColor,
            solo_channel: false,
            solo_layer: false,
            collapsed_layer_groups: HashSet::new(),
            show_uv_wire: true,
            gpu_preview_enabled: cc.wgpu_render_state.is_some(),
            gpu_viewport: cc.wgpu_render_state.as_ref().and_then(|render_state| {
                match GpuViewport::new(render_state.clone()) {
                    Ok(viewport) => Some(viewport),
                    Err(error) => {
                        eprintln!("Could not initialize GPU viewport: {error:#}");
                        None
                    }
                }
            }),
            gpu_scene_dirty: true,
            view: Viewport::new(600, 600),
            uv_zoom: 1.0,
            uv_pan: Vec2::ZERO,
            uv_coverage,
            uv_clipping: true,
            material: None,
            model_texture: None,
            uv_texture: None,
            paint_dirty: true,
            model_dirty: true,
            model_partial_dirty: Vec::new(),
            history: History::new(),
            previous_uv: None,
            previous_model_hit: None,
            previous_model_position: None,
            pending_stroke: None,
            file_path: None,
            recovery_journal: None,
            retired_recovery_writers: Vec::new(),
            recovery_journal_path: None,
            recovery_compaction_retry_after: None,
            recovery_compaction_job: None,
            pending_journal_recovery: None,
            autosave_session_path: None,
            autosave_session_lock: None,
            autosave_session_lock_path: None,
            project_recovery_lock: None,
            project_recovery_lock_path: None,
            pending_open_project_lock: None,
            recovery_unavailable: None,
            recovery_data_rejected: false,
            rejected_recovery_sources: HashSet::new(),
            pending_history_change: None,
            allow_recovery_change: false,
            unsaved: false,
            pending_destructive: None,
            allow_close: false,
            status: "Ready • Left drag to paint • Right drag to orbit • Scroll to zoom".into(),
            activity_log,
            activity_log_error,
            recent_projects,
            recent_projects_error,
            dock: layout::load_workspace(),
            workspace_path: layout::workspace_file(),
            last_layout_save: Instant::now(),
            saved_layout: Vec::new(),
            asset_search: String::new(),
            shelf_presets: Vec::new(),
            shelf_skipped_files: 0,
            shelf_error: None,
            show_tool_shelf: true,
            texture_set_name_input: "Default".into(),
            auto_switch_on_paint: true,
            pending_export_dir: None,
            pending_export_all: false,
            export_job: None,
            export_job_dir: None,
            export_job_progress: None,
            export_job_all: false,
            material_id_bake_job: None,
            material_id_bake_set_id: None,
            material_id_bake_set_name: None,
            material_id_bake_output: None,
            material_id_bake_metadata: None,
            pending_material_id_export_dialog: false,
            material_id_bake_cancel_requested: false,
            ao_bake_job: None,
            ao_bake_progress: None,
            ao_bake_set_id: None,
            ao_settings,
            export_padding_radius: 0,
            export_normal_y: crate::export::NormalYConvention::OpenGl,
            export_profile: export::EngineExportProfile::Custom,
            export_output_size: None,
        };
        match layout::autosave_session_file() {
            Some(path) => {
                if let Err(error) = app.initialize_autosave_session_at(&path) {
                    let message = format!("Session autosave is unavailable: {error:#}");
                    app.recovery_unavailable = Some(message.clone());
                    app.status = message;
                }
            }
            None => {
                let message =
                    "Session autosave is unavailable: cannot locate the user config directory";
                app.recovery_unavailable = Some(message.into());
                app.status = message.into();
            }
        }
        app.refresh_brush_shelf();
        app
    }

    fn refresh_brush_shelf(&mut self) {
        let Some(directory) = layout::brush_shelf_directory() else {
            self.shelf_presets.clear();
            self.shelf_skipped_files = 0;
            self.shelf_error = Some("User preset directory is unavailable".into());
            return;
        };
        match brush_preset::scan_shelf(&directory) {
            Ok(scan) => {
                self.shelf_presets = scan.presets;
                self.shelf_skipped_files = scan.skipped_files;
                self.shelf_error = None;
            }
            Err(error) => {
                self.shelf_presets.clear();
                self.shelf_skipped_files = 0;
                self.shelf_error = Some(error.to_string());
            }
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
        self.gpu_scene_dirty = true;
        self.unsaved = true;
    }
    fn invalidate_paint_caches(&mut self) {
        self.paint_dirty = true;
        self.model_dirty = true;
        self.gpu_scene_dirty = true;
        self.model_partial_dirty.clear();
        self.unsaved = true;
    }
    fn change_with_recovery_events(&mut self, events: &[DocumentEvent]) {
        if self.recovery_journal.is_some() && !events.is_empty() {
            self.append_recovery_batch(events);
            self.allow_recovery_change = self.recovery_journal.is_some();
        }
        self.change();
    }
    fn start_ao_bake(&mut self) -> anyhow::Result<()> {
        self.finish_stroke();
        self.doc.mesh.validate()?;
        if self.ao_bake_job.is_some()
            || self.export_job.is_some()
            || self.material_id_bake_job.is_some()
        {
            anyhow::bail!("Another background job is already running");
        }
        let size = self.doc.size;
        let bounded_settings = self.ao_settings.clamped_for_size(size);
        if bounded_settings != self.ao_settings {
            self.update_ao_settings(bounded_settings);
        }
        self.ao_settings.validate_for_size(size)?;
        let set_id = self.doc.active_set_id;
        let mesh_snapshot = self.doc.mesh.clone();
        let options = bake::AmbientOcclusionBakeJobOptions {
            size,
            bindings: self.doc.material_bindings.clone(),
            active_set_id: set_id,
            sample_count: self.ao_settings.sample_count,
            max_ray_distance: self.ao_settings.max_ray_distance,
            seed: self.ao_settings.seed,
            working_budget_bytes: crate::document::MAX_DECODED_BYTES,
        };
        self.ao_bake_job = Some(bake::AmbientOcclusionBakeWorker::spawn(
            mesh_snapshot,
            options,
        )?);
        self.ao_bake_progress = None;
        self.ao_bake_set_id = Some(set_id);
        self.status = format!("Baking ambient occlusion for {}…", self.doc.active_set_name);
        Ok(())
    }

    fn start_material_id_bake_to_path(&mut self, mut output_path: PathBuf) -> anyhow::Result<()> {
        self.finish_stroke();
        self.doc.mesh.validate()?;
        if self.ao_bake_job.is_some()
            || self.export_job.is_some()
            || self.material_id_bake_job.is_some()
        {
            anyhow::bail!("Another background job is already running");
        }
        match output_path
            .extension()
            .and_then(|extension| extension.to_str())
        {
            None => {
                output_path.set_extension("png");
            }
            Some("png") => {}
            Some("PNG") => {
                output_path.set_extension("png");
            }
            Some(_) => anyhow::bail!("Material-ID export path must use the .png extension"),
        }

        let set_id = self.doc.active_set_id;
        let set_name = self.doc.active_set_name.clone();
        let size = self.doc.size;
        let output_bytes = u64::from(size)
            .checked_mul(u64::from(size))
            .and_then(|texels| texels.checked_mul(4))
            .ok_or_else(|| anyhow::anyhow!("Material-ID output size overflow"))?;
        let working_budget = crate::document::MAX_DECODED_BYTES
            .checked_sub(self.doc.decoded_bytes())
            .ok_or_else(|| anyhow::anyhow!("Project already exceeds the decoded texture budget"))?;
        if output_bytes > working_budget {
            anyhow::bail!("Material-ID output exceeds the remaining decoded texture budget");
        }
        let metadata = bake::material_id_metadata(&self.doc.mesh, size, set_id, &set_name, true)?;
        let options = bake::MaterialIdBakeJobOptions {
            size,
            bindings: self.doc.material_bindings.clone(),
            active_set_id: set_id,
            working_budget_bytes: working_budget,
        };
        self.material_id_bake_job = Some(bake::MaterialIdBakeWorker::spawn(
            self.doc.mesh.clone(),
            options,
        )?);
        self.material_id_bake_set_id = Some(set_id);
        self.material_id_bake_set_name = Some(set_name.clone());
        self.material_id_bake_output = Some(output_path.clone());
        self.material_id_bake_metadata = Some(metadata);
        self.material_id_bake_cancel_requested = false;
        self.status = format!("Baking material IDs for {set_name}…");
        Ok(())
    }

    fn advance_material_id_bake_job(&mut self, ctx: &egui::Context) {
        let Some(job) = self.material_id_bake_job.as_mut() else {
            return;
        };
        let result = job.try_take_result();
        let Some(result) = result else {
            ctx.request_repaint();
            return;
        };
        self.material_id_bake_job = None;
        let canceled_by_user = std::mem::take(&mut self.material_id_bake_cancel_requested);
        let target_set_id = self.material_id_bake_set_id.take().unwrap_or_default();
        let set_name = self.material_id_bake_set_name.take().unwrap_or_default();
        let output_path = self.material_id_bake_output.take();
        let metadata = self.material_id_bake_metadata.take();
        if canceled_by_user {
            self.status = format!("Material-ID bake for {set_name} canceled");
            ctx.request_repaint();
            return;
        }
        match (result, output_path, metadata) {
            (Ok(image), Some(output_path), Some(metadata)) => {
                match bake::write_material_id_outputs(&image, &metadata, &output_path) {
                    Ok(sidecar_path) => {
                        self.status = format!(
                            "Exported material IDs for {set_name} (Texture Set {target_set_id}) to {} with metadata {}",
                            output_path.display(),
                            sidecar_path.display()
                        );
                    }
                    Err(error) => {
                        self.status = format!("Material-ID export failed: {error:#}");
                    }
                }
            }
            (Err(error), _, _) if error.to_string().contains("canceled") => {
                self.status = format!("Material-ID bake for {set_name} canceled");
            }
            (Err(error), _, _) => {
                self.status = format!("Material-ID bake for {set_name} failed: {error:#}");
            }
            _ => self.status = "Material-ID bake result was incomplete".into(),
        }
        ctx.request_repaint();
    }

    fn material_id_bake_dialog(&mut self, ctx: &egui::Context) {
        if self.material_id_bake_job.is_none() {
            return;
        }
        let set_name = self
            .material_id_bake_set_name
            .as_deref()
            .unwrap_or("Texture Set");
        let output_path = self.material_id_bake_output.as_deref();
        let mut cancel = false;
        egui::Modal::new(egui::Id::new("material-id-bake-progress")).show(ctx, |ui| {
            ui.heading("Baking material IDs");
            ui.label(format!("Texture Set: {set_name}"));
            if let Some(path) = output_path {
                ui.label(format!("Output: {}", path.display()));
            }
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Rasterizing UV coverage…");
            });
            ui.label("Writes a standalone PNG and slot-encoding JSON sidecar. The project stays unchanged.");
            if ui.button("Cancel bake").clicked() {
                cancel = true;
            }
        });
        if cancel {
            self.material_id_bake_cancel_requested = true;
            if let Some(job) = self.material_id_bake_job.as_ref() {
                job.cancel();
            }
            ctx.request_repaint();
        }
    }

    fn update_ao_settings(&mut self, settings: bake::AmbientOcclusionBakeSettings) {
        let settings = settings.clamped_for_size(self.doc.size);
        if settings == self.ao_settings {
            return;
        }
        let event = DocumentEvent::TextureSetAoSettings {
            set_id: self.doc.active_set_id,
            sample_count: settings.sample_count,
            max_ray_distance: settings.max_ray_distance,
            seed: settings.seed,
        };
        match event.apply(&mut self.doc) {
            Ok(()) => {
                self.ao_settings = settings;
                self.change_with_recovery_events(&[event]);
            }
            Err(error) => self.status = format!("Cannot update AO bake settings: {error}"),
        }
    }

    fn install_ao_layer(&mut self, baked: bake::BakedAmbientOcclusion) -> anyhow::Result<()> {
        if baked.size != self.doc.size {
            anyhow::bail!("Texture Set resolution changed while ambient occlusion was baking");
        }
        let size = u64::from(baked.size);
        let mask_bytes = size
            .checked_mul(size)
            .ok_or_else(|| anyhow::anyhow!("Ambient occlusion mask size overflow"))?;
        let projected_bytes = self
            .doc
            .decoded_bytes()
            .checked_add(mask_bytes)
            .and_then(|bytes| {
                size.checked_mul(size)
                    .and_then(|texels| texels.checked_mul(4))
                    .and_then(|image_bytes| bytes.checked_add(image_bytes))
            })
            .ok_or_else(|| anyhow::anyhow!("Ambient occlusion layer size overflow"))?;
        if projected_bytes > crate::document::MAX_DECODED_BYTES {
            anyhow::bail!("Ambient occlusion layer exceeds the 512-MiB decoded texture budget");
        }

        let image = baked.occlusion_image()?;
        let selected_before = self.doc.layer_tree.selected_node_id;
        self.doc.add_layer()?;
        let layer = self
            .doc
            .selected_paint_layer_mut()
            .ok_or_else(|| anyhow::anyhow!("AO output layer is not a Paint node"))?;
        let layer_id = layer.id;
        layer.name = "Ambient Occlusion".into();
        layer.pixels.insert(Channel::Occlusion, image);
        if !self
            .history
            .record_added_layer_with_payload(&self.doc, layer_id, selected_before)
        {
            // Keep older entries from applying against a stack that now has an
            // untracked generated layer.
            self.history.clear();
            self.status = "AO layer created; older undo history was cleared because its payload exceeded the history limit".into();
        }
        self.journal_channel_layer(Channel::Occlusion, false);
        self.allow_recovery_change = self.recovery_journal.is_some();
        self.change();
        if self.status.starts_with("Baking ambient occlusion")
            || self.status == "Ready"
            || self.status.is_empty()
        {
            self.status = format!("Baked ambient occlusion into layer {layer_id}");
        }
        Ok(())
    }

    fn advance_ao_bake_job(&mut self, ctx: &egui::Context) {
        let Some(job) = self.ao_bake_job.as_mut() else {
            return;
        };
        let progress = job.progress();
        self.ao_bake_progress = Some(progress);
        let result = job.try_take_result();
        let Some(result) = result else {
            ctx.request_repaint();
            return;
        };
        self.ao_bake_job = None;
        self.ao_bake_progress = None;
        let target_set_id = self.ao_bake_set_id.take();
        match result {
            Ok(baked) if target_set_id == Some(self.doc.active_set_id) => {
                let set_name = self.doc.active_set_name.clone();
                match self.install_ao_layer(baked) {
                    Ok(()) => {
                        if !self.status.starts_with("AO layer created;") {
                            self.status = format!("Baked ambient occlusion into {set_name}");
                        }
                    }
                    Err(error) => self.status = format!("Could not install AO layer: {error:#}"),
                }
            }
            Ok(_) => {
                self.status = "AO bake result was discarded because its Texture Set changed".into()
            }
            Err(error) if error.to_string().contains("canceled") => {
                self.status = "Ambient occlusion bake canceled".into()
            }
            Err(error) => self.status = format!("Ambient occlusion bake failed: {error:#}"),
        }
        ctx.request_repaint();
    }

    fn ao_bake_dialog(&mut self, ctx: &egui::Context) {
        if self.ao_bake_job.is_none() {
            return;
        }
        let progress = self.ao_bake_progress;
        let set_name = self.doc.active_set_name.clone();
        let mut cancel = false;
        egui::Modal::new(egui::Id::new("ao-bake-progress")).show(ctx, |ui| {
            ui.heading("Baking ambient occlusion");
            ui.label(format!("Texture Set: {set_name}"));
            if let Some(progress) = progress {
                let fraction = if progress.total_rows == 0 {
                    0.0
                } else {
                    progress.completed_rows as f32 / progress.total_rows as f32
                };
                ui.add(
                    egui::ProgressBar::new(fraction.clamp(0.0, 1.0))
                        .show_percentage()
                        .text(format!(
                            "{} / {} rows",
                            progress.completed_rows, progress.total_rows
                        )),
                );
            } else {
                ui.label("Preparing UV coverage and acceleration data…");
            }
            ui.label("The result will be added as an editable Occlusion layer.");
            if ui.button("Cancel bake").clicked() {
                cancel = true;
            }
        });
        if cancel {
            if let Some(job) = self.ao_bake_job.as_ref() {
                job.cancel();
            }
            ctx.request_repaint();
        }
    }

    fn apply_history(&mut self, direction: HistoryDirection) {
        self.apply_history_impl(direction, cfg!(test));
    }

    fn apply_history_impl(&mut self, direction: HistoryDirection, wait_for_ack_for_test: bool) {
        self.finish_stroke();
        let has_history_entry = match direction {
            HistoryDirection::Undo => self.history.can_undo(),
            HistoryDirection::Redo => self.history.can_redo(),
        };
        if !has_history_entry {
            return;
        }
        let is_mesh_history = self.history.mesh_recovery_target(direction).is_some();
        if self.recovery_journal.is_some() {
            // Mesh history uses the existing streamed replacement transaction;
            // other history entries stream bounded layer/image events.
            let mesh_recovery =
                self.history
                    .mesh_recovery_target(direction)
                    .map(|(mesh, bindings)| {
                        Self::append_mesh_recovery_transaction_to(
                            &mut self.recovery_journal,
                            &self.doc,
                            mesh,
                            bindings,
                            false,
                        )
                    });
            let tickets = match mesh_recovery {
                Some(Ok(tickets)) => Ok(tickets),
                Some(Err(error)) => {
                    self.status =
                        format!("Mesh Undo/Redo recovery could not be recorded: {error:#}");
                    Err(error.to_string())
                }
                None => self.append_history_recovery_sequence(direction),
            };
            match tickets {
                Ok(tickets) => {
                    self.pending_history_change = Some(PendingHistoryChange {
                        direction,
                        is_mesh_history,
                        tickets,
                    });
                    #[cfg(test)]
                    if wait_for_ack_for_test {
                        self.wait_for_pending_history_for_test();
                    }
                    #[cfg(not(test))]
                    let _ = wait_for_ack_for_test;
                    return;
                }
                Err(error) => {
                    self.disable_recovery_journal(&format!(
                        "This undo or redo change could not be represented and durably recorded in the recovery journal: {error}"
                    ));
                }
            }
        }
        self.finish_history_change(direction, is_mesh_history);
    }

    fn finish_history_change(&mut self, direction: HistoryDirection, is_mesh_history: bool) {
        if !self.apply_history_change(direction) {
            if self.recovery_journal.is_some() {
                self.disable_recovery_journal(
                    "Undo or redo could not apply after its recovery events were prepared",
                );
            }
            return;
        }
        self.allow_recovery_change = self.recovery_journal.is_some();
        if is_mesh_history {
            self.rebuild_coverage();
        }
        self.change();
    }

    fn advance_pending_history(&mut self) -> bool {
        let Some(mut pending) = self.pending_history_change.take() else {
            return true;
        };
        let mut awaiting_ack = false;
        let mut append_error = None;
        for ticket in &mut pending.tickets {
            match ticket.try_result() {
                Ok(Some(())) => {}
                Ok(None) => awaiting_ack = true,
                Err(error) => {
                    append_error.get_or_insert(error);
                }
            }
        }
        if awaiting_ack && append_error.is_none() {
            self.pending_history_change = Some(pending);
            return false;
        }
        if let Some(error) = append_error {
            self.disable_recovery_journal(&format!("Undo or redo recovery append failed: {error}"));
        }
        self.finish_history_change(pending.direction, pending.is_mesh_history);
        true
    }

    #[cfg(test)]
    fn wait_for_pending_history_for_test(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.pending_history_change.is_some() {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for durable history recovery"
            );
            self.advance_pending_history();
            std::thread::yield_now();
        }
    }
    fn apply_history_change(&mut self, direction: HistoryDirection) -> bool {
        match direction {
            HistoryDirection::Undo => self.history.undo(&mut self.doc),
            HistoryDirection::Redo => self.history.redo(&mut self.doc),
        }
    }
    fn update_painted_tiles(&mut self, channel: Channel, tile_keys: &HashSet<usize>) {
        if tile_keys.is_empty() {
            return;
        }
        if self.paint_dirty || self.material.is_none() {
            if channel == Channel::BaseColor {
                self.gpu_scene_dirty = true;
            }
            self.invalidate_paint_caches();
            return;
        }

        let size = self.doc.size;
        let can_refresh_viewport_incrementally = !self.model_dirty && self.model_texture.is_some();
        let tile_columns = size.div_ceil(STROKE_TILE_SIZE) as usize;
        let set_id = self.doc.active_set_id;
        let Some(changed_layer_id) = self.doc.selected_paint_layer().map(|layer| layer.id) else {
            return;
        };
        let mut affected_channels = vec![channel];
        if channel == Channel::Occlusion
            && self
                .doc
                .layer_tree
                .ordered_paint_leaf_ids()
                .into_iter()
                .any(|id| {
                    self.doc.paint_layer(id).is_some_and(|layer| {
                        layer.visible
                            && layer.opacity > 0.0
                            && !layer.disabled_channels.contains(&Channel::Roughness)
                            && layer
                                .procedural_graph
                                .as_ref()
                                .and_then(|graph| graph.occlusion_source_layer_id())
                                == Some(changed_layer_id)
                    })
                })
        {
            affected_channels.push(Channel::Roughness);
        }
        let mut changed_regions = Vec::with_capacity(tile_keys.len());
        for &key in tile_keys {
            let tile_x = (key % tile_columns) as u32;
            let tile_y = (key / tile_columns) as u32;
            let x = tile_x * STROKE_TILE_SIZE;
            let y = tile_y * STROKE_TILE_SIZE;
            let width = STROKE_TILE_SIZE.min(size - x);
            let height = STROKE_TILE_SIZE.min(size - y);
            let mut uv_region = None;
            for &affected_channel in &affected_channels {
                let Some(region) =
                    self.composite_display_region(affected_channel, x, y, width, height)
                else {
                    self.invalidate_paint_caches();
                    return;
                };
                let patched = self.material.as_mut().is_some_and(|material| {
                    material.patch_region(set_id, affected_channel, x, y, &region)
                });
                if !patched {
                    self.invalidate_paint_caches();
                    return;
                }
                if affected_channel == channel {
                    if channel == Channel::BaseColor && !self.gpu_scene_dirty {
                        if let Some(gpu_viewport) = self.gpu_viewport.as_mut() {
                            match gpu_viewport.update_base_color_regions(set_id, &[(x, y, &region)])
                            {
                                Ok(true) => {}
                                Ok(false) => self.gpu_scene_dirty = true,
                                Err(error) => {
                                    self.gpu_scene_dirty = true;
                                    self.status = format!(
                                        "GPU Base Color update deferred to full refresh: {error:#}"
                                    );
                                }
                            }
                        }
                    }
                    uv_region = Some(region);
                }
            }
            changed_regions.push((x, y, width, height));
            if channel == self.channel {
                if let (Some(texture), Some(region)) = (self.uv_texture.as_mut(), uv_region) {
                    let image = ColorImage::from_rgba_unmultiplied(
                        [region.width() as usize, region.height() as usize],
                        region.as_raw(),
                    );
                    texture.set_partial([x as usize, y as usize], image, TextureOptions::LINEAR);
                }
            }
        }
        self.unsaved = true;
        if can_refresh_viewport_incrementally {
            let refresh = self.material.as_ref().map(|material| {
                self.view.refresh_material_tiles(
                    material,
                    &self.doc.material_bindings,
                    set_id,
                    channel,
                    STROKE_TILE_SIZE,
                    &changed_regions,
                    if self.solo_channel {
                        Some(self.channel)
                    } else {
                        None
                    },
                )
            });
            match refresh {
                Some(ScreenRefresh::Updated(rects)) => {
                    if append_screen_damage(&mut self.model_partial_dirty, rects) {
                        self.model_dirty = false;
                    } else {
                        self.model_partial_dirty.clear();
                        self.model_dirty = true;
                    }
                }
                Some(ScreenRefresh::NoVisiblePixels) => {
                    self.model_dirty = false;
                }
                Some(ScreenRefresh::CacheInvalid) | None => {
                    self.invalidate_paint_caches();
                }
            }
        } else {
            self.model_dirty = true;
            self.model_partial_dirty.clear();
        }
    }

    fn composite_display_region(
        &self,
        channel: Channel,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
    ) -> Option<image::RgbaImage> {
        if self.solo_layer {
            self.doc.layer_tree.composite_isolated_region(
                self.doc.layer_tree.selected_node_id,
                self.doc.size,
                channel,
                x,
                y,
                width,
                height,
            )
        } else {
            self.doc
                .composite_region_for_set(self.doc.active_set_id, channel, x, y, width, height)
        }
    }
    fn refresh_uniform_channel_cache(&mut self, channel: Channel) -> bool {
        if self.paint_dirty || self.material.is_none() {
            return false;
        }
        let Some(changed_layer_id) = self.doc.selected_paint_layer().map(|layer| layer.id) else {
            return false;
        };
        if channel == Channel::Occlusion
            && self
                .doc
                .layer_tree
                .ordered_paint_leaf_ids()
                .into_iter()
                .any(|id| {
                    self.doc.paint_layer(id).is_some_and(|layer| {
                        layer.visible
                            && layer.opacity > 0.0
                            && !layer.disabled_channels.contains(&Channel::Roughness)
                            && layer
                                .procedural_graph
                                .as_ref()
                                .and_then(|graph| graph.occlusion_source_layer_id())
                                == Some(changed_layer_id)
                    })
                })
        {
            return false;
        }

        let set_id = self.doc.active_set_id;
        let size = self.doc.size;
        let stripe_height = STROKE_TILE_SIZE;
        let mut y = 0;
        while y < size {
            let height = stripe_height.min(size - y);
            let Some(region) = self.composite_display_region(channel, 0, y, size, height) else {
                self.invalidate_paint_caches();
                return false;
            };
            let patched = self
                .material
                .as_mut()
                .is_some_and(|material| material.patch_region(set_id, channel, 0, y, &region));
            if !patched {
                self.invalidate_paint_caches();
                return false;
            }
            if channel == self.channel {
                if let Some(texture) = self.uv_texture.as_mut() {
                    let image = ColorImage::from_rgba_unmultiplied(
                        [region.width() as usize, region.height() as usize],
                        region.as_raw(),
                    );
                    texture.set_partial([0, y as usize], image, TextureOptions::LINEAR);
                }
            }
            y += height;
        }
        self.model_dirty = true;
        self.model_partial_dirty.clear();
        true
    }
    fn refresh_textures(&mut self, ctx: &egui::Context) {
        if self.paint_dirty || self.material.is_none() {
            self.material = Some(MaterialLibrary::from_document(&self.doc));
            if self.solo_layer {
                if let Some(isolated) = self.doc.layer_tree.composite_isolated(
                    self.doc.layer_tree.selected_node_id,
                    self.doc.size,
                    self.channel,
                ) {
                    if !self.material.as_mut().is_some_and(|material| {
                        material.replace_channel_map(self.doc.active_set_id, self.channel, isolated)
                    }) {
                        self.material = None;
                        return;
                    }
                } else {
                    self.material = None;
                    return;
                }
            }
            self.paint_dirty = false;
            self.model_dirty = true;
            self.uv_texture = None;
        }
        if self.gpu_scene_dirty {
            if let Some(gpu_viewport) = self.gpu_viewport.as_mut() {
                match gpu_viewport.update_scene(&self.doc) {
                    Ok(()) => self.gpu_scene_dirty = false,
                    Err(error) => {
                        self.gpu_preview_enabled = false;
                        self.status =
                            format!("GPU preview unavailable; using CPU reference: {error:#}");
                    }
                }
            } else {
                self.gpu_scene_dirty = false;
            }
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
            self.model_partial_dirty.clear();
            self.model_dirty = false;
        } else if !self.model_partial_dirty.is_empty() {
            if let Some(texture) = self.model_texture.as_mut() {
                let damage = std::mem::take(&mut self.model_partial_dirty);
                let source_stride = self.view.width * 4;
                for rect in damage {
                    let mut pixels = Vec::with_capacity(rect.width * rect.height * 4);
                    let row_bytes = rect.width * 4;
                    for y in rect.y..rect.y + rect.height {
                        let start = y * source_stride + rect.x * 4;
                        pixels.extend_from_slice(&self.view.rgba[start..start + row_bytes]);
                    }
                    let image =
                        ColorImage::from_rgba_unmultiplied([rect.width, rect.height], &pixels);
                    texture.set_partial([rect.x, rect.y], image, TextureOptions::LINEAR);
                }
            }
        }
    }
    fn start_new_project(&mut self) {
        self.finish_stroke();
        let _ = self.stop_recovery_writer();
        self.remove_autosave_session_files();
        self.pending_journal_recovery = None;
        if let Some(path) = self.recovery_journal_path.take() {
            if !self.recovery_data_rejected {
                let _ = std::fs::remove_file(recovery_journal_disabled_path(&path));
                let _ = std::fs::remove_file(path);
            }
        }
        self.recovery_data_rejected = false;
        self.release_project_recovery_lock();
        self.doc = Document::new(512);
        self.ao_settings = ao_settings_for_active_set(&self.doc);
        self.texture_set_name_input = self.doc.active_set_name.clone();
        self.history.clear();
        self.file_path = None;
        self.unsaved = false;
        self.view = Viewport::new(600, 600);
        self.rebuild_coverage();
        self.change();
        self.unsaved = false;
        self.status = "New Kalium project created".into();
        self.record_activity(ActivityLevel::Info, "Project", "Created a new project");
        match layout::autosave_session_file() {
            Some(path) => {
                if let Err(error) = self.initialize_autosave_session_at(&path) {
                    let message =
                        format!("New project created; session autosave is unavailable: {error:#}");
                    self.recovery_unavailable = Some(message.clone());
                    self.status = message;
                }
            }
            None => {
                let message = "New project created; session autosave is unavailable because the user config directory could not be located";
                self.recovery_unavailable = Some(message.into());
                self.status = message.into();
            }
        }
    }

    fn install_project(&mut self, doc: Document, path: PathBuf) {
        self.finish_stroke();
        let _ = self.stop_recovery_writer();
        self.remove_autosave_session_files();
        self.release_autosave_session_lock();
        let incoming_lock_path = project_recovery_lock_path(&path).ok();
        if self
            .pending_open_project_lock
            .as_ref()
            .is_some_and(|(pending_path, _)| Some(pending_path) == incoming_lock_path.as_ref())
        {
            if let Some((lock_path, lock)) = self.pending_open_project_lock.take() {
                self.project_recovery_lock = Some(lock);
                self.project_recovery_lock_path = Some(lock_path);
            }
        } else if self.project_recovery_lock_path.as_deref() != incoming_lock_path.as_deref() {
            self.release_project_recovery_lock();
            self.pending_open_project_lock = None;
        }
        self.recovery_journal_path = None;
        self.pending_journal_recovery = None;
        self.recovery_unavailable = None;
        self.recovery_data_rejected = false;
        self.collapsed_layer_groups.clear();
        self.doc = doc;
        self.ao_settings = ao_settings_for_active_set(&self.doc);
        self.texture_set_name_input = self.doc.active_set_name.clone();
        self.rebuild_coverage();
        self.history.clear();
        self.file_path = Some(path.clone());
        self.change();
        self.unsaved = false;
        self.status = format!("Opened {}", path.display());
        let message = self.status.clone();
        self.record_activity(ActivityLevel::Info, "Project", &message);
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
            self.install_project(doc, path.clone());
            self.configure_recovery_journal();
            if self.recovery_journal.is_none() && self.pending_journal_recovery.is_none() {
                self.recovery_unavailable = Some(self.status.clone());
            }
            if let Some(warning) = self.record_recent_project(&path) {
                self.status = format!("{}; {warning}", self.status);
            }
        }
    }

    fn record_recent_project(&mut self, path: &std::path::Path) -> Option<String> {
        let recent = self.recent_projects.as_mut()?;
        match recent.record(path) {
            Ok(()) => {
                self.recent_projects_error = None;
                None
            }
            Err(error) => {
                let warning = format!("Recent projects could not be saved: {error:#}");
                self.recent_projects_error = Some(warning.clone());
                Some(warning)
            }
        }
    }

    /// Both the file picker and Recent menu use the same guarded installation.
    fn request_open_project(&mut self, path: PathBuf) -> anyhow::Result<String> {
        if self.pending_journal_recovery.is_some() {
            anyhow::bail!("Restore or discard the pending recovery data first");
        }
        if self.pending_destructive.is_some() {
            anyhow::bail!("Finish the pending unsaved-changes choice first");
        }
        let pending_lock = self.provisional_project_recovery_lock(&path)?;
        let (doc, recovered) = match project::load_with_recovery(&path)? {
            project::RecoveryOutcome::Primary(doc) => (doc, false),
            project::RecoveryOutcome::RecoveredTemporary(doc) => (doc, true),
        };
        self.pending_open_project_lock = pending_lock;
        self.finish_stroke();
        if self.unsaved {
            self.pending_destructive = Some(DestructiveAction::Open {
                doc: Box::new(doc),
                path,
                recovered,
            });
            Ok("Choose how to handle unsaved changes".into())
        } else {
            self.install_open_project(doc, path, recovered);
            Ok(self.status.clone())
        }
    }

    fn mark_recovery_data_rejected(&mut self, path: &std::path::Path) {
        self.recovery_data_rejected = true;
        self.rejected_recovery_sources.insert(path.to_owned());
    }

    fn configure_recovery_journal(&mut self) {
        let Some(project_path) = self.file_path.clone() else {
            return;
        };
        self.configure_recovery_journal_for_path(&project_path);
    }

    fn configure_recovery_journal_for_path(&mut self, project_path: &std::path::Path) {
        const JOURNAL_SCHEMA: u32 = 1;
        if !project_path.is_file() {
            return;
        }
        if let Err(error) = self.acquire_project_recovery_lock(project_path) {
            let message =
                format!("Project recovery is owned by another Kalium instance: {error:#}");
            self.recovery_journal_path = Some(recovery_journal_path(project_path));
            self.recovery_unavailable = Some(message.clone());
            self.status = message;
            return;
        }
        let legacy_journal_path = recovery_journal_path(project_path);
        let store = RecoveryGenerationStore::from_journal_path(&legacy_journal_path);
        let active_generation = match store.read_active(self.doc.project_id, JOURNAL_SCHEMA) {
            Ok(generation) => generation,
            Err(error) => {
                self.status = format!("Crash recovery generation was rejected: {error}");
                self.recovery_unavailable = Some(self.status.clone());
                self.mark_recovery_data_rejected(project_path);
                return;
            }
        };
        let (journal_path, baseline_path, digest, sequence_anchor) =
            if let Some(generation) = active_generation.as_ref() {
                (
                    generation.journal_path.clone(),
                    generation.baseline_path.clone(),
                    generation.baseline_digest,
                    generation.sequence_high_water,
                )
            } else {
                let digest = match project::file_digest(project_path) {
                    Ok(digest) => digest,
                    Err(error) => {
                        self.status = format!("Crash recovery is unavailable: {error:#}");
                        self.recovery_unavailable = Some(self.status.clone());
                        return;
                    }
                };
                (
                    legacy_journal_path.clone(),
                    project_path.to_owned(),
                    digest,
                    0,
                )
            };
        if recovery_journal_disabled_path(&legacy_journal_path).exists()
            || recovery_journal_disabled_path(&journal_path).exists()
        {
            self.recovery_journal_path = Some(journal_path);
            let message = "Crash recovery is paused because this project contains edits not yet covered by journal events. Save to start a new recovery baseline.";
            self.status = message.into();
            self.recovery_unavailable = Some(message.into());
            return;
        }
        let primary_digest = match project::file_digest(project_path) {
            Ok(digest) => digest,
            Err(error) => {
                self.status = format!("Crash recovery is unavailable: {error:#}");
                self.recovery_unavailable = Some(self.status.clone());
                return;
            }
        };
        self.recovery_journal_path = Some(journal_path.clone());
        if journal_path.exists() {
            match Journal::open(&journal_path, self.doc.project_id, digest, JOURNAL_SCHEMA) {
                Ok((mut journal, mut records)) => {
                    let committed_count = match recovery::committed_record_count(&records) {
                        Ok(count) => count,
                        Err(error) => {
                            self.status = format!("Crash recovery journal was rejected: {error:#}");
                            self.recovery_unavailable = Some(self.status.clone());
                            self.mark_recovery_data_rejected(project_path);
                            return;
                        }
                    };
                    if committed_count < records.len() {
                        if let Err(error) = journal.truncate_records(&records, committed_count) {
                            self.status = format!(
                                "Crash recovery could not discard an incomplete edit: {error}"
                            );
                            self.recovery_unavailable = Some(self.status.clone());
                            self.mark_recovery_data_rejected(project_path);
                            return;
                        }
                        records.truncate(committed_count);
                    }
                    let mut has_events = active_generation
                        .as_ref()
                        .is_some_and(|generation| generation.baseline_digest != primary_digest);
                    for record in &records {
                        match recovery::decode_event_batch_bounded(&record.payload) {
                            Ok(batch) => has_events |= !batch.is_empty(),
                            Err(error) => {
                                self.status =
                                    format!("Crash recovery journal was rejected: {error}");
                                self.recovery_unavailable = Some(self.status.clone());
                                self.mark_recovery_data_rejected(project_path);
                                return;
                            }
                        }
                    }
                    let Some(sequence_high_water) =
                        sequence_anchor.checked_add(committed_count as u64)
                    else {
                        self.status = "Crash recovery generation sequence overflow".into();
                        self.recovery_unavailable = Some(self.status.clone());
                        self.mark_recovery_data_rejected(project_path);
                        return;
                    };
                    if !has_events {
                        match JournalWriter::new(
                            journal,
                            RECOVERY_WRITER_QUEUE_ITEMS,
                            RECOVERY_WRITER_QUEUE_BYTES,
                        ) {
                            Ok(writer) => self.recovery_journal = Some(writer),
                            Err(error) => {
                                self.status =
                                    format!("Crash recovery writer could not start: {error}");
                                self.recovery_unavailable = Some(self.status.clone());
                            }
                        }
                    } else {
                        self.pending_journal_recovery = Some(PendingJournalRecovery {
                            project_path: project_path.to_owned(),
                            baseline_path,
                            journal_path,
                            sequence_high_water,
                            records,
                            journal,
                        });
                        if active_generation
                            .as_ref()
                            .is_some_and(|generation| generation.used_fallback)
                        {
                            self.status = "The newest recovery checkpoint was invalid; using the previous verified generation".into();
                        }
                    }
                }
                Err(error) => {
                    self.status = format!("Crash recovery journal was rejected: {error}");
                    self.recovery_unavailable = Some(self.status.clone());
                    self.mark_recovery_data_rejected(project_path);
                }
            }
        } else if let Err(error) = self.create_recovery_journal(&journal_path, digest) {
            self.status = format!("Crash recovery is unavailable: {error:#}");
            self.recovery_unavailable = Some(self.status.clone());
        }
    }

    fn initialize_autosave_session_at(&mut self, path: &std::path::Path) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.rejected_recovery_sources.contains(path),
            "Rejected autosave data was preserved at {}; use Save As for the new document",
            path.display()
        );
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let lock_path = autosave_session_lock_path(path);
        let new_lock = if let Some(held_path) = self.autosave_session_lock_path.as_deref() {
            anyhow::ensure!(
                held_path == lock_path,
                "this app already owns a different autosave session"
            );
            None
        } else {
            let lock = std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(&lock_path)?;
            if let Err(error) = lock.try_lock() {
                let reason = format!(
                    "another Kalium instance owns this autosave session ({}): {error}",
                    lock_path.display()
                );
                self.recovery_unavailable = Some(reason.clone());
                anyhow::bail!(reason);
            }
            Some(lock)
        };
        if path.exists() {
            // A journal is bound to the exact primary baseline digest. Do not
            // attach it to a sibling temp archive with different bytes.
            self.doc = project::load(path)?;
        } else {
            project::save(&self.doc, path)?;
        }
        self.autosave_session_path = Some(path.to_owned());
        if let Some(lock) = new_lock {
            self.autosave_session_lock = Some(lock);
            self.autosave_session_lock_path = Some(lock_path);
        }
        self.rebuild_coverage();
        self.texture_set_name_input = self.doc.active_set_name.clone();
        self.file_path = None;
        self.configure_recovery_journal_for_path(path);
        if self.recovery_journal.is_some() || self.pending_journal_recovery.is_some() {
            self.recovery_unavailable = None;
        } else if self.recovery_unavailable.is_none() {
            self.recovery_unavailable = Some(self.status.clone());
        }
        Ok(())
    }

    fn remove_autosave_session_files(&mut self) {
        let Some(path) = self.autosave_session_path.take() else {
            return;
        };
        if self.rejected_recovery_sources.contains(&path) {
            self.release_autosave_session_lock();
            return;
        }
        let journal_path = recovery_journal_path(&path);
        let _ = std::fs::remove_file(recovery_journal_disabled_path(&journal_path));
        let _ = std::fs::remove_file(&journal_path);
        let _ = RecoveryGenerationStore::from_journal_path(&journal_path).remove_all();
        let mut project_temp_name = path
            .file_name()
            .unwrap_or_else(|| std::ffi::OsStr::new("session.kalium"))
            .to_os_string();
        project_temp_name.push(".tmp");
        let _ = std::fs::remove_file(path.with_file_name(project_temp_name));
        let _ = std::fs::remove_file(&path);
        let mut temp_name = journal_path
            .file_name()
            .unwrap_or_else(|| std::ffi::OsStr::new("session.journal"))
            .to_os_string();
        temp_name.push(".tmp");
        let _ = std::fs::remove_file(journal_path.with_file_name(temp_name));
    }

    fn release_autosave_session_lock(&mut self) {
        self.autosave_session_lock.take();
        self.autosave_session_lock_path = None;
    }

    fn acquire_project_recovery_lock(
        &mut self,
        project_path: &std::path::Path,
    ) -> anyhow::Result<()> {
        let lock_path = project_recovery_lock_path(project_path)?;
        if self.project_recovery_lock_path.as_deref() == Some(lock_path.as_path()) {
            return Ok(());
        }
        anyhow::ensure!(
            self.project_recovery_lock_path.is_none(),
            "this app already owns a different project recovery lock"
        );
        let (lock_path, lock) = open_project_recovery_lock(project_path)?;
        self.project_recovery_lock = Some(lock);
        self.project_recovery_lock_path = Some(lock_path);
        Ok(())
    }

    fn provisional_project_recovery_lock(
        &self,
        project_path: &std::path::Path,
    ) -> anyhow::Result<Option<(PathBuf, std::fs::File)>> {
        let lock_path = project_recovery_lock_path(project_path)?;
        if self.project_recovery_lock_path.as_deref() == Some(lock_path.as_path())
            || self
                .pending_open_project_lock
                .as_ref()
                .is_some_and(|(pending_path, _)| pending_path == &lock_path)
        {
            return Ok(None);
        }
        open_project_recovery_lock(project_path).map(Some)
    }

    fn release_project_recovery_lock(&mut self) {
        self.project_recovery_lock.take();
        self.project_recovery_lock_path = None;
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
        let journal = Journal::create(journal_path, self.doc.project_id, digest, JOURNAL_SCHEMA)?;
        self.recovery_journal = Some(JournalWriter::new(
            journal,
            RECOVERY_WRITER_QUEUE_ITEMS,
            RECOVERY_WRITER_QUEUE_BYTES,
        )?);
        self.recovery_journal_path = Some(journal_path.to_owned());
        self.recovery_unavailable = None;
        self.recovery_data_rejected = false;
        Ok(())
    }

    fn stop_recovery_writer(&mut self) -> Result<(), String> {
        let mut first_error = self
            .recovery_journal
            .take()
            .and_then(|writer| writer.shutdown().err());
        for writer in self.retired_recovery_writers.drain(..) {
            if let Err(error) = writer.shutdown() {
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    fn reap_retired_recovery_writers(&mut self) {
        let mut still_running = Vec::new();
        for writer in self.retired_recovery_writers.drain(..) {
            if writer.is_finished() {
                if let Err(error) = writer.shutdown() {
                    self.status = format!("A disabled recovery writer failed: {error}");
                }
            } else {
                still_running.push(writer);
            }
        }
        self.retired_recovery_writers = still_running;
    }

    fn reset_recovery_journal_after_save(
        &mut self,
        path: &std::path::Path,
        same_project_target: bool,
    ) -> anyhow::Result<()> {
        if same_project_target && self.recovery_data_rejected {
            anyhow::bail!("Previously rejected recovery data was preserved. Use Save As to a different path to start new crash recovery");
        }
        let journal_path = recovery_journal_path(path);
        self.stop_recovery_writer().map_err(anyhow::Error::msg)?;
        self.pending_journal_recovery = None;
        let store = RecoveryGenerationStore::from_journal_path(&journal_path);
        let active_generation = if same_project_target {
            match store.read_active(self.doc.project_id, 1) {
                Ok(generation) => generation,
                Err(error) => {
                    self.mark_recovery_data_rejected(path);
                    return Err(error).context("Rejected recovery generation was preserved");
                }
            }
        } else {
            None
        };
        let has_active_generation = active_generation.is_some();
        if has_active_generation || !same_project_target {
            let sequence_high_water = if let Some(active) = active_generation.as_ref() {
                let validated_sequence = (|| -> anyhow::Result<u64> {
                    let records = recovery::scan(
                        &active.journal_path,
                        active.project_id,
                        active.baseline_digest,
                        active.schema_version,
                    )?;
                    active
                        .sequence_high_water
                        .checked_add(recovery::committed_record_count(&records)? as u64)
                        .ok_or_else(|| anyhow::anyhow!("recovery sequence number overflow"))
                })();
                match validated_sequence {
                    Ok(sequence) => sequence,
                    Err(error) => {
                        self.mark_recovery_data_rejected(path);
                        return Err(error).context("Rejected recovery generation was preserved");
                    }
                }
            } else {
                0
            };
            let generation = if has_active_generation {
                store.publish_snapshot(
                    self.doc.project_id,
                    1,
                    sequence_high_water,
                    |stage_path| {
                        std::fs::copy(path, stage_path)?;
                        Ok(())
                    },
                )?
            } else {
                store.publish_clean_snapshot(
                    self.doc.project_id,
                    1,
                    sequence_high_water,
                    |stage_path| {
                        std::fs::copy(path, stage_path)?;
                        Ok(())
                    },
                )?
            };
            let journal = Journal::open(
                &generation.journal_path,
                generation.project_id,
                generation.baseline_digest,
                generation.schema_version,
            )?
            .0;
            self.recovery_journal = Some(JournalWriter::new(
                journal,
                RECOVERY_WRITER_QUEUE_ITEMS,
                RECOVERY_WRITER_QUEUE_BYTES,
            )?);
            self.recovery_journal_path = Some(generation.journal_path);
            if journal_path.exists() {
                let _ = std::fs::remove_file(&journal_path);
            }
            let legacy_disabled_path = recovery_journal_disabled_path(&journal_path);
            if legacy_disabled_path.exists() {
                let _ = std::fs::remove_file(legacy_disabled_path);
            }
            self.recovery_unavailable = None;
            self.recovery_data_rejected = false;
            return Ok(());
        }
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

    fn compact_recovery_journal_if_needed(&mut self) {
        self.compact_recovery_journal_if_needed_at(RECOVERY_COMPACTION_THRESHOLD_BYTES);
    }

    fn compact_recovery_journal_if_needed_at(&mut self, threshold_bytes: u64) {
        if self.recovery_journal.is_none()
            || self.recovery_unavailable.is_some()
            || self
                .recovery_compaction_retry_after
                .is_some_and(|deadline| Instant::now() < deadline)
        {
            return;
        }
        let Some(journal_path) = self.recovery_journal_path.as_ref() else {
            return;
        };
        let Ok(metadata) = std::fs::metadata(journal_path) else {
            return;
        };
        if metadata.len() < threshold_bytes
            || self.pending_stroke.is_some()
            || self.pending_history_change.is_some()
            || self.pending_journal_recovery.is_some()
            || self.material_id_bake_job.is_some()
            || self.ao_bake_job.is_some()
            || self.export_job.is_some()
        {
            return;
        }
        if let Err(error) = self.start_recovery_compaction() {
            let message = format!("Recovery checkpoint could not start: {error:#}");
            self.status = message.clone();
            if self.recovery_journal.is_none() {
                self.disable_recovery_journal(&message);
            } else {
                self.recovery_unavailable = None;
                self.recovery_compaction_retry_after =
                    Some(Instant::now() + Duration::from_secs(5));
            }
        }
    }

    fn start_recovery_compaction(&mut self) -> anyhow::Result<()> {
        #[cfg(test)]
        {
            self.start_recovery_compaction_inner(None, false)
        }
        #[cfg(not(test))]
        self.start_recovery_compaction_inner(None)
    }

    #[cfg(test)]
    fn start_recovery_compaction_paused(
        &mut self,
        start_gate: std::sync::mpsc::Receiver<()>,
    ) -> anyhow::Result<()> {
        self.start_recovery_compaction_inner(Some(start_gate), false)
    }

    #[cfg(test)]
    fn start_recovery_compaction_failing_before_publish(&mut self) -> anyhow::Result<()> {
        self.start_recovery_compaction_inner(None, true)
    }

    fn start_recovery_compaction_inner(
        &mut self,
        #[cfg(test)] start_gate: Option<std::sync::mpsc::Receiver<()>>,
        #[cfg(test)] fail_before_publish: bool,
        #[cfg(not(test))] _unused: Option<()>,
    ) -> anyhow::Result<()> {
        let old_journal_path = self
            .recovery_journal_path
            .clone()
            .ok_or_else(|| anyhow::anyhow!("recovery journal path is unavailable"))?;
        let origin_path = self
            .autosave_session_path
            .clone()
            .or_else(|| self.file_path.clone())
            .ok_or_else(|| anyhow::anyhow!("recovery baseline path is unavailable"))?;
        let store =
            RecoveryGenerationStore::from_journal_path(&recovery_journal_path(&origin_path));
        anyhow::ensure!(
            self.recovery_journal.is_some(),
            "recovery journal writer is unavailable"
        );

        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("kalium-recovery-checkpoint".to_owned())
            .spawn(move || {
                let input = receiver
                    .recv()
                    .expect("compaction input is sent after worker startup");
                run_recovery_compaction(input)
            })
            .context("start recovery checkpoint worker")?;

        let doc = std::mem::replace(&mut self.doc, Document::new(64));
        let input = RecoveryCompactionInput {
            doc,
            current_writer: self.recovery_journal.take(),
            retired_writers: std::mem::take(&mut self.retired_recovery_writers),
            old_journal_path,
            origin_path,
            store,
            #[cfg(test)]
            start_gate,
            #[cfg(test)]
            fail_before_publish,
        };
        if let Err(error) = sender.send(input) {
            let mut input = error.0;
            self.doc = input.doc;
            self.recovery_journal = input.current_writer.take();
            self.retired_recovery_writers = input.retired_writers;
            let _ = worker.join();
            anyhow::bail!("recovery checkpoint worker stopped before accepting the document");
        }
        self.recovery_compaction_job = Some(worker);
        self.paint_dirty = true;
        self.model_dirty = true;
        self.status = "Writing recovery checkpoint…".into();
        Ok(())
    }

    fn apply_recovery_compaction_output(&mut self, output: RecoveryCompactionOutput) {
        self.doc = output.doc;
        self.paint_dirty = true;
        self.model_dirty = true;
        self.model_partial_dirty.clear();
        self.uv_texture = None;
        self.rebuild_coverage();
        match output.result {
            Ok(generation) => {
                if let Err(error) = self.open_recovery_generation_writer(&generation) {
                    let message = format!(
                        "Checkpoint published, but its journal could not be opened: {error:#}"
                    );
                    self.recovery_unavailable = Some(message.clone());
                    self.status = message;
                } else {
                    self.recovery_compaction_retry_after = None;
                    self.status = generation.cleanup_warning.map_or_else(
                        || "Recovery checkpoint compacted".to_owned(),
                        |warning| {
                            format!(
                                "Recovery checkpoint compacted; old files need cleanup: {warning}"
                            )
                        },
                    );
                }
            }
            Err(error) => {
                let message = format!("Recovery checkpoint could not be compacted: {error:#}");
                self.status = message;
                if let Err(restart_error) = self.restart_recovery_writer_after_compaction() {
                    let unavailable = format!(
                        "{}; the previous recovery journal could not be reopened: {restart_error:#}",
                        self.status
                    );
                    self.disable_recovery_journal(&unavailable);
                } else {
                    self.recovery_compaction_retry_after =
                        Some(Instant::now() + Duration::from_secs(5));
                }
            }
        }
    }

    fn show_recovery_compaction_progress(&mut self, ctx: &egui::Context) {
        if ctx.input(|input| input.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        }
        self.status_bar(ctx);
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(ui.available_height() * 0.4);
                ui.spinner();
                ui.label("Writing recovery checkpoint. Editing is paused until it is ready.");
            });
        });
        ctx.request_repaint_after(Duration::from_millis(24));
    }

    fn finish_recovery_compaction(&mut self) {
        let Some(worker) = self.recovery_compaction_job.take() else {
            return;
        };
        match worker.join() {
            Ok(output) => self.apply_recovery_compaction_output(output),
            Err(_) => {
                let origin_path = self
                    .autosave_session_path
                    .clone()
                    .or_else(|| self.file_path.clone());
                let Some(origin_path) = origin_path else {
                    self.recovery_unavailable = Some(
                        "Checkpoint worker failed and no recovery baseline is available".into(),
                    );
                    self.status = self.recovery_unavailable.clone().unwrap();
                    return;
                };
                match project::load(&origin_path) {
                    Ok(doc) => {
                        self.install_open_project(doc, origin_path, false);
                        self.status =
                            "Checkpoint worker failed; project reloaded from recovery files".into();
                        self.paint_dirty = true;
                        self.model_dirty = true;
                    }
                    Err(error) => {
                        let message = format!(
                            "Checkpoint worker failed and recovery baseline could not be loaded: {error:#}"
                        );
                        self.recovery_unavailable = Some(message.clone());
                        self.status = message;
                    }
                }
            }
        }
    }

    fn open_recovery_generation_writer(
        &mut self,
        generation: &RecoveryGeneration,
    ) -> anyhow::Result<()> {
        let journal = Journal::open(
            &generation.journal_path,
            generation.project_id,
            generation.baseline_digest,
            generation.schema_version,
        )?
        .0;
        self.recovery_journal = Some(JournalWriter::new(
            journal,
            RECOVERY_WRITER_QUEUE_ITEMS,
            RECOVERY_WRITER_QUEUE_BYTES,
        )?);
        self.recovery_journal_path = Some(generation.journal_path.clone());
        self.recovery_unavailable = None;
        Ok(())
    }

    fn restart_recovery_writer_after_compaction(&mut self) -> anyhow::Result<()> {
        const JOURNAL_SCHEMA: u32 = 1;
        let origin_path = self
            .autosave_session_path
            .clone()
            .or_else(|| self.file_path.clone())
            .ok_or_else(|| anyhow::anyhow!("recovery baseline path is unavailable"))?;
        let legacy_journal_path = recovery_journal_path(&origin_path);
        let store = RecoveryGenerationStore::from_journal_path(&legacy_journal_path);
        let (path, digest) = match store.read_active(self.doc.project_id, JOURNAL_SCHEMA)? {
            Some(generation) => (generation.journal_path, generation.baseline_digest),
            None => (
                self.recovery_journal_path
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("recovery journal path is unavailable"))?,
                project::file_digest(&origin_path)?,
            ),
        };
        let (journal, _) = Journal::open(&path, self.doc.project_id, digest, JOURNAL_SCHEMA)?;
        self.recovery_journal = Some(JournalWriter::new(
            journal,
            RECOVERY_WRITER_QUEUE_ITEMS,
            RECOVERY_WRITER_QUEUE_BYTES,
        )?);
        self.recovery_journal_path = Some(path);
        self.recovery_unavailable = None;
        Ok(())
    }

    #[cfg(test)]
    fn compact_recovery_journal(&mut self) -> anyhow::Result<()> {
        const JOURNAL_SCHEMA: u32 = 1;
        let old_journal_path = self
            .recovery_journal_path
            .clone()
            .ok_or_else(|| anyhow::anyhow!("recovery journal path is unavailable"))?;
        let origin_path = self
            .autosave_session_path
            .clone()
            .or_else(|| self.file_path.clone())
            .ok_or_else(|| anyhow::anyhow!("recovery baseline path is unavailable"))?;
        let legacy_journal_path = recovery_journal_path(&origin_path);
        let store = RecoveryGenerationStore::from_journal_path(&legacy_journal_path);
        let active = store.read_active(self.doc.project_id, JOURNAL_SCHEMA)?;
        let (baseline_digest, sequence_anchor) = match active.as_ref() {
            Some(generation) => (generation.baseline_digest, generation.sequence_high_water),
            None => (project::file_digest(&origin_path)?, 0),
        };

        self.stop_recovery_writer().map_err(anyhow::Error::msg)?;
        let result = (|| -> anyhow::Result<()> {
            let records = recovery::scan(
                &old_journal_path,
                self.doc.project_id,
                baseline_digest,
                JOURNAL_SCHEMA,
            )?;
            let committed_count = recovery::committed_record_count(&records)?;
            anyhow::ensure!(
                committed_count == records.len(),
                "journal ends with an incomplete edit transaction"
            );
            let sequence_high_water = sequence_anchor
                .checked_add(committed_count as u64)
                .ok_or_else(|| anyhow::anyhow!("recovery sequence number overflow"))?;

            let generation = store.publish_snapshot(
                self.doc.project_id,
                JOURNAL_SCHEMA,
                sequence_high_water,
                |stage_path| project::save_checkpoint(&self.doc, stage_path),
            );
            let generation = match generation {
                Ok(generation) => generation,
                Err(publish_error) => {
                    // The manifest may already point at this complete pair if
                    // only its final directory sync failed.
                    match store.read_active(self.doc.project_id, JOURNAL_SCHEMA) {
                        Ok(Some(generation))
                            if generation.sequence_high_water == sequence_high_water =>
                        {
                            generation
                        }
                        _ => anyhow::bail!("{publish_error:#}"),
                    }
                }
            };
            let journal = Journal::open(
                &generation.journal_path,
                generation.project_id,
                generation.baseline_digest,
                generation.schema_version,
            )?
            .0;
            let writer = JournalWriter::new(
                journal,
                RECOVERY_WRITER_QUEUE_ITEMS,
                RECOVERY_WRITER_QUEUE_BYTES,
            )?;
            self.recovery_journal = Some(writer);
            self.recovery_journal_path = Some(generation.journal_path);
            self.recovery_unavailable = None;
            self.recovery_compaction_retry_after = None;
            self.status = generation.cleanup_warning.map_or_else(
                || "Recovery checkpoint compacted".to_owned(),
                |warning| {
                    format!("Recovery checkpoint compacted; old files need cleanup: {warning}")
                },
            );
            Ok(())
        })();
        match result {
            Ok(()) => Ok(()),
            Err(error) => {
                if self.recovery_journal.is_none() {
                    let active_generation = store
                        .read_active(self.doc.project_id, JOURNAL_SCHEMA)
                        .ok()
                        .flatten();
                    let (restart_path, restart_digest) = active_generation
                        .as_ref()
                        .map_or((old_journal_path.clone(), baseline_digest), |generation| {
                            (generation.journal_path.clone(), generation.baseline_digest)
                        });
                    if let Ok((journal, _)) = Journal::open(
                        &restart_path,
                        self.doc.project_id,
                        restart_digest,
                        JOURNAL_SCHEMA,
                    ) {
                        if let Ok(writer) = JournalWriter::new(
                            journal,
                            RECOVERY_WRITER_QUEUE_ITEMS,
                            RECOVERY_WRITER_QUEUE_BYTES,
                        ) {
                            self.recovery_journal = Some(writer);
                            self.recovery_journal_path = Some(restart_path);
                        }
                    }
                }
                Err(error)
            }
        }
    }

    fn disable_recovery_journal(&mut self, reason: &str) {
        self.recovery_unavailable = Some(reason.to_owned());
        if self.recovery_data_rejected {
            let _ = self.stop_recovery_writer();
            self.status = format!("Crash recovery rejected; original data preserved: {reason}");
            return;
        }
        let Some(journal_path) = self.recovery_journal_path.clone() else {
            let _ = self.stop_recovery_writer();
            return;
        };
        let disabled_path = recovery_journal_disabled_path(&journal_path);
        match std::fs::write(disabled_path, reason.as_bytes()) {
            Ok(()) => {
                if let Some(mut writer) = self.recovery_journal.take() {
                    writer.stop_accepting();
                    self.retired_recovery_writers.push(writer);
                }
                self.status = format!("Crash recovery paused: {reason}");
            }
            Err(error) => {
                let shutdown_error = self.stop_recovery_writer().err();
                match invalidate_recovery_journal_file(&journal_path) {
                    Ok(()) => {
                        self.status = format!(
                            "Crash recovery paused: {reason}; its marker could not be written ({error}), so the journal header was invalidated to prevent replay{}",
                            shutdown_error
                                .as_deref()
                                .map(|error| format!("; writer shutdown failed: {error}"))
                                .unwrap_or_default()
                        );
                    }
                    Err(invalidate_error) => {
                        let remove_result = std::fs::remove_file(&journal_path);
                        self.status = match remove_result {
                            Ok(()) => format!(
                                "Crash recovery paused: {reason}; marker creation failed ({error}) and journal invalidation failed ({invalidate_error}), so the journal was removed{}",
                                shutdown_error
                                    .as_deref()
                                    .map(|error| format!("; writer shutdown failed: {error}"))
                                    .unwrap_or_default()
                            ),
                            Err(remove_error) => format!(
                                "Crash recovery could not be safely disabled: {reason}; marker creation failed ({error}), journal invalidation failed ({invalidate_error}), journal removal failed ({remove_error}), and writer shutdown was {}",
                                shutdown_error.as_deref().unwrap_or("successful")
                            ),
                        };
                    }
                }
            }
        }
    }

    fn append_recovery_batch(&mut self, events: &[DocumentEvent]) {
        self.append_recovery_batch_inner(events, true);
    }

    fn append_history_recovery_sequence(
        &mut self,
        direction: HistoryDirection,
    ) -> Result<Vec<JournalTicket>, String> {
        let mut batch = Vec::new();
        let mut tickets = Vec::new();
        let mut encoded_bytes = 0usize;
        let mut append_error = None;
        let journal = &mut self.recovery_journal;
        let mut queued_tickets = Vec::new();
        let plan_result = self.history.visit_recovery_events(
            &self.doc,
            self.doc.active_set_id,
            direction,
            |event| {
                let encoded_len = match event.encode_bounded() {
                    Ok(payload) => payload.len(),
                    Err(error) => {
                        append_error =
                            Some(format!("could not encode an undo or redo event: {error}"));
                        return false;
                    }
                };
                if !batch.is_empty()
                    && encoded_bytes
                        .checked_add(encoded_len)
                        .is_none_or(|size| size > HISTORY_RECOVERY_BATCH_BYTES)
                {
                    match append_recovery_events_to_journal(journal, &batch, false) {
                        Ok(ticket) => {
                            #[cfg(test)]
                            let mut ticket = ticket;
                            #[cfg(test)]
                            if let Err(error) = pause_after_async_snapshot_batch(&mut ticket) {
                                append_error = Some(error);
                                return false;
                            }
                            queued_tickets.push(ticket);
                        }
                        Err(error) => {
                            append_error = Some(error);
                            return false;
                        }
                    }
                    batch.clear();
                    encoded_bytes = 0;
                }
                if batch.try_reserve(1).is_err() {
                    append_error = Some(
                        "could not reserve memory for bounded undo or redo journal batches"
                            .to_owned(),
                    );
                    return false;
                }
                batch.push(event);
                encoded_bytes = encoded_bytes.saturating_add(encoded_len);
                true
            },
        );
        if plan_result.is_none() {
            return Err("undo or redo has no recovery event plan".to_owned());
        }
        if let Some(error) = append_error {
            self.status = format!("Crash recovery paused: {error}");
            return Err(error);
        }
        if !batch.is_empty() {
            match append_recovery_events_to_journal(journal, &batch, false) {
                Ok(ticket) => {
                    #[cfg(test)]
                    let mut ticket = ticket;
                    #[cfg(test)]
                    if let Err(error) = pause_after_async_snapshot_batch(&mut ticket) {
                        self.status = format!("Crash recovery paused: {error}");
                        return Err(error);
                    }
                    queued_tickets.push(ticket);
                }
                Err(error) => {
                    self.status = format!("Crash recovery paused: {error}");
                    return Err(error);
                }
            }
        }
        if queued_tickets.is_empty() {
            return Err("undo or redo produced no recovery records".to_owned());
        }
        tickets.append(&mut queued_tickets);
        Ok(tickets)
    }

    fn append_mesh_recovery_transaction_to(
        journal: &mut Option<JournalWriter>,
        doc: &Document,
        mesh: &Mesh,
        material_bindings: &[Option<u32>],
        wait_for_durability: bool,
    ) -> anyhow::Result<Vec<JournalTicket>> {
        if material_bindings.len() != mesh.material_count()
            || material_bindings.iter().flatten().any(|set_id| {
                *set_id != doc.active_set_id
                    && !doc.texture_sets.iter().any(|set| set.id == *set_id)
            })
        {
            anyhow::bail!("mesh recovery bindings do not match the target mesh and document");
        }
        let (encoded_len, sha256) = mesh_recovery_fingerprint(mesh)?;
        let transaction_id = uuid::Uuid::new_v4();
        let begin = DocumentEvent::MeshReplaceBegin {
            transaction_id,
            encoded_len: encoded_len as u64,
            sha256,
            material_bindings: material_bindings.to_vec(),
        };
        let mut tickets = vec![append_recovery_events_to_journal(
            journal,
            std::slice::from_ref(&begin),
            wait_for_durability,
        )
        .map_err(anyhow::Error::msg)?];

        let mut writer = MeshChunkJournalWriter::new(
            journal,
            &mut tickets,
            wait_for_durability,
            transaction_id,
        )?;
        serde_json::to_writer(&mut writer, mesh)?;
        let (written_len, written_sha256) = writer.finish()?;
        if written_len != encoded_len as u64 || written_sha256 != sha256 {
            anyhow::bail!("mesh serialization changed while journaling the import");
        }

        let commit = DocumentEvent::MeshReplaceCommit { transaction_id };
        tickets.push(
            append_recovery_events_to_journal(
                journal,
                std::slice::from_ref(&commit),
                wait_for_durability,
            )
            .map_err(anyhow::Error::msg)?,
        );
        Ok(tickets)
    }

    fn append_recovery_batch_inner(&mut self, events: &[DocumentEvent], append_selection: bool) {
        if events.is_empty() {
            return;
        }
        if self.recovery_journal.is_none() {
            return;
        }
        let mut batch = Vec::new();
        if batch
            .try_reserve_exact(events.len().saturating_add(usize::from(append_selection)))
            .is_err()
        {
            self.disable_recovery_journal("could not reserve recovery event batch memory");
            return;
        }
        batch.extend_from_slice(events);
        if append_selection {
            batch.push(DocumentEvent::LayerSelection {
                set_id: self.doc.active_set_id,
                layer_id: self.doc.layer_tree.selected_node_id,
            });
        }
        let payload = match recovery::encode_event_batch_bounded(&batch) {
            Ok(payload) => payload,
            Err(error) => {
                self.disable_recovery_journal(&format!("could not encode the edit: {error}"));
                return;
            }
        };
        let result = enqueue_recovery_payload_to(&mut self.recovery_journal, payload, true);
        if let Err(error) = result {
            self.disable_recovery_journal(&format!("journal append failed: {error}"));
        }
    }

    fn journal_channel_layer(&mut self, channel: Channel, imported: bool) {
        if self.recovery_journal.is_none() {
            return;
        }
        let (set_id, layer_id, to_index, name, size) = {
            let layer = self
                .doc
                .selected_paint_layer()
                .expect("channel layers are always Paint nodes");
            (
                self.doc.active_set_id,
                layer.id,
                self.doc
                    .layer_tree
                    .path_to(layer.id)
                    .and_then(|path| path.last().copied())
                    .unwrap_or(0),
                layer.name.clone(),
                self.doc.size,
            )
        };
        if size == 0 || size > crate::document::MAX_SIZE {
            self.disable_recovery_journal("channel layer resolution is outside recovery limits");
            return;
        }

        let begin = if imported {
            DocumentEvent::TextureImportBegin {
                set_id,
                layer_id,
                channel,
            }
        } else {
            DocumentEvent::LayerChannelBegin {
                set_id,
                layer_id,
                channel,
            }
        };
        self.append_recovery_batch_inner(std::slice::from_ref(&begin), false);
        if self.recovery_journal.is_none() {
            return;
        }

        let create = DocumentEvent::LayerCreate {
            set_id,
            layer_id,
            to_index,
            name,
        };
        self.append_recovery_batch_inner(std::slice::from_ref(&create), false);
        self.allow_recovery_change = self.recovery_journal.is_some();
        if self.recovery_journal.is_none() {
            return;
        }
        let Some(row_bytes) = usize::try_from(size)
            .ok()
            .and_then(|size| size.checked_mul(4))
        else {
            self.disable_recovery_journal("channel layer recovery row size overflowed");
            return;
        };
        if row_bytes > RECOVERY_IMAGE_PATCH_BYTES {
            self.disable_recovery_journal("channel layer row exceeds the recovery patch limit");
            return;
        }
        let rows_per_patch = (RECOVERY_IMAGE_PATCH_BYTES / row_bytes).max(1);
        let height = size as usize;
        let mut generated_patch_batch = Vec::new();
        if !imported
            && generated_patch_batch
                .try_reserve_exact(GENERATED_PATCHES_PER_RECORD)
                .is_err()
        {
            self.disable_recovery_journal(
                "could not reserve generated channel recovery batch memory",
            );
            return;
        }
        for first_row in (0..height).step_by(rows_per_patch) {
            let rows = rows_per_patch.min(height - first_row);
            let patch = {
                let Some(image) = self
                    .doc
                    .selected_paint_layer()
                    .and_then(|layer| layer.pixels.get(&channel))
                else {
                    self.disable_recovery_journal(
                        "channel layer data disappeared before journaling",
                    );
                    return;
                };
                if image.dimensions() != (size, size) {
                    self.disable_recovery_journal(
                        "channel layer dimensions changed before journaling",
                    );
                    return;
                }
                let Some(start) = first_row.checked_mul(row_bytes) else {
                    self.disable_recovery_journal("channel layer patch offset overflowed");
                    return;
                };
                let Some(length) = rows.checked_mul(row_bytes) else {
                    self.disable_recovery_journal("channel layer patch size overflowed");
                    return;
                };
                let Some(end) = start.checked_add(length) else {
                    self.disable_recovery_journal("channel layer patch range overflowed");
                    return;
                };
                let Some(source) = image.as_raw().get(start..end) else {
                    self.disable_recovery_journal(
                        "channel layer bytes do not match their dimensions",
                    );
                    return;
                };
                let mut bytes = Vec::new();
                if bytes.try_reserve_exact(length).is_err() {
                    self.disable_recovery_journal(
                        "could not reserve channel layer recovery patch memory",
                    );
                    return;
                }
                bytes.extend_from_slice(source);
                DocumentEvent::LayerImagePatch {
                    set_id,
                    layer_id,
                    target: LayerImageTarget::Channel(channel),
                    x: 0,
                    y: first_row as u32,
                    width: size,
                    height: rows as u32,
                    bytes,
                }
            };
            if imported {
                self.append_recovery_batch_inner(std::slice::from_ref(&patch), false);
                self.allow_recovery_change = self.recovery_journal.is_some();
                if self.recovery_journal.is_none() {
                    return;
                }
            } else {
                generated_patch_batch.push(patch);
                if generated_patch_batch.len() == GENERATED_PATCHES_PER_RECORD {
                    self.append_recovery_batch_inner(&generated_patch_batch, false);
                    generated_patch_batch.clear();
                    self.allow_recovery_change = self.recovery_journal.is_some();
                    if self.recovery_journal.is_none() {
                        return;
                    }
                }
            }
        }
        if !generated_patch_batch.is_empty() {
            self.append_recovery_batch_inner(&generated_patch_batch, false);
            self.allow_recovery_change = self.recovery_journal.is_some();
            if self.recovery_journal.is_none() {
                return;
            }
        }
        let select = DocumentEvent::LayerSelection { set_id, layer_id };
        self.append_recovery_batch_inner(std::slice::from_ref(&select), false);
        if self.recovery_journal.is_none() {
            return;
        }
        let commit = if imported {
            DocumentEvent::TextureImportCommit {
                set_id,
                layer_id,
                channel,
            }
        } else {
            DocumentEvent::LayerChannelCommit {
                set_id,
                layer_id,
                channel,
            }
        };
        self.append_recovery_batch_inner(std::slice::from_ref(&commit), false);
        self.allow_recovery_change = self.recovery_journal.is_some();
    }

    fn import_texture_path(&mut self, path: &std::path::Path) -> anyhow::Result<()> {
        self.finish_stroke();
        let channel = self.channel;
        ops::import_texture_with_history(&mut self.doc, &mut self.history, channel, path)?;
        self.journal_channel_layer(channel, true);
        self.change();
        Ok(())
    }

    fn import_mesh_path(&mut self, path: &std::path::Path) -> anyhow::Result<bool> {
        let mesh = Mesh::from_file(path)?;
        self.install_imported_mesh(mesh)
    }

    fn install_imported_mesh(&mut self, mesh: Mesh) -> anyhow::Result<bool> {
        mesh.validate()?;
        self.finish_stroke();
        let new_bindings = vec![Some(self.doc.active_set_id); mesh.material_count()];
        if self.recovery_journal.is_some() {
            let wait_for_durability = {
                #[cfg(test)]
                {
                    std::env::var_os("KALIUM_MESH_RECOVERY_PROCESS_READY").is_none()
                }
                #[cfg(not(test))]
                {
                    true
                }
            };
            if let Err(error) = Self::append_mesh_recovery_transaction_to(
                &mut self.recovery_journal,
                &self.doc,
                &mesh,
                &new_bindings,
                wait_for_durability,
            ) {
                self.disable_recovery_journal(&format!(
                    "Mesh import could not be recorded for crash recovery: {error:#}. Save after import to establish a new recovery baseline."
                ));
            }
        }
        self.allow_recovery_change = self.recovery_journal.is_some();
        let (previous_mesh, previous_bindings) = self.doc.replace_mesh(mesh)?;
        self.rebuild_coverage();
        let undoable = self
            .history
            .record_mesh_replacement(previous_mesh, previous_bindings);
        if !undoable {
            self.history.clear();
        }
        self.change();
        Ok(undoable)
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

    fn commit_uniform_recovery_event(&mut self, event: DocumentEvent) {
        if self.recovery_journal.is_some() {
            self.append_recovery_batch(std::slice::from_ref(&event));
            self.allow_recovery_change = self.recovery_journal.is_some();
        }
    }

    fn resolve_journal_recovery(&mut self, choice: JournalRecoveryChoice) {
        let Some(pending) = self.pending_journal_recovery.take() else {
            return;
        };
        match choice {
            JournalRecoveryChoice::Restore => {
                match project::load(&pending.baseline_path) {
                    Ok(baseline) => self.doc = baseline,
                    Err(error) => {
                        self.mark_recovery_data_rejected(&pending.project_path);
                        self.recovery_journal_path = Some(pending.journal_path.clone());
                        self.disable_recovery_journal(&format!(
                            "recovery baseline could not be reopened: {error:#}"
                        ));
                        self.status = format!("Recovery baseline was rejected: {error:#}");
                        return;
                    }
                }
                let mut replayer = recovery::MeshRecoveryReplayer::new();
                let replay = pending
                    .records
                    .iter()
                    .try_for_each(|record| {
                        let events = recovery::decode_event_batch_bounded(&record.payload)?;
                        for event in events {
                            replayer.apply_event(&mut self.doc, event)?;
                        }
                        Ok::<(), anyhow::Error>(())
                    })
                    .and_then(|()| replayer.finish())
                    .and_then(|()| self.doc.validate());
                if let Err(error) = replay {
                    let project_path = pending.project_path.clone();
                    let journal_path = pending.journal_path.clone();
                    match project::load(&pending.project_path) {
                        Ok(base) => self.install_project(base, project_path),
                        Err(load_error) => {
                            self.mark_recovery_data_rejected(&pending.project_path);
                            self.recovery_journal = None;
                            self.recovery_journal_path = Some(journal_path);
                            self.status = format!(
                                "Journal replay failed ({error:#}); reopening the saved project also failed ({load_error:#})"
                            );
                            return;
                        }
                    }
                    self.mark_recovery_data_rejected(&pending.project_path);
                    self.recovery_journal_path = Some(journal_path);
                    self.disable_recovery_journal(
                        "journal replay failed validation; the saved project was reloaded",
                    );
                    self.status = format!(
                        "Crash recovery was rejected and the saved project was reloaded: {error:#}"
                    );
                    return;
                }
                self.ao_settings = ao_settings_for_active_set(&self.doc);
                self.rebuild_coverage();
                self.texture_set_name_input = self.doc.active_set_name.clone();
                match JournalWriter::new(
                    pending.journal,
                    RECOVERY_WRITER_QUEUE_ITEMS,
                    RECOVERY_WRITER_QUEUE_BYTES,
                ) {
                    Ok(writer) => self.recovery_journal = Some(writer),
                    Err(error) => {
                        self.recovery_journal_path = Some(pending.journal_path);
                        self.disable_recovery_journal(&format!(
                            "recovery writer could not start after replay: {error}"
                        ));
                        return;
                    }
                }
                self.recovery_journal_path = Some(pending.journal_path);
                self.allow_recovery_change = true;
                self.change();
                self.status = "Unsaved edits restored from the recovery journal".into();
            }
            JournalRecoveryChoice::Discard => {
                drop(pending.journal);
                let clean_base = match project::load(&pending.project_path) {
                    Ok(doc) => doc,
                    Err(error) => {
                        self.mark_recovery_data_rejected(&pending.project_path);
                        self.recovery_journal_path = Some(pending.journal_path.clone());
                        self.disable_recovery_journal(&format!(
                            "could not reload the saved recovery baseline: {error:#}"
                        ));
                        self.status = format!("Recovery discard failed: {error:#}");
                        return;
                    }
                };
                let legacy_journal = recovery_journal_path(&pending.project_path);
                let store = RecoveryGenerationStore::from_journal_path(&legacy_journal);
                let generation = store.publish_clean_snapshot(
                    clean_base.project_id,
                    1,
                    pending.sequence_high_water,
                    |stage_path| {
                        std::fs::copy(&pending.project_path, stage_path)?;
                        Ok(())
                    },
                );
                match generation.and_then(|generation| {
                    let (journal, records) = Journal::open(
                        &generation.journal_path,
                        generation.project_id,
                        generation.baseline_digest,
                        generation.schema_version,
                    )?;
                    anyhow::ensure!(records.is_empty(), "clean recovery journal is not empty");
                    Ok((generation, journal))
                }) {
                    Ok((generation, journal)) => {
                        self.recovery_journal_path = Some(generation.journal_path.clone());
                        match JournalWriter::new(
                            journal,
                            RECOVERY_WRITER_QUEUE_ITEMS,
                            RECOVERY_WRITER_QUEUE_BYTES,
                        ) {
                            Ok(writer) => self.recovery_journal = Some(writer),
                            Err(error) => {
                                self.recovery_journal = None;
                                self.disable_recovery_journal(&format!(
                                    "recovery was discarded, but the new journal writer could not start: {error}"
                                ));
                            }
                        }
                        self.doc = clean_base;
                        if legacy_journal.exists() {
                            let _ = std::fs::remove_file(&legacy_journal);
                        }
                        let legacy_disabled_path = recovery_journal_disabled_path(&legacy_journal);
                        if legacy_disabled_path.exists() {
                            let _ = std::fs::remove_file(legacy_disabled_path);
                        }
                        self.ao_settings = ao_settings_for_active_set(&self.doc);
                        self.texture_set_name_input = self.doc.active_set_name.clone();
                        self.rebuild_coverage();
                        self.history.clear();
                        self.allow_recovery_change = true;
                        self.unsaved = false;
                        if self.recovery_journal.is_some() {
                            self.status = "Recovery journal discarded".into();
                        }
                    }
                    Err(error) => {
                        self.recovery_journal = None;
                        self.recovery_journal_path = Some(pending.journal_path);
                        self.disable_recovery_journal(&format!(
                            "could not publish a clean recovery generation after discard: {error:#}"
                        ));
                        self.status = format!("Recovery was discarded from memory; autosave is unavailable: {error:#}");
                    }
                }
            }
        }
    }

    fn save_project(&mut self, save_as: bool) -> anyhow::Result<Option<String>> {
        if self.pending_journal_recovery.is_some() {
            anyhow::bail!("Restore or discard the pending recovery data before saving");
        }
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
        self.save_project_to_path(path).map(Some)
    }

    fn save_project_to_path(&mut self, path: PathBuf) -> anyhow::Result<String> {
        if path
            .extension()
            .and_then(|value| value.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("spp"))
        {
            anyhow::bail!(
                "SPP writing is not implemented; refusing to write a Kalium archive to an SPP path"
            );
        }
        if self.pending_journal_recovery.is_some() {
            anyhow::bail!("Restore or discard the pending recovery data before saving");
        }
        self.finish_stroke();
        if self
            .autosave_session_path
            .as_deref()
            .is_some_and(|session| paths_refer_to_same_file(session, &path))
        {
            anyhow::bail!("The per-user autosave baseline is reserved by Kalium");
        }
        let target_lock_path = project_recovery_lock_path(&path)?;
        let replacement_project_lock =
            if self.project_recovery_lock_path.as_deref() == Some(target_lock_path.as_path()) {
                None
            } else {
                Some(open_project_recovery_lock(&path)?)
            };
        let same_project_target = self
            .file_path
            .as_deref()
            .is_some_and(|current| paths_refer_to_same_file(current, &path));
        let previous_project_id = self.doc.project_id;
        self.doc.project_id =
            project_id_for_save_target(previous_project_id, self.file_path.as_deref(), &path);
        let durability_warning = match project::save(&self.doc, &path) {
            Ok(()) => None,
            Err(error)
                if error
                    .downcast_ref::<project::PostReplaceSyncError>()
                    .is_some() =>
            {
                Some(error.to_string())
            }
            Err(error) => {
                self.doc.project_id = previous_project_id;
                return Err(error);
            }
        };
        self.file_path = Some(path.clone());
        self.unsaved = false;
        let durability_uncertain = durability_warning.is_some();
        let mut saved_message = match durability_warning {
            Some(warning) => format!(
                "Saved {}; power-loss durability is uncertain: {warning}",
                path.display()
            ),
            None => format!("Saved {}", path.display()),
        };
        if let Some(warning) = self.record_recent_project(&path) {
            saved_message = format!("{saved_message}; {warning}");
        }
        if let Err(error) = self.reset_recovery_journal_after_save(&path, same_project_target) {
            if let Some((lock_path, lock)) = replacement_project_lock {
                self.project_recovery_lock = Some(lock);
                self.project_recovery_lock_path = Some(lock_path);
            }
            self.recovery_journal = None;
            self.recovery_journal_path = Some(recovery_journal_path(&path));
            let message = format!("Crash recovery is unavailable after the save: {error:#}");
            self.recovery_unavailable = Some(message.clone());
            self.remove_autosave_session_files();
            self.release_autosave_session_lock();
            let combined = format!("{saved_message}; {message}");
            self.record_activity(ActivityLevel::Warning, "Project", &combined);
            return Ok(combined);
        }
        if let Some((lock_path, lock)) = replacement_project_lock {
            self.project_recovery_lock = Some(lock);
            self.project_recovery_lock_path = Some(lock_path);
        }
        self.remove_autosave_session_files();
        self.release_autosave_session_lock();
        let level = if durability_uncertain {
            ActivityLevel::Warning
        } else {
            ActivityLevel::Info
        };
        self.record_activity(level, "Project", &saved_message);
        Ok(saved_message)
    }

    fn resolve_destructive_choice(&mut self, choice: DestructiveChoice, ctx: &egui::Context) {
        if choice == DestructiveChoice::Cancel {
            self.pending_destructive = None;
            self.pending_open_project_lock = None;
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
            if let Err(error) = self.stop_recovery_writer() {
                self.status =
                    format!("Recovery journal could not be drained before discard: {error}");
            }
            if let Some(path) = self.recovery_journal_path.take() {
                if !self.recovery_data_rejected {
                    let _ = std::fs::remove_file(recovery_journal_disabled_path(&path));
                    let _ = std::fs::remove_file(path);
                }
            }
            if !self.recovery_data_rejected {
                if let Some(path) = self.file_path.as_deref() {
                    let _ =
                        RecoveryGenerationStore::from_journal_path(&recovery_journal_path(path))
                            .remove_all();
                }
            }
            self.remove_autosave_session_files();
        }
        if let Some(action) = self.pending_destructive.take() {
            match action {
                DestructiveAction::New => self.start_new_project(),
                DestructiveAction::Open {
                    doc,
                    path,
                    recovered,
                } => self.install_open_project(*doc, path, recovered),
                DestructiveAction::Close => self.approve_close(ctx),
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
                let restore = ui.button("Restore edits");
                #[cfg(test)]
                ui.ctx().data_mut(|data| {
                    data.insert_temp(egui::Id::new("journal-recovery-restore-rect"), restore.rect);
                });
                if restore.clicked() {
                    choice = Some(JournalRecoveryChoice::Restore);
                }
                let discard = ui.button("Discard recovery data");
                #[cfg(test)]
                ui.ctx().data_mut(|data| {
                    data.insert_temp(egui::Id::new("journal-recovery-discard-rect"), discard.rect);
                });
                if discard.clicked() {
                    choice = Some(JournalRecoveryChoice::Discard);
                }
            });
        });
        if let Some(choice) = choice {
            self.resolve_journal_recovery(choice);
        }
    }

    fn perform(&mut self, action: Action) {
        if self.pending_journal_recovery.is_some()
            && matches!(
                action,
                Action::New
                    | Action::Open
                    | Action::Save
                    | Action::SaveAs
                    | Action::SaveCheckpoint
                    | Action::RecoverCheckpoint
            )
        {
            self.status = "Restore or discard the pending recovery data first".into();
            return;
        }
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
                self.request_open_project(path).map(Some)
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
                let undoable = self.import_mesh_path(&path)?;
                Ok(Some(if undoable {
                    format!("Imported {} — Undo available", path.display())
                } else {
                    format!(
                        "Imported {}; previous mesh was too large for the 96-MiB Undo budget",
                        path.display()
                    )
                }))
            }
            Action::ImportTexture => {
                let Some(path) = rfd::FileDialog::new()
                    .add_filter("Image texture", &["png", "jpg", "jpeg"])
                    .pick_file()
                else {
                    return Ok(None);
                };
                self.import_texture_path(&path)?;
                Ok(Some(format!(
                    "Imported texture {} as new {} layer",
                    path.display(),
                    self.channel.label()
                )))
            }
            Action::ImportBrushPreset => {
                let Some(source) = rfd::FileDialog::new()
                    .add_filter("Kalium brush preset", &["kbrush"])
                    .pick_file()
                else {
                    return Ok(None);
                };
                let Some(directory) = layout::brush_shelf_directory() else {
                    anyhow::bail!("User preset directory is unavailable");
                };
                let path = import_brush_preset_to_shelf(&source, &directory)?;
                self.refresh_brush_shelf();
                Ok(Some(format!(
                    "Added brush preset to shelf: {}",
                    path.display()
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
            Action::LoadBrushTip => {
                let Some(path) = rfd::FileDialog::new()
                    .add_filter("Alpha brush tip image", &["png", "jpg", "jpeg"])
                    .pick_file()
                else {
                    return Ok(None);
                };
                let tip = brush_preset::load_alpha_tip(&path)?;
                self.finish_stroke();
                self.brush.tip = tip;
                Ok(Some(format!("Loaded alpha brush tip {}", path.display())))
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
                let cached_channel_is_current = !self.paint_dirty && self.material.is_some();
                let Some(layer_id) = self.doc.selected_paint_layer().map(|layer| layer.id) else {
                    return Ok(Some(
                        "Select a Paint layer to edit pixels or its mask".into(),
                    ));
                };
                let result = ops::fill_selected_with_history(
                    &mut self.doc,
                    &mut self.history,
                    self.channel,
                    &self.brush,
                )?;
                if result.changed {
                    let event = match mode {
                        BrushMode::MaskWhite => DocumentEvent::LayerMaskSet {
                            set_id: self.doc.active_set_id,
                            layer_id,
                            value: 255,
                        },
                        BrushMode::MaskBlack => DocumentEvent::LayerMaskSet {
                            set_id: self.doc.active_set_id,
                            layer_id,
                            value: 0,
                        },
                        BrushMode::Erase => DocumentEvent::LayerImageFill {
                            set_id: self.doc.active_set_id,
                            layer_id,
                            channel: self.channel,
                            value: [0, 0, 0, 0],
                        },
                        _ if self.channel.is_color() => DocumentEvent::LayerImageFill {
                            set_id: self.doc.active_set_id,
                            layer_id,
                            channel: self.channel,
                            value: self.brush.color,
                        },
                        _ => DocumentEvent::LayerImageFill {
                            set_id: self.doc.active_set_id,
                            layer_id,
                            channel: self.channel,
                            value: [
                                self.brush.grayscale,
                                self.brush.grayscale,
                                self.brush.grayscale,
                                255,
                            ],
                        },
                    };
                    self.commit_uniform_recovery_event(event);
                    self.change();
                    if cached_channel_is_current
                        && !matches!(mode, BrushMode::MaskWhite | BrushMode::MaskBlack)
                    {
                        self.paint_dirty = false;
                        if !self.refresh_uniform_channel_cache(self.channel) {
                            self.invalidate_paint_caches();
                        }
                    }
                }
                Ok(Some(if result.changed && result.undoable {
                    "Filled active layer channel or mask".into()
                } else if result.changed {
                    "Filled active layer; this image exceeds the Undo history limit".into()
                } else {
                    "Nothing to fill (already identical)".into()
                }))
            }
            Action::ClearChannel => {
                self.finish_stroke();
                let selected_id = self.doc.layer_tree.selected_node_id;
                match self.doc.layer_tree.find_node(selected_id) {
                    Some(LayerNode::Paint(layer)) => {
                        if !layer.pixels.contains_key(&self.channel) {
                            return Ok(Some(format!(
                                "{} is already empty on the selected layer",
                                self.channel.label()
                            )));
                        }
                        let cached_channel_is_current =
                            !self.paint_dirty && self.material.is_some();
                        let mut brush = self.brush.clone();
                        brush.mode = BrushMode::Erase;
                        let result = ops::fill_selected_with_history(
                            &mut self.doc,
                            &mut self.history,
                            self.channel,
                            &brush,
                        )?;
                        if result.changed {
                            self.commit_uniform_recovery_event(DocumentEvent::LayerImageFill {
                                set_id: self.doc.active_set_id,
                                layer_id: selected_id,
                                channel: self.channel,
                                value: [0, 0, 0, 0],
                            });
                            self.change();
                            if cached_channel_is_current {
                                self.paint_dirty = false;
                                if !self.refresh_uniform_channel_cache(self.channel) {
                                    self.invalidate_paint_caches();
                                }
                            }
                        }
                        Ok(Some(if result.changed && !result.undoable {
                            format!(
                                "Cleared {}; this image exceeds the Undo history limit",
                                self.channel.label()
                            )
                        } else {
                            format!("Cleared {} on selected Paint layer", self.channel.label())
                        }))
                    }
                    Some(LayerNode::Fill(fill)) => {
                        let before = fill.values.get(&self.channel).copied();
                        if before.is_none() {
                            return Ok(Some(format!(
                                "{} is already empty on the selected Fill layer",
                                self.channel.label()
                            )));
                        }
                        if fill.values.len() <= 1 {
                            return Ok(Some(
                                "A Fill layer must keep at least one channel value".into(),
                            ));
                        }
                        if let Some(LayerNode::Fill(fill)) =
                            self.doc.layer_tree.find_node_mut(selected_id)
                        {
                            fill.values.remove(&self.channel);
                        }
                        self.history
                            .record_fill_value(selected_id, self.channel, before, None);
                        self.commit_uniform_recovery_event(DocumentEvent::LayerTreeFillValueSet {
                            set_id: self.doc.active_set_id,
                            node_id: selected_id,
                            channel: self.channel,
                            value: None,
                        });
                        self.change();
                        Ok(Some(format!(
                            "Cleared {} on selected Fill layer",
                            self.channel.label()
                        )))
                    }
                    Some(LayerNode::Group(_)) => Ok(Some(
                        "Groups have no channel value; edit a child Paint or Fill layer".into(),
                    )),
                    None => Ok(Some("The selected layer no longer exists".into())),
                }
            }
            Action::InvertMask | Action::ResetMask => {
                self.finish_stroke();
                let layer_id = self.doc.layer_tree.selected_node_id;
                let (changed, undoable) = if matches!(action, Action::InvertMask) {
                    let undoable = self.history.prepare_uniform_edit(0);
                    let changed = self
                        .doc
                        .layer_tree
                        .find_node_mut(layer_id)
                        .is_some_and(|node| {
                            let mask = match node {
                                LayerNode::Paint(layer) => &mut layer.mask,
                                LayerNode::Fill(fill) => &mut fill.properties.mask,
                                LayerNode::Group(group) => &mut group.properties.mask,
                            };
                            for pixel in mask.as_mut() {
                                *pixel = 255 - *pixel;
                            }
                            true
                        });
                    if changed {
                        if undoable {
                            self.history.record_mask_invert(layer_id);
                        } else {
                            self.history.clear();
                        }
                        self.commit_uniform_recovery_event(DocumentEvent::LayerMaskInvert {
                            set_id: self.doc.active_set_id,
                            layer_id,
                        });
                    }
                    (changed, undoable)
                } else {
                    let Some(node) = self.doc.layer_tree.find_node(layer_id) else {
                        return Ok(Some("The selected layer no longer exists".into()));
                    };
                    let mask = match node {
                        LayerNode::Paint(layer) => &layer.mask,
                        LayerNode::Fill(fill) => &fill.properties.mask,
                        LayerNode::Group(group) => &group.properties.mask,
                    };
                    let before = mask.clone();
                    if before.pixels().all(|pixel| pixel.0[0] == 255) {
                        return Ok(Some("Selected layer mask is already white".into()));
                    }
                    let undoable = self.history.prepare_uniform_edit(before.len());
                    let mask = match self.doc.layer_tree.find_node_mut(layer_id) {
                        Some(LayerNode::Paint(layer)) => &mut layer.mask,
                        Some(LayerNode::Fill(fill)) => &mut fill.properties.mask,
                        Some(LayerNode::Group(group)) => &mut group.properties.mask,
                        None => return Ok(Some("The selected layer no longer exists".into())),
                    };
                    for pixel in mask.pixels_mut() {
                        pixel.0[0] = 255;
                    }
                    if undoable {
                        self.history.record_uniform_mask(layer_id, 255, before);
                    } else {
                        self.history.clear();
                    }
                    self.commit_uniform_recovery_event(DocumentEvent::LayerMaskSet {
                        set_id: self.doc.active_set_id,
                        layer_id,
                        value: 255,
                    });
                    (true, undoable)
                };
                if changed {
                    self.change();
                }
                Ok(Some(if matches!(action, Action::InvertMask) && undoable {
                    "Inverted selected layer mask".into()
                } else if matches!(action, Action::InvertMask) && changed {
                    "Inverted mask; Undo history is unavailable".into()
                } else if changed && !undoable {
                    "Reset mask; this image exceeds the Undo history limit".into()
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
                Ok(Some("Choose an export profile, then export".into()))
            }
            Action::BakeAmbientOcclusion => {
                self.start_ao_bake()?;
                Ok(Some("Ambient occlusion bake started".into()))
            }
            Action::ExportMaterialId => {
                self.pending_material_id_export_dialog = true;
                Ok(Some("Choose a PNG path for the material-ID bake".into()))
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
                    "Choose an export profile and per-set UV padding, then export".into(),
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
    fn recent_projects_menu(&self, ui: &mut egui::Ui) -> Option<RecentProjectAction> {
        let mut action = None;
        let entries = self
            .recent_projects
            .as_ref()
            .map_or(&[][..], RecentProjects::entries);
        if entries.is_empty() {
            ui.add_enabled(false, egui::Label::new("No recent projects"));
        }
        for (index, path) in entries.iter().enumerate() {
            let exists = path.is_file();
            ui.horizontal(|ui| {
                let name = path
                    .file_name()
                    .unwrap_or(path.as_os_str())
                    .to_string_lossy();
                let label = if exists {
                    format!("{}. {name}", index + 1)
                } else {
                    format!("{}. {name} (missing)", index + 1)
                };
                let response = ui
                    .allocate_ui_with_layout(
                        egui::vec2(260.0, 19.0),
                        egui::Layout::top_down_justified(egui::Align::Min),
                        |ui| {
                            ui.add_enabled(
                                exists
                                    && self.pending_journal_recovery.is_none()
                                    && self.pending_destructive.is_none(),
                                egui::Button::new(label).truncate(),
                            )
                        },
                    )
                    .inner
                    .on_hover_text(path.display().to_string())
                    .on_disabled_hover_text(path.display().to_string());
                #[cfg(test)]
                capture_control_rect(ui, &format!("test-recent-open-{index}"), response.rect);
                if response.clicked() {
                    action = Some(RecentProjectAction::Open(path.clone()));
                    ui.close_menu();
                }
                let remove = ui
                    .small_button("×")
                    .on_hover_text("Remove from recent projects");
                #[cfg(test)]
                capture_control_rect(ui, &format!("test-recent-remove-{index}"), remove.rect);
                if remove.clicked() {
                    action = Some(RecentProjectAction::Remove(path.clone()));
                }
            });
        }
        ui.separator();
        if ui
            .add_enabled(
                !entries.is_empty() || self.recent_projects_error.is_some(),
                egui::Button::new("Clear recent projects"),
            )
            .clicked()
        {
            action = Some(RecentProjectAction::Clear);
        }
        if let Some(error) = self.recent_projects_error.as_ref() {
            ui.label(
                egui::RichText::new("Recent history is unavailable")
                    .small()
                    .weak(),
            )
            .on_hover_text(error);
        }
        action
    }

    fn perform_recent_project_action(&mut self, action: RecentProjectAction) {
        match action {
            RecentProjectAction::Open(path) => {
                let result = if path.is_file() {
                    self.request_open_project(path)
                } else {
                    Err(anyhow::anyhow!(
                        "The recent project is no longer available: {}",
                        path.display()
                    ))
                };
                self.status = match result {
                    Ok(message) => message,
                    Err(error) => format!("Could not open recent project: {error:#}"),
                };
            }
            RecentProjectAction::Remove(path) => {
                if let Some(recent) = self.recent_projects.as_mut() {
                    match recent.remove(&path) {
                        Ok(()) => self.recent_projects_error = None,
                        Err(error) => {
                            self.recent_projects_error =
                                Some(format!("Recent projects could not be saved: {error:#}"))
                        }
                    }
                }
            }
            RecentProjectAction::Clear => {
                if let Some(recent) = self.recent_projects.as_mut() {
                    match recent.clear() {
                        Ok(()) => self.recent_projects_error = None,
                        Err(error) => {
                            self.recent_projects_error =
                                Some(format!("Recent projects could not be saved: {error:#}"))
                        }
                    }
                }
            }
        }
    }

    fn menu(&mut self, ctx: &egui::Context) {
        // Capture keyboard ownership before constructing the menu widgets; the
        // menu itself can request keyboard input for accessibility navigation.
        let text_input_focused = ctx.wants_keyboard_input();
        let mut action = None;
        let mut recent_action = None;
        egui::TopBottomPanel::top("menu").show(ctx, |ui| {
            egui::menu::bar(ui, |ui| {
                let file_menu = ui.menu_button("File", |ui| {
                    if ui.button("New").clicked() {
                        action = Some(Action::New);
                        ui.close_menu();
                    }
                    if ui
                        .add(
                            egui::Button::new("Open .kalium…").shortcut_text(ctx.format_shortcut(
                                &egui::KeyboardShortcut::new(
                                    egui::Modifiers::COMMAND,
                                    egui::Key::O,
                                ),
                            )),
                        )
                        .clicked()
                    {
                        action = Some(Action::Open);
                        ui.close_menu();
                    }
                    let recent_menu = ui.menu_button("Open Recent", |ui| {
                        recent_action = self.recent_projects_menu(ui);
                    });
                    #[cfg(test)]
                    capture_control_rect(ui, "test-file-recent-menu", recent_menu.response.rect);
                    #[cfg(not(test))]
                    let _ = recent_menu;
                    if matches!(
                        recent_action.as_ref(),
                        Some(RecentProjectAction::Open(_) | RecentProjectAction::Clear)
                    ) {
                        ui.close_menu();
                    }
                    ui.separator();
                    for (label, a) in [
                        ("Save", Action::Save),
                        ("Save As…", Action::SaveAs),
                        ("Save recovery checkpoint", Action::SaveCheckpoint),
                        ("Recover global checkpoint…", Action::RecoverCheckpoint),
                        ("Import OBJ / glTF / GLB…", Action::ImportMesh),
                        ("Import PNG / JPEG as layer…", Action::ImportTexture),
                        ("Load brush preset (.kbrush)…", Action::LoadBrush),
                        ("Load alpha brush tip image…", Action::LoadBrushTip),
                        ("Save brush preset (.kbrush)…", Action::SaveBrush),
                        ("Export active set PBR textures…", Action::Export),
                        ("Export all texture sets…", Action::ExportAll),
                        (
                            "Export active Texture Set material IDs…",
                            Action::ExportMaterialId,
                        ),
                        ("Bake AO to editable layer", Action::BakeAmbientOcclusion),
                        ("Clear active channel", Action::ClearChannel),
                        ("Inspect SPP signature (experimental)…", Action::InspectSpp),
                    ] {
                        if ui.button(label).clicked() {
                            action = Some(a);
                            ui.close_menu();
                        }
                    }
                });
                #[cfg(test)]
                capture_control_rect(ui, "test-file-menu", file_menu.response.rect);
                #[cfg(not(test))]
                let _ = file_menu;
                ui.menu_button("Edit", |ui| {
                    if ui.button("Undo").clicked() {
                        self.apply_history(HistoryDirection::Undo);
                        ui.close_menu();
                    }
                    if ui.button("Redo").clicked() {
                        self.apply_history(HistoryDirection::Redo);
                        ui.close_menu();
                    }
                });
                ui.menu_button("Mode", |ui| {
                    for (label, mode) in [
                        ("Paint", BrushMode::Paint),
                        ("Erase", BrushMode::Erase),
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
                            ui.close_menu();
                        }
                    }
                });
                ui.menu_button("Window", |ui| {
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
                            layout::set_pane_visible(&mut self.dock, pane, visible);
                        }
                    }
                });
                ui.menu_button("Viewport", |ui| {
                    if ui.button("Reset camera").clicked() {
                        self.view.reset_camera();
                        self.model_dirty = true;
                        ui.close_menu();
                    }
                    ui.checkbox(&mut self.gpu_preview_enabled, "Experimental GPU preview");
                    ui.label("GPU preview: Base Color with diffuse lighting");
                });
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
                ui.menu_button("Help", |ui| {
                    ui.label("Kalium · independent texture-painting editor");
                    ui.label("AGPL-3.0-only");
                    ui.label("See README.md and docs/COMPATIBILITY.md");
                });
            });
        });
        if let Some(a) = action {
            self.perform(a);
        }
        if let Some(action) = recent_action {
            self.perform_recent_project_action(action);
        }
        // Modal choices own keyboard input too: egui blocks background pointer
        // interaction, but these explicit shortcut handlers run outside widgets.
        if self.pending_destructive.is_some() || self.pending_journal_recovery.is_some() {
            return;
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
        // History shortcuts belong to the focused editor when a text field is
        // active (for example, Assets search or Texture Set rename).
        if !text_input_focused {
            let (undo, redo) = ctx.input(|i| {
                (
                    i.modifiers.command && i.key_pressed(egui::Key::Z) && !i.modifiers.shift,
                    i.modifiers.command
                        && (i.key_pressed(egui::Key::Y)
                            || (i.modifiers.shift && i.key_pressed(egui::Key::Z))),
                )
            });
            if undo {
                self.apply_history(HistoryDirection::Undo);
            }
            if redo {
                self.apply_history(HistoryDirection::Redo);
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

    fn brush_toolbar(&mut self, ui: &mut egui::Ui) {
        use crate::painter_classic::toolbar_parameter;
        let radius = toolbar_parameter(ui, &mut self.brush.radius, 1..=180, "Size", 1.0);
        #[cfg(test)]
        capture_control_rect(ui, "toolbar-radius", radius.rect);
        let _ = radius;
        let flow = toolbar_parameter(ui, &mut self.brush.opacity, 0.01..=1.0, "Flow", 0.01);
        #[cfg(test)]
        capture_control_rect(ui, "toolbar-flow", flow.rect);
        let _ = flow;
        let hardness = toolbar_parameter(ui, &mut self.brush.hardness, 0.0..=1.0, "Hardness", 0.01);
        #[cfg(test)]
        capture_control_rect(ui, "toolbar-hardness", hardness.rect);
        let _ = hardness;
        let spacing = toolbar_parameter(ui, &mut self.brush.spacing, 0.1..=1.5, "Spacing", 0.01);
        #[cfg(test)]
        capture_control_rect(ui, "toolbar-spacing", spacing.rect);
        let _ = spacing;
        egui::ComboBox::from_id_salt("toolbar-channel")
            .selected_text(self.channel.label())
            .show_ui(ui, |ui| {
                for channel in Channel::ALL {
                    if ui
                        .selectable_value(&mut self.channel, channel, channel.label())
                        .changed()
                    {
                        self.uv_texture = None;
                        if self.solo_channel || self.solo_layer {
                            self.model_dirty = true;
                            self.paint_dirty |= self.solo_layer;
                        }
                    }
                }
            });
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
            ui.horizontal(|ui| {
                ui.label("Output size");
                egui::ComboBox::from_id_salt("export-output-size")
                    .selected_text(self.export_output_size.map_or_else(
                        || "Each working Texture Set size".to_owned(),
                        |size| format!("{size} × {size}"),
                    ))
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut self.export_output_size,
                            None,
                            "Each working Texture Set size",
                        );
                        for size in export::EXPORT_SIZES {
                            ui.selectable_value(
                                &mut self.export_output_size,
                                Some(size),
                                format!("{size} × {size}"),
                            );
                        }
                    });
            });
            ui.add(
                egui::Slider::new(
                    &mut self.export_padding_radius,
                    0..=export::MAX_PADDING_RADIUS,
                )
                .suffix(" texels")
                .text("UV edge padding"),
            );
            egui::ComboBox::from_id_salt("engine-export-profile")
                .selected_text(self.export_profile.label())
                .show_ui(ui, |ui| {
                    for profile in export::EngineExportProfile::ALL {
                        ui.selectable_value(&mut self.export_profile, profile, profile.label());
                    }
                });
            if self.export_profile == export::EngineExportProfile::Custom {
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
            } else {
                let description = match self.export_profile {
                    export::EngineExportProfile::Godot4Orm => {
                        "Godot profile: ORM (R occlusion, G roughness, B metallic), OpenGL +Y normal."
                    }
                    export::EngineExportProfile::UnityMetallic => {
                        "Unity profile: metallic in R, smoothness in A, OpenGL +Y normal."
                    }
                    export::EngineExportProfile::GltfMetallicRoughness => {
                        "glTF profile: packed ORM (G roughness, B metallic; R occlusion), OpenGL +Y. Writes a sidecar, not a glTF model."
                    }
                    export::EngineExportProfile::SubstancePainterPython => {
                        "Painter preset JSON: documented Python API payload for exporting standard PBR maps from Painter. This is not an .spp project or .spexp shelf file."
                    }
                    export::EngineExportProfile::Custom => unreachable!(),
                };
                ui.label(description);
            }
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
                    profile: self.export_profile,
                    output_size: self.export_output_size,
                };
                let export_result = if self.pending_export_all {
                    export::ExportJob::new(&self.doc, &dir, options)
                } else {
                    export::ExportJob::new_active(&self.doc, &dir, options)
                };
                match export_result {
                    Ok(job) => {
                        self.export_job = Some(job);
                        self.export_job_dir = Some(dir);
                        self.export_job_progress = None;
                        self.export_job_all = self.pending_export_all;
                        self.status = "Texture export started".into();
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

    fn advance_export_job(&mut self, ctx: &egui::Context) {
        let result = self.export_job.as_mut().map(|job| job.step(&self.doc));
        let Some(result) = result else {
            return;
        };
        match result {
            Ok(progress) => match progress.state {
                export::ExportJobState::Running => self.export_job_progress = Some(progress),
                export::ExportJobState::Complete => {
                    let dir = self
                        .export_job_dir
                        .take()
                        .unwrap_or_else(|| PathBuf::from("."));
                    self.status = if self.export_job_all {
                        format!(
                            "Exported {} texture maps from {} sets into {}",
                            progress.completed_maps,
                            self.doc.texture_set_count(),
                            dir.display()
                        )
                    } else {
                        format!(
                            "Exported {} texture maps into {}",
                            progress.completed_maps,
                            dir.display()
                        )
                    };
                    let message = self.status.clone();
                    self.record_activity(ActivityLevel::Info, "Export", &message);
                    self.export_job = None;
                    self.export_job_progress = None;
                }
                export::ExportJobState::Cancelled => {
                    let dir = self
                        .export_job_dir
                        .take()
                        .unwrap_or_else(|| PathBuf::from("."));
                    self.status = format!(
                        "Export canceled after {} of {} maps; partial files remain in {}",
                        progress.completed_maps,
                        progress.total_maps,
                        dir.display()
                    );
                    let message = self.status.clone();
                    self.record_activity(ActivityLevel::Warning, "Export", &message);
                    self.export_job = None;
                    self.export_job_progress = None;
                }
            },
            Err(error) => {
                let dir = self
                    .export_job_dir
                    .take()
                    .unwrap_or_else(|| PathBuf::from("."));
                self.status = format!(
                    "Export failed after partial output in {}: {error:#}",
                    dir.display()
                );
                let message = self.status.clone();
                self.record_activity(ActivityLevel::Error, "Export", &message);
                self.export_job = None;
                self.export_job_progress = None;
            }
        }
        if self.export_job.is_some() {
            ctx.request_repaint();
        }
    }

    fn export_job_dialog(&mut self, ctx: &egui::Context) {
        if self.export_job.is_none() {
            return;
        }
        let progress = self.export_job_progress.as_ref();
        let mut cancel = false;
        egui::Modal::new(egui::Id::new("export-job-progress")).show(ctx, |ui| {
            ui.heading(if self.export_job_all {
                "Exporting all texture sets"
            } else {
                "Exporting active texture set"
            });
            if let Some(progress) = progress {
                if let Some(map) = &progress.current {
                    ui.label(format!(
                        "Set {}/{} · {} · map {}/{} · {}",
                        map.set_index,
                        map.set_count,
                        map.set_name,
                        map.map_index,
                        map.map_count,
                        map.map_filename
                    ));
                }
                let fraction = if progress.total_maps == 0 {
                    0.0
                } else {
                    progress.completed_maps as f32 / progress.total_maps as f32
                };
                ui.add(
                    egui::ProgressBar::new(fraction.clamp(0.0, 1.0))
                        .show_percentage()
                        .text(format!(
                            "{} / {} maps",
                            progress.completed_maps, progress.total_maps
                        )),
                );
            } else {
                ui.label("Preparing texture maps…");
            }
            ui.label("Cancel leaves already written maps in the output folder.");
            if ui.button("Cancel export").clicked() {
                cancel = true;
            }
        });
        if cancel {
            if let Some(job) = self.export_job.as_mut() {
                job.cancel();
            }
            ctx.request_repaint();
        }
    }

    fn status_bar(&self, ctx: &egui::Context) {
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
                if let Some(reason) = &self.recovery_unavailable {
                    ui.label(
                        egui::RichText::new(format!("Recovery unavailable · {reason}"))
                            .color(Color32::YELLOW),
                    );
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
    }

    fn record_activity(&mut self, level: ActivityLevel, category: &str, message: &str) {
        let Some(log) = self.activity_log.as_mut() else {
            return;
        };
        if let Err(error) = log.record(level, category, message) {
            self.activity_log_error =
                Some(format!("Activity history could not be saved: {error:#}"));
        } else {
            self.activity_log_error = None;
        }
    }

    fn activity(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("Activity");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Clear").clicked() {
                    if let Some(log) = self.activity_log.as_mut() {
                        match log.clear() {
                            Ok(()) => self.activity_log_error = None,
                            Err(error) => {
                                self.activity_log_error = Some(format!(
                                    "Activity history could not be cleared: {error:#}"
                                ));
                            }
                        }
                    }
                }
            });
        });
        if let Some(error) = &self.activity_log_error {
            ui.colored_label(Color32::YELLOW, error);
        }
        let Some(log) = self.activity_log.as_ref() else {
            ui.label("Activity history is unavailable for this session.");
            return;
        };
        egui::ScrollArea::vertical().show(ui, |ui| {
            for entry in log.entries_newest_first() {
                ui.horizontal_wrapped(|ui| {
                    let color = match entry.level {
                        ActivityLevel::Info => Color32::LIGHT_GRAY,
                        ActivityLevel::Warning => Color32::YELLOW,
                        ActivityLevel::Error => Color32::LIGHT_RED,
                    };
                    ui.colored_label(color, format_activity_timestamp(entry.timestamp_unix_ms));
                    ui.label(egui::RichText::new(&entry.category).strong());
                    ui.label(&entry.message);
                });
                ui.separator();
            }
            if log.entries_newest_first().len() == 0 {
                ui.weak("No recorded activity yet.");
            }
        });
    }

    fn tool_shelf(&mut self, ctx: &egui::Context) {
        if !self.show_tool_shelf {
            return;
        }
        egui::SidePanel::left("kalium-tool-shelf")
            .exact_width(38.0)
            .frame(egui::Frame::NONE.inner_margin(egui::Margin::symmetric(2, 4)))
            .show(ctx, |ui| {
                ui.vertical_centered(|ui| {
                    for (icon, mode, help) in [
                        ("paint", BrushMode::Paint, "Paint (B)"),
                        ("erase", BrushMode::Erase, "Eraser (E)"),
                        ("sample", BrushMode::Sample, "Sample (I)"),
                        ("fill", BrushMode::Bucket, "Connected fill (G)"),
                        ("mask_white", BrushMode::MaskWhite, "Paint mask white"),
                        ("mask_black", BrushMode::MaskBlack, "Paint mask black"),
                    ] {
                        let response = ui
                            .add_sized(
                                [28.0, 28.0],
                                egui::Button::new("").selected(self.brush.mode == mode),
                            )
                            .on_hover_text(help);
                        crate::painter_classic::PainterClassic::layer_icon(
                            ui.painter(),
                            response.rect,
                            icon,
                            if response.hovered() || self.brush.mode == mode {
                                crate::painter_classic::PainterClassic::TEXT
                            } else {
                                crate::painter_classic::PainterClassic::MUTED
                            },
                        );
                        if response.clicked() {
                            self.finish_stroke();
                            self.brush.mode = mode;
                        }
                    }
                });
            });
    }

    fn dock_shortcuts(&mut self, ctx: &egui::Context) {
        use crate::painter_classic::PainterClassic as Theme;
        egui::SidePanel::right("kalium-dock-shortcuts")
            .exact_width(38.0)
            .frame(
                egui::Frame::NONE
                    .fill(Theme::TAB)
                    .inner_margin(egui::Margin::symmetric(4, 4)),
            )
            .show(ctx, |ui| {
                ui.vertical_centered(|ui| {
                    for (pane, glyph, help) in [
                        (Pane::TextureSettings, "settings", "Texture Set Settings"),
                        (Pane::Properties, "material", "Properties"),
                        (Pane::Activity, "history", "Activity"),
                        (Pane::BrushStudio, "paint", "Brush Studio"),
                    ] {
                        let response =
                            ui.add_sized([28.0, 28.0], egui::Button::new("").frame(false));
                        response.widget_info(|| {
                            egui::WidgetInfo::labeled(
                                egui::WidgetType::Button,
                                ui.is_enabled(),
                                help,
                            )
                        });
                        let color = if response.hovered() {
                            Theme::TEXT
                        } else {
                            Theme::MUTED
                        };
                        match glyph {
                            "settings" => {
                                Theme::asset_icon(ui.painter(), response.rect, "filter", color)
                            }
                            "material" => {
                                Theme::asset_icon(ui.painter(), response.rect, "material", color)
                            }
                            _ => Theme::layer_icon(ui.painter(), response.rect, glyph, color),
                        }
                        #[cfg(test)]
                        capture_control_rect(ui, &format!("dock-shortcut-{glyph}"), response.rect);
                        if response.clicked() {
                            layout::focus_pane(&mut self.dock, pane);
                        }
                        response.on_hover_text(help);
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
                            if self.solo_channel || self.solo_layer {
                                self.model_dirty = true;
                                self.paint_dirty |= self.solo_layer;
                            }
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
            match &self.brush.tip {
                crate::paint::BrushTip::Round => {
                    ui.small("Tip: Round");
                }
                crate::paint::BrushTip::Alpha { width, height, .. } => {
                    ui.small(format!("Tip: Alpha mask · {width} × {height}"));
                }
            }
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
            if ui.button("Load alpha tip…").clicked() {self.perform(Action::LoadBrushTip);}
                if ui.button("Save brush…").clicked() {self.perform(Action::SaveBrush);}
            });
            ui.separator();
            ui.heading("Display");
            if ui.checkbox(&mut self.solo_channel,"Show selected channel in 3D").changed(){self.model_dirty=true;}
            let mut lighting = self.view.lighting_settings();
            let mut lighting_changed = false;
            ui.label("CPU reference lighting");
            let environment_name = match lighting.environment {
                EnvironmentPreset::Studio => "Studio",
                EnvironmentPreset::Warm => "Warm studio",
                EnvironmentPreset::Cool => "Cool studio",
            };
            let _response = egui::ComboBox::from_label("Environment")
                .selected_text(environment_name)
                .show_ui(ui, |ui| {
                    for (preset, label) in [
                        (EnvironmentPreset::Studio, "Studio"),
                        (EnvironmentPreset::Warm, "Warm studio"),
                        (EnvironmentPreset::Cool, "Cool studio"),
                    ] {
                        let response = ui.selectable_value(&mut lighting.environment, preset, label);
                        #[cfg(test)]
                        capture_control_rect(
                            ui,
                            match preset {
                                EnvironmentPreset::Studio => "test-lighting-environment-studio",
                                EnvironmentPreset::Warm => "test-lighting-environment-warm",
                                EnvironmentPreset::Cool => "test-lighting-environment-cool",
                            },
                            response.rect,
                        );
                        if response.changed() {
                            lighting_changed = true;
                        }
                    }
                });
            #[cfg(test)]
            capture_control_rect(ui, "test-lighting-environment", _response.response.rect);
            let response = ui.add(
                egui::Slider::new(&mut lighting.key_azimuth_degrees, -180.0..=180.0)
                    .text("Key azimuth (°)"),
            );
            #[cfg(test)]
            capture_control_rect(ui, "test-lighting-azimuth", response.rect);
            lighting_changed |= response.changed();
            let response = ui.add(
                egui::Slider::new(&mut lighting.key_elevation_degrees, -89.0..=89.0)
                    .text("Key elevation (°)"),
            );
            #[cfg(test)]
            capture_control_rect(ui, "test-lighting-elevation", response.rect);
            lighting_changed |= response.changed();
            let response = ui.add(
                egui::Slider::new(&mut lighting.key_intensity, 0.0..=4.0).text("Key intensity"),
            );
            #[cfg(test)]
            capture_control_rect(ui, "test-lighting-intensity", response.rect);
            lighting_changed |= response.changed();
            let response = ui.add(
                egui::Slider::new(&mut lighting.ambient_fill, 0.0..=2.0).text("Ambient fill"),
            );
            #[cfg(test)]
            capture_control_rect(ui, "test-lighting-ambient", response.rect);
            lighting_changed |= response.changed();
            if lighting_changed {
                self.view.set_lighting_settings(lighting);
                self.model_dirty = true;
            }
            ui.small("Analytic CPU preview environments; not production image-based lighting.");
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
        let selected_before = self.doc.layer_tree.selected_node_id;
        let selected_id = selected_before;
        let Some(parent_id) = self.doc.layer_tree.parent_id(selected_id) else {
            self.status = "The selected layer tree node is missing".into();
            return;
        };
        let Some(path) = self.doc.layer_tree.path_to(selected_id) else {
            self.status = "The selected layer tree node is missing".into();
            return;
        };
        let old_index = *path.last().unwrap_or(&0);
        let set_id = self.doc.active_set_id;
        let mut recovery_events = Vec::new();
        let changed = match command {
            LayerCommand::Add | LayerCommand::AddGroup | LayerCommand::AddFill => {
                let id = self.doc.layer_tree.next_node_id;
                let insertion_parent = match self.doc.layer_tree.find_node(selected_id) {
                    Some(LayerNode::Group(_)) => Some(selected_id),
                    _ => parent_id,
                };
                let index = if insertion_parent == Some(selected_id) {
                    self.doc
                        .layer_tree
                        .siblings(insertion_parent)
                        .map(|nodes| nodes.len())
                        .unwrap_or(0)
                } else {
                    old_index + 1
                };
                let node = match command {
                    LayerCommand::Add => {
                        LayerNode::Paint(Layer::new(id, format!("Paint Layer {id}"), self.doc.size))
                    }
                    LayerCommand::AddGroup => LayerNode::Group(crate::layer_tree::LayerGroup {
                        properties: crate::layer_tree::LayerNodeProperties::new(
                            id,
                            format!("Group {id}"),
                            self.doc.size,
                        )
                        .expect("validated document size and generated name"),
                        children: Vec::new(),
                    }),
                    LayerCommand::AddFill => LayerNode::Fill(crate::layer_tree::FillLayer {
                        properties: crate::layer_tree::LayerNodeProperties::new(
                            id,
                            format!("Fill {id}"),
                            self.doc.size,
                        )
                        .expect("validated document size and generated name"),
                        values: std::collections::HashMap::from([(
                            self.channel,
                            default_fill_value(self.channel),
                        )]),
                    }),
                    _ => unreachable!("the surrounding match arm restricts commands"),
                };
                match self
                    .history
                    .insert_subtree(&mut self.doc, insertion_parent, index, node, id)
                {
                    Ok(()) => {
                        if let Some(node) = self.doc.layer_tree.find_node(id) {
                            if let Ok(events) = recovery::prepare_layer_tree_subtree_events(
                                node,
                                set_id,
                                insertion_parent,
                                index as u32,
                                id,
                            ) {
                                recovery_events.extend(events);
                            }
                        }
                        true
                    }
                    Err(err) => {
                        self.status = err.into();
                        false
                    }
                }
            }
            LayerCommand::Duplicate => {
                match self.history.duplicate_subtree(&mut self.doc, selected_id) {
                    Ok(duplicate_id) => {
                        if let Some(node) = self.doc.layer_tree.find_node(duplicate_id) {
                            if let Ok(events) = recovery::prepare_layer_tree_subtree_events(
                                node,
                                set_id,
                                parent_id,
                                old_index.saturating_add(1) as u32,
                                duplicate_id,
                            ) {
                                recovery_events.extend(events);
                            }
                        }
                        true
                    }
                    Err(err) => {
                        self.status = format!("Cannot duplicate this layer: {err}");
                        false
                    }
                }
            }
            LayerCommand::Remove => {
                let siblings = match self.doc.layer_tree.siblings(parent_id) {
                    Ok(siblings) => siblings,
                    Err(error) => {
                        self.status = format!("Cannot delete this layer: {error}");
                        return;
                    }
                };
                if siblings.len() == 1 && parent_id.is_none() {
                    false
                } else {
                    let selected_after = siblings
                        .iter()
                        .find(|node| node.id() != selected_id)
                        .map(LayerNode::id)
                        .or(parent_id);
                    let Some(selected_after) = selected_after else {
                        self.status = "Cannot find a surviving layer selection".into();
                        return;
                    };
                    match self
                        .history
                        .remove_subtree(&mut self.doc, selected_id, selected_after)
                    {
                        Ok(true) => {
                            recovery_events.push(DocumentEvent::LayerTreeNodeRemove {
                                set_id,
                                node_id: selected_id,
                                expected_parent_id: parent_id,
                                expected_index: old_index as u32,
                                selected_after_node_id: selected_after,
                            });
                            true
                        }
                        Ok(false) => false,
                        Err(error) => {
                            self.status = format!("Cannot delete this layer: {error}");
                            false
                        }
                    }
                }
            }
            LayerCommand::Up | LayerCommand::Down => {
                let sibling_count = self
                    .doc
                    .layer_tree
                    .siblings(parent_id)
                    .map(|siblings| siblings.len())
                    .unwrap_or(0);
                let target_index = if command == LayerCommand::Up {
                    old_index + 1
                } else {
                    old_index.saturating_sub(1)
                };
                if (command == LayerCommand::Up && target_index >= sibling_count)
                    || (command == LayerCommand::Down && old_index == 0)
                {
                    false
                } else {
                    match self.history.move_subtree(
                        &mut self.doc,
                        selected_id,
                        parent_id,
                        target_index,
                    ) {
                        Ok(true) => {
                            recovery_events.push(DocumentEvent::LayerTreeNodeMove {
                                set_id,
                                node_id: selected_id,
                                from_parent_id: parent_id,
                                from_index: old_index as u32,
                                to_parent_id: parent_id,
                                to_index: target_index as u32,
                            });
                            true
                        }
                        Ok(false) => false,
                        Err(error) => {
                            self.status = format!("Cannot move this layer: {error}");
                            false
                        }
                    }
                }
            }
        };
        if changed {
            self.change_with_recovery_events(&recovery_events);
        }
    }
    fn move_layer_to(&mut self, from_id: u32, target_id: u32) -> bool {
        if from_id == target_id {
            return false;
        }
        self.finish_stroke();
        let Some(source_parent) = self.doc.layer_tree.parent_id(from_id) else {
            return false;
        };
        let Some(target_node) = self.doc.layer_tree.find_node(target_id) else {
            return false;
        };
        let Some(source_path) = self.doc.layer_tree.path_to(from_id) else {
            return false;
        };
        let Some(target_path) = self.doc.layer_tree.path_to(target_id) else {
            return false;
        };
        let source_index = *source_path.last().unwrap_or(&0);
        let (target_parent, destination) = match target_node {
            LayerNode::Group(group) => (Some(target_id), group.children.len()),
            LayerNode::Paint(_) | LayerNode::Fill(_) => (
                self.doc.layer_tree.parent_id(target_id).unwrap_or(None),
                *target_path.last().unwrap_or(&0),
            ),
        };
        if source_parent == target_parent && source_index == destination {
            return false;
        }
        let set_id = self.doc.active_set_id;
        match self
            .history
            .move_subtree(&mut self.doc, from_id, target_parent, destination)
        {
            Ok(true) => {}
            Ok(false) => return false,
            Err(error) => {
                self.status = format!("Cannot move this layer: {error}");
                return false;
            }
        }
        self.change_with_recovery_events(&[DocumentEvent::LayerTreeNodeMove {
            set_id,
            node_id: from_id,
            from_parent_id: source_parent,
            from_index: source_index as u32,
            to_parent_id: target_parent,
            to_index: destination as u32,
        }]);
        true
    }
    fn select_layer_by_id(&mut self, layer_id: u32) -> bool {
        if self.doc.layer_tree.find_node(layer_id).is_none() {
            return false;
        }
        if self.doc.layer_tree.selected_node_id == layer_id {
            return false;
        }
        self.finish_stroke();
        if self.doc.select_node(layer_id).is_err() {
            return false;
        }
        let event = DocumentEvent::LayerSelection {
            set_id: self.doc.active_set_id,
            layer_id,
        };
        self.change_with_recovery_events(std::slice::from_ref(&event));
        true
    }
    fn resize_document(&mut self, size: u32) {
        if size == self.doc.size {
            return;
        }
        self.finish_stroke();
        match self.doc.try_resize(size) {
            Ok(()) => {
                let event = DocumentEvent::TextureSetResize {
                    set_id: self.doc.active_set_id,
                    size,
                };
                self.rebuild_coverage();
                self.history.clear();
                self.change_with_recovery_events(&[event]);
            }
            Err(err) => self.status = format!("Cannot resize textures: {err}"),
        }
    }
    fn layers(&mut self, ui: &mut egui::Ui) {
        let before_properties: Vec<_> = self
            .doc
            .layer_tree
            .all_node_ids()
            .into_iter()
            .filter_map(|id| {
                let node = self.doc.layer_tree.find_node(id)?;
                Some((
                    id,
                    LayerProperties::capture_node(node),
                    LayerRecoveryProperties::capture_node(node),
                ))
            })
            .collect();
        let ao_sources: Vec<_> = self
            .doc
            .layer_tree
            .ordered_paint_leaf_ids()
            .into_iter()
            .filter_map(|id| {
                let layer = self.doc.paint_layer(id)?;
                layer
                    .pixels
                    .contains_key(&Channel::Occlusion)
                    .then(|| (id, layer.name.clone()))
            })
            .collect();
        let mut changed = false;
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                for (icon, control, help, command) in [
                    ("paint", "Paint", "Add Paint layer", LayerCommand::Add),
                    (
                        "group",
                        "Group",
                        "Add an empty Group",
                        LayerCommand::AddGroup,
                    ),
                    ("fill", "Fill", "Add a Fill layer", LayerCommand::AddFill),
                    (
                        "duplicate",
                        "Duplicate",
                        "Duplicate selected node and its contents",
                        LayerCommand::Duplicate,
                    ),
                    (
                        "delete",
                        "Delete",
                        "Delete selected node",
                        LayerCommand::Remove,
                    ),
                    ("up", "Up", "Move selected node up", LayerCommand::Up),
                    (
                        "down",
                        "Down",
                        "Move selected node down",
                        LayerCommand::Down,
                    ),
                ] {
                    let response = ui
                        .add_sized(egui::vec2(20.0, 19.0), egui::Button::new(""))
                        .on_hover_text(help);
                    crate::painter_classic::PainterClassic::layer_icon(
                        ui.painter(),
                        response.rect,
                        icon,
                        if response.hovered() {
                            crate::painter_classic::PainterClassic::TEXT
                        } else {
                            crate::painter_classic::PainterClassic::MUTED
                        },
                    );
                    #[cfg(test)]
                    if matches!(control, "Group" | "Fill" | "Duplicate") {
                        capture_control_rect(
                            ui,
                            &format!("test-layer-action-{control}"),
                            response.rect,
                        );
                    }
                    #[cfg(not(test))]
                    let _ = control;
                    if response.clicked() {
                        self.layer_command(command);
                    }
                }
                let response = ui
                    .add_sized(egui::vec2(20.0, 19.0), egui::Button::new(""))
                    .on_hover_text("Invert selected mask");
                crate::painter_classic::PainterClassic::layer_icon(
                    ui.painter(),
                    response.rect,
                    "mask",
                    if response.hovered() {
                        crate::painter_classic::PainterClassic::TEXT
                    } else {
                        crate::painter_classic::PainterClassic::MUTED
                    },
                );
                if response.clicked() {
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
                                if self.solo_channel || self.solo_layer {
                                    self.model_dirty = true;
                                    self.paint_dirty |= self.solo_layer;
                                }
                            }
                        }
                    });
            });
            let solo_response = ui.checkbox(&mut self.solo_layer, "Solo selected layer");
            #[cfg(test)]
            capture_control_rect(ui, "test-layer-solo", solo_response.rect);
            if solo_response.changed() {
                self.paint_dirty = true;
                self.model_dirty = true;
                self.model_partial_dirty.clear();
            }
            let mut requested_selection = None;
            let mut requested_group_toggle = None;
            let node_ids: Vec<_> = self
                .doc
                .layer_tree
                .all_node_ids()
                .into_iter()
                .rev()
                .filter(|node_id| {
                    let mut parent_id = self.doc.layer_tree.parent_id(*node_id).flatten();
                    while let Some(ancestor_id) = parent_id {
                        if self.collapsed_layer_groups.contains(&ancestor_id) {
                            return false;
                        }
                        parent_id = self.doc.layer_tree.parent_id(ancestor_id).flatten();
                    }
                    true
                })
                .collect();
            egui::ScrollArea::vertical()
                .max_height(360.0)
                .show(ui, |ui| {
                    for node_id in node_ids {
                        let selected = self.doc.layer_tree.selected_node_id == node_id;
                        let depth = self
                            .doc
                            .layer_tree
                            .path_to(node_id)
                            .map_or(1, |path| path.len());
                        let (
                            type_glyph,
                            title,
                            row_meta,
                            masked,
                            visibility,
                            group_has_children,
                            thumbnail_color,
                            mask_preview,
                        ) = {
                            let Some(node) = self.doc.layer_tree.find_node_mut(node_id) else {
                                continue;
                            };
                            let (
                                kind,
                                name,
                                opacity,
                                blend,
                                channels,
                                masked,
                                visibility,
                                group_has_children,
                                thumbnail_color,
                                mask_preview,
                            ) = match node {
                                LayerNode::Paint(layer) => (
                                    "Paint",
                                    layer.name.clone(),
                                    layer.opacity,
                                    layer.blend,
                                    layer.pixels.contains_key(&self.channel),
                                    !layer.mask.pixels().all(|pixel| pixel.0[0] == 255),
                                    &mut layer.visible,
                                    false,
                                    layer.pixels.get(&self.channel).map(|image| {
                                        let pixel = image
                                            .get_pixel(image.width() / 2, image.height() / 2)
                                            .0;
                                        Color32::from_rgba_unmultiplied(
                                            pixel[0], pixel[1], pixel[2], pixel[3],
                                        )
                                    }),
                                    layer
                                        .mask
                                        .pixels()
                                        .any(|pixel| pixel.0[0] != 255)
                                        .then(|| layer_mask_preview(&layer.mask)),
                                ),
                                LayerNode::Fill(fill) => (
                                    "Fill",
                                    fill.properties.name.clone(),
                                    fill.properties.opacity,
                                    fill.properties.blend,
                                    fill.values.contains_key(&self.channel),
                                    !fill.properties.mask.pixels().all(|pixel| pixel.0[0] == 255),
                                    &mut fill.properties.visible,
                                    false,
                                    fill.values.get(&self.channel).map(|value| {
                                        Color32::from_rgba_unmultiplied(
                                            value[0], value[1], value[2], value[3],
                                        )
                                    }),
                                    fill.properties
                                        .mask
                                        .pixels()
                                        .any(|pixel| pixel.0[0] != 255)
                                        .then(|| layer_mask_preview(&fill.properties.mask)),
                                ),
                                LayerNode::Group(group) => (
                                    "Group",
                                    group.properties.name.clone(),
                                    group.properties.opacity,
                                    group.properties.blend,
                                    false,
                                    !group
                                        .properties
                                        .mask
                                        .pixels()
                                        .all(|pixel| pixel.0[0] == 255),
                                    &mut group.properties.visible,
                                    !group.children.is_empty(),
                                    None,
                                    group
                                        .properties
                                        .mask
                                        .pixels()
                                        .any(|pixel| pixel.0[0] != 255)
                                        .then(|| layer_mask_preview(&group.properties.mask)),
                                ),
                            };
                            let type_glyph = match kind {
                                "Paint" => "paint",
                                "Fill" => "fill",
                                _ => "group",
                            };
                            let blend = match blend {
                                crate::document::BlendMode::Normal => "Norm",
                                crate::document::BlendMode::Multiply => "Mult",
                                crate::document::BlendMode::Add => "Add",
                                crate::document::BlendMode::Detail => "Detail",
                            };
                            let row_meta = format!("{blend}\n{:.0}", opacity * 100.0);
                            let thumbnail_color = thumbnail_color.filter(|_| channels);
                            (
                                type_glyph,
                                name,
                                row_meta,
                                masked,
                                visibility,
                                group_has_children,
                                thumbnail_color,
                                mask_preview,
                            )
                        };
                        let group_collapsed = self.collapsed_layer_groups.contains(&node_id);
                        let mut selection_rect = None;
                        use crate::painter_classic::PainterClassic as Theme;
                        let row_frame = egui::Frame::new()
                            .fill(if selected {
                                Theme::SELECTED_SURFACE
                            } else {
                                Color32::TRANSPARENT
                            })
                            .stroke(if selected {
                                egui::Stroke::new(1.0_f32, Theme::SELECTED_EDGE)
                            } else {
                                egui::Stroke::NONE
                            })
                            .inner_margin(egui::Margin::symmetric(2, 1))
                            .show(ui, |ui| {
                                ui.set_min_width(ui.available_width());
                                ui.set_min_height(38.0);
                                ui.horizontal(|ui| {
                                    ui.add_space(((depth - 1) * 12) as f32);
                                    if group_has_children {
                                        let disclosure = ui
                                            .small_button(if group_collapsed {
                                                "▶"
                                            } else {
                                                "▼"
                                            })
                                            .on_hover_text(if group_collapsed {
                                                "Expand Group"
                                            } else {
                                                "Collapse Group"
                                            });
                                        #[cfg(test)]
                                        capture_control_rect(
                                            ui,
                                            &format!("test-layer-group-toggle-{node_id}"),
                                            disclosure.rect,
                                        );
                                        if disclosure.clicked() {
                                            requested_group_toggle = Some(node_id);
                                        }
                                    } else {
                                        ui.add_space(16.0);
                                    }
                                    ui.dnd_drag_source(
                                        egui::Id::new(("layer-node-drag", node_id)),
                                        node_id,
                                        |ui| {
                                            ui.horizontal(|ui| {
                                                let visibility_button = ui
                                                    .add_sized(
                                                        egui::vec2(17.0, 18.0),
                                                        egui::Button::new("").frame(false),
                                                    )
                                                    .on_hover_text(if *visibility {
                                                        "Hide layer"
                                                    } else {
                                                        "Show layer"
                                                    });
                                                crate::painter_classic::PainterClassic::layer_icon(
                                                    ui.painter(),
                                                    visibility_button.rect,
                                                    if *visibility { "eye" } else { "eye_off" },
                                                    if *visibility {
                                                        Theme::MUTED
                                                    } else {
                                                        Theme::BORDER
                                                    },
                                                );
                                                if visibility_button.clicked() {
                                                    *visibility = !*visibility;
                                                    changed = true;
                                                }
                                                let (thumbnail, _) = ui.allocate_exact_size(
                                                    egui::vec2(28.0, 28.0),
                                                    egui::Sense::hover(),
                                                );
                                                let thumb_painter = ui.painter();
                                                thumb_painter.rect_filled(
                                                    thumbnail,
                                                    egui::CornerRadius::ZERO,
                                                    Theme::INPUT,
                                                );
                                                if let Some(color) = thumbnail_color {
                                                    thumb_painter.rect_filled(
                                                        thumbnail.shrink(2.0),
                                                        egui::CornerRadius::ZERO,
                                                        color,
                                                    );
                                                } else {
                                                    let cell_size = thumbnail.width() / 4.0;
                                                    for y in 0..4 {
                                                        for x in 0..4 {
                                                            thumb_painter.rect_filled(
                                                                egui::Rect::from_min_size(
                                                                    thumbnail.min
                                                                        + egui::vec2(
                                                                            x as f32 * cell_size,
                                                                            y as f32 * cell_size,
                                                                        ),
                                                                    egui::Vec2::splat(cell_size),
                                                                ),
                                                                egui::CornerRadius::ZERO,
                                                                if (x + y) % 2 == 0 {
                                                                    Theme::BORDER
                                                                } else {
                                                                    Theme::MUTED
                                                                },
                                                            );
                                                        }
                                                    }
                                                }
                                                if let Some(mask) = mask_preview {
                                                    let (mask_rect, _) = ui.allocate_exact_size(
                                                        egui::vec2(26.0, 28.0),
                                                        egui::Sense::hover(),
                                                    );
                                                    let mask_painter = ui.painter();
                                                    mask_painter.rect_filled(
                                                        mask_rect,
                                                        egui::CornerRadius::ZERO,
                                                        Theme::INPUT,
                                                    );
                                                    for (index, value) in mask.iter().enumerate() {
                                                        let x = index % 4;
                                                        let y = index / 4;
                                                        let cell = egui::Rect::from_min_size(
                                                            mask_rect.min
                                                                + egui::vec2(
                                                                    1.0 + x as f32 * 6.0,
                                                                    1.0 + y as f32 * 6.0,
                                                                ),
                                                            egui::vec2(6.0, 6.0),
                                                        );
                                                        mask_painter.rect_filled(
                                                            cell,
                                                            egui::CornerRadius::ZERO,
                                                            Color32::from_gray(*value),
                                                        );
                                                    }
                                                    mask_painter.rect_filled(
                                                        egui::Rect::from_min_size(
                                                            egui::pos2(
                                                                mask_rect.left(),
                                                                mask_rect.bottom() - 2.0,
                                                            ),
                                                            egui::vec2(mask_rect.width(), 2.0),
                                                        ),
                                                        egui::CornerRadius::ZERO,
                                                        Color32::from_rgb(184, 105, 55),
                                                    );
                                                }
                                                let (type_rect, _) = ui.allocate_exact_size(
                                                    egui::vec2(16.0, 16.0),
                                                    egui::Sense::hover(),
                                                );
                                                crate::painter_classic::PainterClassic::layer_icon(
                                                    ui.painter(),
                                                    type_rect,
                                                    type_glyph,
                                                    crate::painter_classic::PainterClassic::MUTED,
                                                );
                                                let selection = ui.selectable_label(false, &title);
                                                selection_rect = Some(selection.rect);
                                                #[cfg(test)]
                                                capture_control_rect(
                                                    ui,
                                                    &format!("test-layer-row-select-{node_id}"),
                                                    selection.rect,
                                                );
                                                if masked {
                                                    ui.label(egui::RichText::new("◐").small())
                                                        .on_hover_text("Masked node");
                                                }
                                                ui.with_layout(
                                                    egui::Layout::right_to_left(
                                                        egui::Align::Center,
                                                    ),
                                                    |ui| {
                                                        ui.label(
                                                            egui::RichText::new(&row_meta)
                                                                .small()
                                                                .weak(),
                                                        );
                                                    },
                                                );
                                            })
                                        },
                                    )
                                })
                            });
                        let row = row_frame.inner.response;
                        #[cfg(test)]
                        capture_control_rect(
                            ui,
                            &format!("test-layer-row-drag-{node_id}"),
                            row.rect,
                        );
                        let drop_target = ui.interact(
                            row.rect,
                            egui::Id::new(("layer-node-drop", node_id)),
                            egui::Sense::hover(),
                        );
                        let dropped_payload = drop_target.dnd_release_payload::<u32>();
                        if let Some(from_id) = dropped_payload {
                            self.move_layer_to(*from_id, node_id);
                        } else {
                            if row.clicked() {
                                requested_selection = Some(node_id);
                            }
                            let released_on_selection = ui.input(|input| {
                                input.pointer.button_released(egui::PointerButton::Primary)
                                    && input.pointer.interact_pos().is_some_and(|position| {
                                        selection_rect.is_some_and(|rect| rect.contains(position))
                                    })
                            });
                            if released_on_selection {
                                requested_selection = Some(node_id);
                            }
                        }
                        if drop_target.dnd_hover_payload::<u32>().is_some() {
                            ui.painter().hline(
                                drop_target.rect.x_range(),
                                drop_target.rect.bottom(),
                                ui.visuals().widgets.active.bg_stroke,
                            );
                        }
                    }
                });
            if let Some(group_id) = requested_group_toggle {
                if !self.collapsed_layer_groups.remove(&group_id) {
                    self.collapsed_layer_groups.insert(group_id);
                }
            }
            if let Some(node_id) = requested_selection {
                self.select_layer_by_id(node_id);
            }
            let selected_id = self.doc.layer_tree.selected_node_id;
            let fill_value_before = match self.doc.layer_tree.find_node(selected_id) {
                Some(LayerNode::Fill(fill)) => fill.values.get(&self.channel).copied(),
                _ => None,
            };
            let fill_value_count = match self.doc.layer_tree.find_node(selected_id) {
                Some(LayerNode::Fill(fill)) => fill.values.len(),
                _ => 0,
            };
            let mut fill_value = fill_value_before;
            let (kind, mut name, mut opacity, mut blend, mut disabled, mut graph, selected_has_ao) =
                match self.doc.layer_tree.find_node(selected_id) {
                    Some(LayerNode::Paint(layer)) => (
                        "Paint",
                        layer.name.clone(),
                        layer.opacity,
                        layer.blend,
                        layer.disabled_channels.clone(),
                        layer.procedural_graph.clone(),
                        layer.pixels.contains_key(&Channel::Occlusion),
                    ),
                    Some(LayerNode::Fill(fill)) => (
                        "Fill",
                        fill.properties.name.clone(),
                        fill.properties.opacity,
                        fill.properties.blend,
                        fill.properties.disabled_channels.clone(),
                        None,
                        false,
                    ),
                    Some(LayerNode::Group(group)) => (
                        "Group",
                        group.properties.name.clone(),
                        group.properties.opacity,
                        group.properties.blend,
                        group.properties.disabled_channels.clone(),
                        None,
                        false,
                    ),
                    None => return,
                };
            let mut invert_mask = false;
            let mut reset_mask = false;
            let mut clear_channel = false;
            {
                ui.separator();
                ui.label(format!("Selected {kind}"));
                changed |= ui.text_edit_singleline(&mut name).changed();
                let opacity_slider =
                    ui.add(egui::Slider::new(&mut opacity, 0.0..=1.0).text("Opacity"));
                #[cfg(test)]
                capture_control_rect(ui, "test-layer-opacity-slider", opacity_slider.rect);
                changed |= opacity_slider.changed();
                egui::ComboBox::from_id_salt(("blend_mode", selected_id))
                    .selected_text(blend.label())
                    .show_ui(ui, |ui| {
                        for mode in BlendMode::ALL {
                            changed |= ui
                                .selectable_value(&mut blend, mode, mode.label())
                                .changed();
                        }
                    });
                if kind == "Fill" {
                    if fill_value.is_none() {
                        let add_fill_value =
                            ui.button(format!("Add {} fill value", self.channel.label()));
                        #[cfg(test)]
                        capture_control_rect(ui, "test-layer-add-fill-value", add_fill_value.rect);
                        if add_fill_value.clicked() {
                            fill_value = Some(default_fill_value(self.channel));
                            changed = true;
                        }
                    }
                    if fill_value.is_some() && fill_value_count > 1 {
                        let remove_fill_value =
                            ui.button(format!("Remove {} fill value", self.channel.label()));
                        #[cfg(test)]
                        capture_control_rect(
                            ui,
                            "test-layer-remove-fill-value",
                            remove_fill_value.rect,
                        );
                        if remove_fill_value.clicked() && fill_value.take().is_some() {
                            changed = true;
                        }
                    }
                }
                if let Some(value) = fill_value.as_mut() {
                    ui.label(format!("{} fill value", self.channel.label()));
                    for (index, label) in ["R", "G", "B", "A"].into_iter().enumerate() {
                        let slider =
                            ui.add(egui::Slider::new(&mut value[index], 0..=255).text(label));
                        #[cfg(test)]
                        if index == 0 {
                            capture_control_rect(ui, "test-layer-fill-red-slider", slider.rect);
                        }
                        changed |= slider.changed();
                    }
                }
                if kind == "Paint" {
                    ui.separator();
                    ui.label("Procedural roughness");
                    let mut remove_graph = false;
                    if graph.is_none() {
                        if ui.button("Add seeded noise graph").clicked() {
                            graph = Some(crate::procedural::ProceduralGraph::value_noise(0, 8.0));
                            changed = true;
                        }
                        if selected_has_ao {
                            let ao_graph_button =
                                ui.button("Create AO-driven roughness from this layer");
                            #[cfg(test)]
                            capture_control_rect(
                                ui,
                                "test-layer-create-ao-roughness",
                                ao_graph_button.rect,
                            );
                            if ao_graph_button.clicked() {
                                graph = Some(
                                    crate::procedural::ProceduralGraph::value_noise_times_occlusion(
                                        0,
                                        8.0,
                                        selected_id,
                                    ),
                                );
                                changed = true;
                            }
                        }
                    } else if let Some(graph_ref) = graph.as_mut() {
                        let noise_parameters =
                            graph_ref
                                .nodes
                                .iter_mut()
                                .find_map(|node| match &mut node.kind {
                                    crate::procedural::NodeKind::ValueNoise { seed, scale } => {
                                        Some((seed, scale))
                                    }
                                    _ => None,
                                });
                        if let Some((seed, scale)) = noise_parameters {
                            changed |= ui
                                .add(egui::Slider::new(seed, 0..=9999).text("Seed"))
                                .changed();
                            changed |= ui
                                .add(egui::Slider::new(scale, 0.01..=64.0).text("Scale"))
                                .changed();
                        }
                        if let Some(source_id) = graph_ref.occlusion_source_layer_id() {
                            let source_name = ao_sources
                                .iter()
                                .find(|(id, _)| *id == source_id)
                                .map(|(_, name)| name.as_str())
                                .unwrap_or("Missing AO source");
                            let source_label = format!("{source_name} (layer {source_id})");
                            let source_combo =
                                egui::ComboBox::from_id_salt(("ao_source_layer", selected_id))
                                    .selected_text(source_label)
                                    .show_ui(ui, |ui| {
                                        for (candidate_id, candidate_name) in &ao_sources {
                                            let response = ui.selectable_label(
                                                *candidate_id == source_id,
                                                format!("{candidate_name} (layer {candidate_id})"),
                                            );
                                            #[cfg(test)]
                                            capture_control_rect(
                                                ui,
                                                &format!(
                                                    "test-layer-ao-source-option-{candidate_id}"
                                                ),
                                                response.rect,
                                            );
                                            if response.clicked() {
                                                changed |= graph_ref.set_occlusion_source_layer_id(
                                                    Some(*candidate_id),
                                                );
                                            }
                                        }
                                        ui.separator();
                                        let remove_input = ui.button("Remove AO input");
                                        #[cfg(test)]
                                        capture_control_rect(
                                            ui,
                                            "test-layer-ao-source-remove",
                                            remove_input.rect,
                                        );
                                        if remove_input.clicked() {
                                            changed |=
                                                graph_ref.set_occlusion_source_layer_id(None);
                                        }
                                    });
                            #[cfg(test)]
                            capture_control_rect(
                                ui,
                                "test-layer-ao-source-combo",
                                source_combo.response.rect,
                            );
                            #[cfg(not(test))]
                            let _ = source_combo;
                        } else if selected_has_ao
                            && ui.button("Use this layer's Occlusion").clicked()
                        {
                            let noise = graph_ref.nodes.iter().find_map(|node| match node.kind {
                                crate::procedural::NodeKind::ValueNoise { seed, scale } => {
                                    Some((seed, scale))
                                }
                                _ => None,
                            });
                            if let Some((seed, scale)) = noise {
                                *graph_ref =
                                    crate::procedural::ProceduralGraph::value_noise_times_occlusion(
                                        seed,
                                        scale,
                                        selected_id,
                                    );
                                changed = true;
                            }
                        }
                        if ui.button("Remove procedural graph").clicked() {
                            remove_graph = true;
                            changed = true;
                        }
                    }
                    if remove_graph {
                        graph = None;
                    }
                }
                ui.separator();
                ui.label(if kind == "Paint" {
                    "Paint channels"
                } else {
                    "Enabled channels"
                });
                ui.horizontal_wrapped(|ui| {
                    for channel in Channel::ALL {
                        let mut enabled = !disabled.contains(&channel);
                        let channel_present = match self.doc.layer_tree.find_node(selected_id) {
                            Some(LayerNode::Paint(layer)) => {
                                Some(layer.pixels.contains_key(&channel))
                            }
                            Some(LayerNode::Fill(fill)) => Some(fill.values.contains_key(&channel)),
                            Some(LayerNode::Group(_)) | None => None,
                        };
                        let label = channel_present.map_or_else(
                            || painter_channel_label(channel).to_owned(),
                            |present| {
                                format!(
                                    "{}{}",
                                    if present { "● " } else { "○ " },
                                    painter_channel_label(channel)
                                )
                            },
                        );
                        let checkbox = ui.checkbox(&mut enabled, label);
                        #[cfg(test)]
                        capture_control_rect(
                            ui,
                            &format!("test-layer-channel-{selected_id}-{}", channel.label()),
                            checkbox.rect,
                        );
                        if checkbox
                            .on_hover_text(format!(
                                "Enable {} on this {kind} layer",
                                channel.label()
                            ))
                            .changed()
                        {
                            if enabled {
                                disabled.remove(&channel);
                            } else {
                                disabled.insert(channel);
                            }
                            changed = true;
                        }
                    }
                });
                ui.horizontal_wrapped(|ui| {
                    if ui.small_button("Invert mask").clicked() {
                        invert_mask = true;
                    }
                    if ui.small_button("Reset mask").clicked() {
                        reset_mask = true;
                    }
                    if ui.small_button("Clear channel").clicked() {
                        clear_channel = true;
                    }
                });
            }
            if let Some(node) = self.doc.layer_tree.find_node_mut(selected_id) {
                match node {
                    LayerNode::Paint(layer) => {
                        layer.name = name;
                        layer.opacity = opacity;
                        layer.blend = blend;
                        layer.disabled_channels = disabled;
                        layer.procedural_graph = graph;
                    }
                    LayerNode::Fill(fill) => {
                        fill.properties.name = name;
                        fill.properties.opacity = opacity;
                        fill.properties.blend = blend;
                        fill.properties.disabled_channels = disabled;
                        match fill_value {
                            Some(value) => {
                                fill.values.insert(self.channel, value);
                            }
                            None => {
                                fill.values.remove(&self.channel);
                            }
                        }
                    }
                    LayerNode::Group(group) => {
                        group.properties.name = name;
                        group.properties.opacity = opacity;
                        group.properties.blend = blend;
                        group.properties.disabled_channels = disabled;
                    }
                }
            }
            if invert_mask {
                self.perform(Action::InvertMask);
            }
            if reset_mask {
                self.perform(Action::ResetMask);
            }
            if clear_channel {
                self.perform(Action::ClearChannel);
            }
            let mut additional_events = Vec::new();
            if fill_value != fill_value_before {
                if let Some(LayerNode::Fill(fill)) = self.doc.layer_tree.find_node(selected_id) {
                    self.history.record_fill_value(
                        selected_id,
                        self.channel,
                        fill_value_before,
                        fill_value,
                    );
                    additional_events.push(DocumentEvent::LayerTreeFillValueSet {
                        set_id: self.doc.active_set_id,
                        node_id: selected_id,
                        channel: self.channel,
                        value: fill.values.get(&self.channel).copied(),
                    });
                    changed = true;
                }
            }
            if changed {
                let set_id = self.doc.active_set_id;
                let mut events = additional_events;
                for (node_id, before, recovery_before) in before_properties {
                    if let Some(node) = self.doc.layer_tree.find_node(node_id) {
                        let after = LayerProperties::capture_node(node);
                        self.history.record_properties(node_id, before, after);
                        if let Some(event) = recovery_before.tree_event_since(set_id, node) {
                            events.push(event);
                        }
                    }
                }
                self.change_with_recovery_events(&events);
            }
        });
    }

    fn assets(&mut self, ui: &mut egui::Ui) {
        const BUILT_IN_PRESETS: [(&str, &[u8]); 2] = [
            (
                "Soft Painter",
                include_bytes!("../examples/Soft-Painter.kbrush"),
            ),
            (
                "Hard Detail",
                include_bytes!("../examples/Hard-Detail.kbrush"),
            ),
        ];
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("ALL LIBRARIES ▼").small());
            if ui.small_button("Refresh").clicked() {
                self.refresh_brush_shelf();
            }
            if ui
                .small_button("+")
                .on_hover_text("Import preset")
                .clicked()
            {
                self.perform(Action::ImportBrushPreset);
            }
        });
        let search = ui.add(
            egui::TextEdit::singleline(&mut self.asset_search).hint_text("Search brush presets…"),
        );
        #[cfg(test)]
        ui.ctx().data_mut(|data| {
            data.insert_temp(egui::Id::new("asset-preset-search"), search.rect);
        });
        #[cfg(not(test))]
        let _ = search;
        ui.horizontal(|ui| {
            use crate::painter_classic::PainterClassic as Theme;
            for (kind, label, enabled) in [
                ("material", "Materials are not yet supported", false),
                ("alpha", "Alpha asset shelf is not yet supported", false),
                ("brush", "Brush preset shelf", true),
                ("texture", "Texture assets are not yet supported", false),
                ("filter", "Filters are not yet supported", false),
                ("generator", "Generators are not yet supported", false),
            ] {
                let response = ui
                    .add_enabled(
                        enabled,
                        egui::Button::new("")
                            .selected(enabled)
                            .min_size(egui::vec2(25.0, 20.0)),
                    )
                    .on_hover_text(label);
                crate::painter_classic::PainterClassic::asset_icon(
                    ui.painter(),
                    response.rect,
                    kind,
                    if enabled { Theme::TEXT } else { Theme::MUTED },
                );
            }
        });
        ui.separator();
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.label(egui::RichText::new("BRUSH PRESETS").small().weak());
            let query = self.asset_search.trim().to_lowercase();
            let mut visible_presets: usize = 0;
            let columns = ((ui.available_width() + 4.0) / 80.0).floor().max(1.0) as usize;
            egui::Grid::new("builtin-brush-asset-grid")
                .num_columns(columns)
                .spacing(egui::vec2(4.0, 4.0))
                .show(ui, |ui| {
                    let mut column = 0;
                    for (bundled_name, bytes) in BUILT_IN_PRESETS.iter() {
                        let (name, brush) = match brush_preset::load_bytes(bytes) {
                            Ok(preset) => preset,
                            Err(error) => {
                                ui.colored_label(
                                    Color32::LIGHT_RED,
                                    format!("Cannot load {bundled_name}: {error}"),
                                );
                                continue;
                            }
                        };
                        if !name.to_lowercase().contains(&query) {
                            continue;
                        }
                        visible_presets += 1;
                        let selected = self.brush.radius == brush.radius
                            && (self.brush.opacity - brush.opacity).abs() < 0.001
                            && self.brush.tip == brush.tip;
                        let tile = brush_asset_tile(ui, &name, &brush, selected);
                        #[cfg(test)]
                        ui.ctx().data_mut(|data| {
                            data.insert_temp(
                                egui::Id::new(("asset-preset-card", name.as_str())),
                                tile.rect,
                            );
                        });
                        if tile.clicked() {
                            self.brush = brush;
                            self.status = format!("Selected brush: {name}");
                        }
                        column += 1;
                        if column == columns {
                            ui.end_row();
                            column = 0;
                        }
                    }
                });
            ui.separator();
            ui.label(egui::RichText::new("LOCAL ASSETS").small().weak());
            if let Some(error) = &self.shelf_error {
                ui.colored_label(
                    Color32::LIGHT_RED,
                    format!("Cannot scan local preset shelf: {error}"),
                );
            } else if self.shelf_skipped_files > 0 {
                ui.colored_label(
                    Color32::YELLOW,
                    format!(
                        "{} invalid or unreadable preset(s) skipped",
                        self.shelf_skipped_files
                    ),
                );
            }
            #[cfg(test)]
            let local_count = self.shelf_presets.len();
            let columns = ((ui.available_width() + 4.0) / 80.0).floor().max(1.0) as usize;
            egui::Grid::new("local-brush-asset-grid")
                .num_columns(columns)
                .spacing(egui::vec2(4.0, 4.0))
                .show(ui, |ui| {
                    let mut column = 0;
                    for preset in self.shelf_presets.iter() {
                        if !preset.name.to_lowercase().contains(&query) {
                            continue;
                        }
                        visible_presets += 1;
                        let selected = self.brush.radius == preset.brush.radius
                            && (self.brush.opacity - preset.brush.opacity).abs() < 0.001
                            && self.brush.tip == preset.brush.tip;
                        let tile = brush_asset_tile(ui, &preset.name, &preset.brush, selected)
                            .on_hover_text(preset.path.display().to_string());
                        #[cfg(test)]
                        ui.ctx().data_mut(|data| {
                            data.insert_temp(
                                egui::Id::new(("asset-preset-card", preset.name.as_str())),
                                tile.rect,
                            );
                        });
                        if tile.clicked() {
                            self.brush = preset.brush.clone();
                            self.status = format!("Selected brush: {}", preset.name);
                        }
                        column += 1;
                        if column == columns {
                            ui.end_row();
                            column = 0;
                        }
                    }
                });
            if visible_presets == 0 {
                ui.label("No brush presets match this search.");
            }
            #[cfg(test)]
            ui.ctx().data_mut(|data| {
                data.insert_temp(egui::Id::new("asset-preset-visible-count"), visible_presets);
            });
            #[cfg(test)]
            ui.ctx().data_mut(|data| {
                data.insert_temp(egui::Id::new("asset-local-preset-count"), local_count);
            });
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
        let mut switch_to = None;
        for (id, name, size, active) in self.doc.texture_set_list() {
            let row =
                ui.allocate_response(egui::vec2(ui.available_width(), 24.0), egui::Sense::click());
            let painter = ui.painter().with_clip_rect(row.rect);
            if active {
                painter.rect_filled(
                    row.rect,
                    egui::CornerRadius::ZERO,
                    crate::painter_classic::PainterClassic::SELECTED_SURFACE,
                );
                painter.rect_stroke(
                    row.rect,
                    egui::CornerRadius::ZERO,
                    egui::Stroke::new(
                        1.0_f32,
                        crate::painter_classic::PainterClassic::SELECTED_EDGE,
                    ),
                    egui::StrokeKind::Inside,
                );
            } else if row.hovered() {
                painter.rect_filled(
                    row.rect,
                    egui::CornerRadius::ZERO,
                    crate::painter_classic::PainterClassic::HOVER,
                );
            }
            painter.text(
                row.rect.left_center() + egui::vec2(5.0, 0.0),
                egui::Align2::LEFT_CENTER,
                if active { "◉" } else { "○" },
                egui::FontId::proportional(11.0),
                crate::painter_classic::PainterClassic::MUTED,
            );
            painter.text(
                row.rect.left_center() + egui::vec2(23.0, 0.0),
                egui::Align2::LEFT_CENTER,
                name,
                egui::FontId::proportional(11.0),
                crate::painter_classic::PainterClassic::TEXT,
            );
            painter.text(
                row.rect.right_center() + egui::vec2(-6.0, 0.0),
                egui::Align2::RIGHT_CENTER,
                format!("{size}×{size}"),
                egui::FontId::proportional(10.0),
                crate::painter_classic::PainterClassic::MUTED,
            );
            #[cfg(test)]
            ui.ctx().data_mut(|data| {
                data.insert_temp(egui::Id::new(("texture-set-list-row-rect", id)), row.rect);
            });
            if row.clicked() && !active {
                switch_to = Some(id);
            }
        }
        if let Some(id) = switch_to {
            self.select_texture_set(id);
        }
        ui.separator();
        ui.horizontal_wrapped(|ui| {
            let add_set = ui.small_button("+").on_hover_text("Add Texture Set");
            #[cfg(test)]
            ui.ctx().data_mut(|data| {
                data.insert_temp(
                    egui::Id::new("texture-set-list-add-button-rect"),
                    add_set.rect,
                );
            });
            if add_set.clicked() {
                self.create_texture_set();
            }
            if ui
                .add_enabled(self.doc.texture_set_count() > 1, egui::Button::new("−"))
                .on_hover_text("Remove active Texture Set")
                .clicked()
            {
                self.remove_active_texture_set();
            }
        });
        ui.horizontal(|ui| {
            #[cfg(test)]
            let active_set_id = self.doc.active_set_id;
            let name_input = ui.add(
                egui::TextEdit::singleline(&mut self.texture_set_name_input).desired_width(140.0),
            );
            #[cfg(test)]
            ui.ctx().data_mut(|data| {
                data.insert_temp(
                    egui::Id::new(("texture-set-list-name-input-rect", active_set_id)),
                    name_input.rect,
                );
            });
            #[cfg(not(test))]
            let _ = name_input;
            let rename = ui.small_button("Rename");
            #[cfg(test)]
            ui.ctx().data_mut(|data| {
                data.insert_temp(
                    egui::Id::new(("texture-set-list-rename-button-rect", active_set_id)),
                    rename.rect,
                );
            });
            if rename.clicked() {
                self.rename_active_texture_set();
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
                let combo = egui::ComboBox::from_id_salt(("mesh-slot", slot))
                    .selected_text(
                        binding
                            .and_then(|id| options.iter().find(|s| s.0 == id).map(|s| s.1.as_str()))
                            .unwrap_or("Active set (unbound)"),
                    )
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut binding, None, "Active set (unbound)");
                        for (id, label, _, _) in &options {
                            let option =
                                ui.selectable_value(&mut binding, Some(*id), label.as_str());
                            #[cfg(test)]
                            ui.ctx().data_mut(|data| {
                                data.insert_temp(
                                    egui::Id::new(("mesh-slot-set-option-rect", slot, *id)),
                                    option.rect,
                                );
                            });
                            #[cfg(not(test))]
                            let _ = option;
                        }
                    });
                #[cfg(not(test))]
                let _ = combo;
                #[cfg(test)]
                ui.ctx().data_mut(|data| {
                    data.insert_temp(
                        egui::Id::new(("mesh-slot-binding-combo-rect", slot)),
                        combo.response.rect,
                    );
                });
            });
            let old = self.doc.material_bindings.get(slot).copied().flatten();
            if binding != old {
                self.set_material_binding(slot, binding);
            }
        }
        ui.separator();
        ui.label(
            egui::RichText::new(
                "CPU reference tools · linked Texture Sets · experimental GPU Base Color preview",
            )
            .weak()
            .small(),
        );
        if ui.small_button("Import mesh…").clicked() {
            self.perform(Action::ImportMesh);
        }
    }

    fn rename_active_texture_set(&mut self) {
        self.finish_stroke();
        let event = DocumentEvent::TextureSetRename {
            set_id: self.doc.active_set_id,
            name: self.texture_set_name_input.clone(),
        };
        match event.apply(&mut self.doc) {
            Ok(()) => {
                self.texture_set_name_input = self.doc.active_set_name.clone();
                self.change_with_recovery_events(&[event]);
            }
            Err(err) => self.status = format!("Cannot rename texture set: {err}"),
        }
    }

    fn create_texture_set(&mut self) {
        self.finish_stroke();
        let name = format!("Texture Set {}", self.doc.next_set_id);
        let event = DocumentEvent::TextureSetCreate {
            set_id: self.doc.next_set_id,
            name: name.clone(),
        };
        match event.apply(&mut self.doc) {
            Ok(()) => {
                self.texture_set_name_input = self.doc.active_set_name.clone();
                self.ao_settings = ao_settings_for_active_set(&self.doc);
                self.history.clear();
                self.rebuild_coverage();
                self.change_with_recovery_events(&[event]);
                self.status = "Created independent texture set".into();
            }
            Err(err) => self.status = format!("Cannot create texture set: {err}"),
        }
    }

    fn remove_active_texture_set(&mut self) {
        self.finish_stroke();
        let event = DocumentEvent::TextureSetRemove {
            set_id: self.doc.active_set_id,
        };
        match event.apply(&mut self.doc) {
            Ok(()) => {
                self.texture_set_name_input = self.doc.active_set_name.clone();
                self.ao_settings = ao_settings_for_active_set(&self.doc);
                self.history.clear();
                self.rebuild_coverage();
                self.change_with_recovery_events(&[event]);
                self.status = "Removed texture set".into();
            }
            Err(err) => self.status = format!("Cannot remove texture set: {err}"),
        }
    }

    fn set_material_binding(&mut self, slot: usize, set_id: Option<u32>) {
        self.finish_stroke();
        let event = DocumentEvent::MaterialBinding { slot, set_id };
        match event.apply(&mut self.doc) {
            Ok(()) => {
                self.rebuild_coverage();
                self.change_with_recovery_events(&[event]);
                self.status = format!("Material slot {slot} binding updated");
            }
            Err(err) => self.status = format!("Cannot bind material: {err}"),
        }
    }

    fn select_texture_set(&mut self, set_id: u32) {
        self.finish_stroke();
        let event = DocumentEvent::TextureSetSelect { set_id };
        match event.apply(&mut self.doc) {
            Ok(()) => {
                self.texture_set_name_input = self.doc.active_set_name.clone();
                self.ao_settings = ao_settings_for_active_set(&self.doc);
                self.history.clear();
                self.rebuild_coverage();
                self.change_with_recovery_events(&[event]);
                self.status = format!("Selected texture set: {}", self.doc.active_set_name);
            }
            Err(err) => self.status = format!("Cannot select texture set: {err}"),
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
        ui.label(egui::RichText::new("AMBIENT OCCLUSION BAKE").small().weak());
        let mut settings = self.ao_settings.clamped_for_size(self.doc.size);
        let mut settings_changed = settings != self.ao_settings;
        let sample_limit = bake::AmbientOcclusionBakeSettings::sample_limit(self.doc.size);
        let response =
            ui.add(egui::Slider::new(&mut settings.sample_count, 1..=sample_limit).text("Samples"));
        #[cfg(test)]
        capture_control_rect(ui, "test-ao-samples", response.rect);
        settings_changed |= response.changed();
        let response = ui.add(
            egui::Slider::new(&mut settings.max_ray_distance, 0.01..=100.0).text("Ray distance"),
        );
        #[cfg(test)]
        capture_control_rect(ui, "test-ao-ray-distance", response.rect);
        settings_changed |= response.changed();
        let response = ui.add(egui::Slider::new(&mut settings.seed, 0..=9999).text("Seed"));
        #[cfg(test)]
        capture_control_rect(ui, "test-ao-seed", response.rect);
        settings_changed |= response.changed();
        if settings_changed {
            self.update_ao_settings(settings);
        }
        if ui.button("Bake AO to editable layer").clicked() {
            self.perform(Action::BakeAmbientOcclusion);
        }
        let response = ui.button("Export material-ID map…");
        #[cfg(test)]
        capture_control_rect(ui, "test-material-id-export", response.rect);
        if response.clicked() {
            self.perform(Action::ExportMaterialId);
        }
        ui.label(
            egui::RichText::new(
                "Settings are saved per Texture Set and restored with the project journal.",
            )
            .small()
            .weak(),
        );
        ui.separator();
        ui.label(
            egui::RichText::new(
                "Mesh material bindings are available in Texture Set List. UDIM is pending.",
            )
            .weak()
            .small(),
        );
    }

    fn properties(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let preview_width = ((ui.available_width() - 4.0) * 0.5).max(68.0);
            let preview_height = (preview_width - 8.0).clamp(82.0, 148.0);
            let (stroke_rect, _) = ui.allocate_exact_size(
                egui::vec2(preview_width, preview_height),
                egui::Sense::hover(),
            );
            let stroke_painter = ui.painter();
            stroke_painter.rect_filled(
                stroke_rect,
                egui::CornerRadius::ZERO,
                crate::painter_classic::PainterClassic::INPUT,
            );
            stroke_painter.rect_stroke(
                stroke_rect,
                egui::CornerRadius::ZERO,
                egui::Stroke::new(1.0_f32, crate::painter_classic::PainterClassic::HAIRLINE),
                egui::StrokeKind::Inside,
            );
            let stroke_color = Color32::from_white_alpha(
                (self.brush.opacity.clamp(0.0, 1.0) * 230.0).round() as u8,
            );
            let point = |x: f32, y: f32| {
                egui::pos2(
                    stroke_rect.left() + x * stroke_rect.width(),
                    stroke_rect.top() + y * stroke_rect.height(),
                )
            };
            let stroke = egui::Stroke::new((self.brush.radius as f32).sqrt() * 1.5, stroke_color);
            for points in [
                [
                    point(0.08, 0.72),
                    point(0.30, -0.05),
                    point(0.30, 0.95),
                    point(0.52, 0.68),
                ],
                [
                    point(0.52, 0.68),
                    point(0.70, 0.00),
                    point(0.72, 0.15),
                    point(0.92, 0.34),
                ],
            ] {
                stroke_painter.add(egui::epaint::CubicBezierShape::from_points_stroke(
                    points,
                    false,
                    Color32::TRANSPARENT,
                    stroke,
                ));
            }

            let (sphere_rect, _) = ui.allocate_exact_size(
                egui::vec2(preview_width, preview_height),
                egui::Sense::hover(),
            );
            let sphere_painter = ui.painter();
            sphere_painter.rect_filled(
                sphere_rect,
                egui::CornerRadius::ZERO,
                crate::painter_classic::PainterClassic::INPUT,
            );
            let source = self.brush.color;
            crate::painter_classic::PainterClassic::material_sphere(
                sphere_painter,
                sphere_rect.center(),
                preview_height * 0.43,
                Color32::from_rgb(source[0], source[1], source[2]),
            );
        });
        ui.separator();
        ui.label(egui::RichText::new("MATERIAL CHANNEL").small().weak());
        egui::ComboBox::from_id_salt("properties_channel")
            .selected_text(self.channel.label())
            .show_ui(ui, |ui| {
                for channel in Channel::ALL {
                    if ui
                        .selectable_value(&mut self.channel, channel, channel.label())
                        .changed()
                    {
                        self.uv_texture = None;
                        if self.solo_channel || self.solo_layer {
                            self.model_dirty = true;
                            self.paint_dirty |= self.solo_layer;
                        }
                    }
                }
            });
        ui.horizontal_wrapped(|ui| {
            for channel in Channel::ALL {
                if ui
                    .small_button(painter_channel_label(channel))
                    .on_hover_text(channel.label())
                    .clicked()
                {
                    self.channel = channel;
                    self.uv_texture = None;
                    if self.solo_channel || self.solo_layer {
                        self.model_dirty = true;
                        self.paint_dirty |= self.solo_layer;
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
        self.persist_workspace_now();
    }

    fn persist_workspace_now(&mut self) {
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

        let persisted = if let Some(path) = self.workspace_path.as_deref() {
            layout::persist_workspace_at(path, &self.dock)
        } else {
            Ok(encoded.clone())
        };
        match persisted {
            Ok(_) => {
                self.saved_layout = encoded;
            }
            Err(err) => {
                self.status = format!("Workspace was not saved: {err}");
            }
        }
    }

    fn approve_close(&mut self, ctx: &egui::Context) {
        self.persist_workspace_now();
        self.allow_close = true;
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
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
        self.previous_model_hit = None;
        self.previous_model_position = None;
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
                    self.disable_recovery_journal(
                        "committed stroke could not be encoded as recovery patches",
                    );
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
        let mode = self.brush.mode;
        let bytes_per_pixel = if mode.is_mask() { 1 } else { 4 };
        if !History::snapshot_upper_bound_fits(self.doc.size, bytes_per_pixel) {
            self.status =
                "Flood fill canceled: its full-image undo snapshot exceeds the history budget"
                    .into();
            return;
        }
        let Some(selected_layer) = self.doc.selected_paint_layer() else {
            self.status = "Select a Paint layer before using Flood Fill".into();
            return;
        };
        let channel_was_present =
            mode.is_mask() || selected_layer.pixels.contains_key(&self.channel);
        let before = History::capture(&self.doc, self.channel, mode);
        let layer_id = selected_layer.id;
        let coverage = if self.uv_clipping {
            self.uv_coverage.as_ref()
        } else {
            None
        };
        match ops::flood_fill(&mut self.doc, self.channel, uv, &self.brush, coverage) {
            Ok(true) => {
                let after = History::capture(&self.doc, self.channel, mode);
                self.commit_image_snapshot(Snapshot {
                    layer_id,
                    channel: self.channel,
                    mask: mode.is_mask(),
                    channel_was_present,
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
        if self.brush.mode.is_mask() {
            if self
                .doc
                .layer_tree
                .find_node(self.doc.layer_tree.selected_node_id)
                .is_none()
            {
                return;
            }
        } else if self.doc.selected_paint_layer().is_none() {
            self.status = "Select a Paint layer before painting pixels".into();
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
        let stroke_within_budget = self.brush.stroke_masked_recording(
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
        if !stroke_within_budget {
            self.status = "Stroke stopped: texture memory or undo capture budget reached".into();
        }
        self.previous_uv = Some(uv);
        if self.brush.mode.is_mask() {
            self.invalidate_paint_caches();
        } else {
            self.update_painted_tiles(self.channel, &touched_tiles);
        }
    }

    fn mesh_hit_position(&self, hit: PickHit) -> Option<[f32; 3]> {
        let start = hit.triangle_index.checked_mul(3)?;
        let triangle = self.doc.mesh.indices.get(start..start.checked_add(3)?)?;
        let mut position = [0.0; 3];
        for (corner, vertex_index) in triangle.iter().enumerate() {
            let vertex = self.doc.mesh.vertices.get(*vertex_index as usize)?;
            for (axis, value) in position.iter_mut().enumerate() {
                *value += vertex.position[axis] * hit.barycentric[corner];
            }
        }
        position
            .iter()
            .all(|value| value.is_finite())
            .then_some(position)
    }

    /// Continue a model-space stroke through source triangles without ever
    /// interpolating directly between UVs on opposite sides of a seam.
    fn paint_surface_hit(&mut self, hit: PickHit) {
        if let Some(previous) = self.previous_model_hit {
            if previous.triangle_index != hit.triangle_index {
                let seam_uvs = self
                    .doc
                    .mesh
                    .shared_triangle_edge(previous.triangle_index, hit.triangle_index)
                    .and_then(|edge| {
                        let previous_position = self.mesh_hit_position(previous)?;
                        let current_position = self.mesh_hit_position(hit)?;
                        let mut middle = [0.0; 3];
                        let mut edge_vector = [0.0; 3];
                        let mut offset = [0.0; 3];
                        for axis in 0..3 {
                            middle[axis] = (previous_position[axis] + current_position[axis]) * 0.5;
                            edge_vector[axis] = edge.positions[1][axis] - edge.positions[0][axis];
                            offset[axis] = middle[axis] - edge.positions[0][axis];
                        }
                        let length_squared =
                            edge_vector.iter().map(|value| value * value).sum::<f32>();
                        if !length_squared.is_finite() || length_squared <= f32::EPSILON {
                            return None;
                        }
                        let amount = (offset
                            .iter()
                            .zip(edge_vector)
                            .map(|(offset, edge)| offset * edge)
                            .sum::<f32>()
                            / length_squared)
                            .clamp(0.0, 1.0);
                        let interpolate = |uvs: [[f32; 2]; 2]| {
                            [
                                uvs[0][0] + (uvs[1][0] - uvs[0][0]) * amount,
                                uvs[0][1] + (uvs[1][1] - uvs[0][1]) * amount,
                            ]
                        };
                        Some((interpolate(edge.first_uv), interpolate(edge.second_uv)))
                    });

                if let Some((previous_chart_edge, current_chart_edge)) = seam_uvs {
                    self.paint_uv(previous_chart_edge);
                    self.previous_uv = None;
                    self.paint_uv(current_chart_edge);
                } else {
                    // Disconnected or ambiguous geometry must split the stroke.
                    self.previous_uv = None;
                }
            }
        }
        self.paint_uv(hit.uv);
        self.previous_model_hit = Some(hit);
    }

    /// Resample a sparse pointer segment through the reference renderer's
    /// surface buffer. Only triangle transitions and the final hit are sent
    /// to the brush so same-triangle paths do not add extra opacity dabs.
    fn paint_model_pointer_segment(
        &mut self,
        previous: Option<egui::Pos2>,
        current: egui::Pos2,
        rect: egui::Rect,
        material_slot: usize,
    ) {
        let (start, steps) = previous.map_or((current, 1), |start| {
            let count =
                (start.distance(current).ceil() as usize).clamp(1, MAX_MODEL_SEGMENT_SAMPLES);
            (start, count)
        });
        for sample in 1..=steps {
            let amount = sample as f32 / steps as f32;
            let position = start.lerp(current, amount);
            if !rect.contains(position) {
                self.previous_uv = None;
                self.previous_model_hit = None;
                continue;
            }
            let local = position - rect.min;
            let x = (local.x / rect.width() * self.view.width as f32)
                .floor()
                .clamp(0.0, (self.view.width - 1) as f32) as usize;
            let y = (local.y / rect.height() * self.view.height as f32)
                .floor()
                .clamp(0.0, (self.view.height - 1) as f32) as usize;
            let Some(hit) = self.view.pick_surface(x, y) else {
                self.previous_uv = None;
                self.previous_model_hit = None;
                continue;
            };
            if hit.material_slot != material_slot {
                // A bound-set transition is handled at the actual pointer
                // event boundary, where the app can switch Texture Sets
                // without mixing one stroke capture across two sets.
                self.previous_uv = None;
                self.previous_model_hit = None;
                continue;
            }
            let triangle_changed = self
                .previous_model_hit
                .is_none_or(|previous| previous.triangle_index != hit.triangle_index);
            if triangle_changed || sample == steps {
                self.paint_surface_hit(hit);
            }
        }
    }

    fn uv_image_rect(&self, viewport: egui::Rect) -> egui::Rect {
        let size = viewport.size() * self.uv_zoom;
        egui::Rect::from_center_size(viewport.center() + self.uv_pan, size)
    }

    fn zoom_uv_at(&mut self, viewport: egui::Rect, pointer: egui::Pos2, scroll: f32) {
        if scroll == 0.0 || !scroll.is_finite() {
            return;
        }
        let old_rect = self.uv_image_rect(viewport);
        let uv = (pointer - old_rect.min) / old_rect.size();
        let zoom = (self.uv_zoom * (scroll * 0.002).exp()).clamp(1.0, 16.0);
        let size = viewport.size() * zoom;
        let desired_min = pointer - egui::vec2(uv.x * size.x, uv.y * size.y);
        let centered_min = viewport.center() - size * 0.5;
        let max_pan = (size - viewport.size()) * 0.5;
        self.uv_zoom = zoom;
        self.uv_pan = (desired_min - centered_min).clamp(-max_pan, max_pan);
        self.previous_uv = None;
    }

    fn view_pane(&mut self, ui: &mut egui::Ui, canvas: Canvas) {
        let ctx = ui.ctx().clone();
        ui.horizontal_wrapped(|ui| {
            self.brush_toolbar(ui);
            ui.separator();
            ui.label(egui::RichText::new(&self.doc.mesh.name).weak().small());
            ui.separator();
            ui.label(if canvas == Canvas::Model {
                "Material · 3D"
            } else {
                "Material · UV"
            });
            if canvas == Canvas::Model {
                ui.separator();
                let reset = ui
                    .button("Reset camera")
                    .on_hover_text("Restore the default 3D view");
                #[cfg(test)]
                capture_control_rect(ui, "test-viewport-reset-camera", reset.rect);
                if reset.clicked() {
                    self.view.reset_camera();
                    self.model_dirty = true;
                    self.previous_model_hit = None;
                    self.previous_model_position = None;
                }
                let gpu_enabled = self.gpu_viewport.is_some();
                let response = ui.add_enabled(
                    gpu_enabled,
                    egui::Checkbox::new(
                        &mut self.gpu_preview_enabled,
                        "GPU preview · experimental",
                    ),
                );
                if !gpu_enabled {
                    response.on_hover_text("No WGPU render state is available; using the CPU reference viewport");
                } else {
                    response.on_hover_text("Experimental depth-tested GPU Base Color preview; CPU reference remains active for painting and picking");
                }
                let mut lighting = self.view.lighting_settings();
                let current = match lighting.environment {
                    EnvironmentPreset::Studio => "Studio",
                    EnvironmentPreset::Warm => "Warm",
                    EnvironmentPreset::Cool => "Cool",
                };
                let response = egui::ComboBox::from_id_salt("viewport-preview-environment")
                    .selected_text(current)
                    .show_ui(ui, |ui| {
                        let mut changed = false;
                        for (preset, label) in [
                            (EnvironmentPreset::Studio, "Studio"),
                            (EnvironmentPreset::Warm, "Warm"),
                            (EnvironmentPreset::Cool, "Cool"),
                        ] {
                            let response =
                                ui.selectable_value(&mut lighting.environment, preset, label);
                            changed |= response.changed();
                            #[cfg(test)]
                            capture_control_rect(
                                ui,
                                match preset {
                                    EnvironmentPreset::Studio => "test-viewport-environment-studio",
                                    EnvironmentPreset::Warm => "test-viewport-environment-warm",
                                    EnvironmentPreset::Cool => "test-viewport-environment-cool",
                                },
                                response.rect,
                            );
                        }
                        changed
                    });
                #[cfg(test)]
                capture_control_rect(ui, "test-viewport-environment", response.response.rect);
                if response.inner == Some(true) {
                    self.view.set_lighting_settings(lighting);
                    self.model_dirty = true;
                }
            }
        });
        ui.separator();
        let available = ui.available_size();
        let size = if canvas == Canvas::UV {
            Vec2::splat(available.x.min(available.y).clamp(128.0, 850.0))
        } else {
            egui::vec2(available.x.max(128.0), available.y.max(128.0))
        };
        if canvas == Canvas::Model {
            // Fill the dock while retaining the existing bounded reference
            // buffer. Uniform scaling keeps displayed geometry and picking in
            // the same aspect ratio even on wide or high-DPI workspaces.
            let scale = (1100.0 / size.x).min(900.0 / size.y).min(1.0);
            let desired = (
                (size.x * scale).round().max(128.0) as usize,
                (size.y * scale).round().max(128.0) as usize,
            );
            if (self.view.width, self.view.height) != desired {
                self.view.resize(desired.0, desired.1);
                self.model_dirty = true;
                self.refresh_textures(&ctx);
            }
        }
        ui.vertical_centered(|ui| {
            let gpu_texture_id = if canvas == Canvas::Model
                && self.gpu_preview_enabled
                && !self.solo_channel
                && !self.solo_layer
            {
                self.gpu_viewport.as_mut().map(|gpu| {
                    // Both previews use the same bounded aspect ratio; the
                    // dock image scales them to the available display area.
                    gpu.resize(self.view.width, self.view.height);
                    let _ = gpu.render(
                        self.view.yaw,
                        self.view.pitch,
                        self.view.distance,
                        crate::painter_classic::PainterClassic::gpu_canvas_clear(),
                    );
                    gpu.texture_id()
                })
            } else {
                None
            };
            let texture_id = match canvas {
                Canvas::Model => {
                    gpu_texture_id.or_else(|| self.model_texture.as_ref().map(TextureHandle::id))
                }
                Canvas::UV => self.uv_texture.as_ref().map(TextureHandle::id),
            };
            let Some(texture_id) = texture_id else {
                return;
            };
            let (viewport, response) = ui.allocate_exact_size(size, egui::Sense::click_and_drag());
            let image_rect = if canvas == Canvas::UV {
                self.uv_image_rect(viewport)
            } else {
                viewport
            };
            ui.painter_at(viewport).image(
                texture_id,
                image_rect,
                egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
                Color32::WHITE,
            );
            #[cfg(test)]
            ui.ctx().data_mut(|data| {
                let id = match canvas {
                    Canvas::Model => egui::Id::new("view-pane-model-canvas-rect"),
                    Canvas::UV => egui::Id::new("view-pane-uv-canvas-rect"),
                };
                data.insert_temp(id, viewport);
            });
            if canvas == Canvas::UV && self.show_uv_wire {
                let painter = ui.painter_at(viewport);
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
                                    image_rect.left() + a[0] * image_rect.width(),
                                    image_rect.top() + a[1] * image_rect.height(),
                                ),
                                egui::pos2(
                                    image_rect.left() + b[0] * image_rect.width(),
                                    image_rect.top() + b[1] * image_rect.height(),
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
                        self.brush.radius as f32 * image_rect.width() / self.doc.size as f32;
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
                    self.previous_uv = None;
                    self.previous_model_hit = None;
                    self.previous_model_position = None;
                }
                if scroll != 0.0 {
                    self.view.zoom(scroll);
                    self.model_dirty = true;
                    self.previous_uv = None;
                    self.previous_model_hit = None;
                    self.previous_model_position = None;
                }
            }
            if canvas == Canvas::UV && response.hovered() {
                let (secondary, delta, scroll, pointer) = ctx.input(|i| {
                    (
                        i.pointer.secondary_down(),
                        i.pointer.delta(),
                        i.raw_scroll_delta.y,
                        i.pointer.hover_pos(),
                    )
                });
                if secondary && delta != Vec2::ZERO {
                    let size = viewport.size() * self.uv_zoom;
                    let max_pan = (size - viewport.size()) * 0.5;
                    self.uv_pan = (self.uv_pan + delta).clamp(-max_pan, max_pan);
                    self.previous_uv = None;
                }
                if let Some(pointer) = pointer {
                    self.zoom_uv_at(viewport, pointer, scroll);
                }
                if response.double_clicked() {
                    self.uv_zoom = 1.0;
                    self.uv_pan = Vec2::ZERO;
                    self.previous_uv = None;
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
            let pointer_inside = pointer.is_some_and(|position| {
                response.rect.contains(position)
                    && (canvas != Canvas::UV || image_rect.contains(position))
            });
            // Paint gestures belong to the image that captured the primary press. Merely
            // dragging a held button over this canvas must not start a stroke here.
            let primary_captured = response.is_pointer_button_down_on() && down;
            if primary_captured && !pointer_inside && !bucket {
                // Pause an owned stroke while outside the image. The next in-canvas event
                // starts a fresh dab instead of connecting across the off-canvas gap.
                self.previous_uv = None;
                self.previous_model_hit = None;
                self.previous_model_position = None;
            }
            if pointer_inside
                && (if bucket {
                    pressed && response.is_pointer_button_down_on()
                } else {
                    primary_captured
                })
                && !secondary
            {
                if let Some(pos) = pointer {
                    let local = pos - image_rect.min;
                    let x = (local.x / image_rect.width()).clamp(0.0, 0.999999);
                    let y = (local.y / image_rect.height()).clamp(0.0, 0.999999);
                    let surface_hit = (canvas == Canvas::Model)
                        .then(|| {
                            self.view.pick_surface(
                                (x * self.view.width as f32) as usize,
                                (y * self.view.height as f32) as usize,
                            )
                        })
                        .flatten();
                    let uv = match canvas {
                        Canvas::UV => Some([x, y]),
                        Canvas::Model => surface_hit.map(|hit| hit.uv),
                    };
                    if let Some(uv) = uv {
                        if canvas == Canvas::Model && self.auto_switch_on_paint {
                            if let Some(hit) = surface_hit {
                                let slot = hit.material_slot;
                                let desired = self.doc.texture_set_for_material(slot);
                                if desired != self.doc.active_set_id {
                                    self.select_texture_set(desired);
                                    // The same pointer event continues into painting below. Rebuild
                                    // the switched set's material cache before the brush patches it.
                                    self.refresh_textures(&ctx);
                                    self.status = format!(
                                        "Painting material {} → {}",
                                        self.doc.mesh.material_name(slot),
                                        self.doc.active_set_name
                                    );
                                }
                            }
                        }
                        if bucket {
                            self.previous_model_hit = None;
                            self.previous_model_position = None;
                            self.fill_uv(uv);
                        } else if let Some(hit) = surface_hit {
                            let previous_position = self.previous_model_position;
                            self.paint_model_pointer_segment(
                                previous_position,
                                pos,
                                response.rect,
                                hit.material_slot,
                            );
                            self.previous_model_position = Some(pos);
                        } else {
                            self.previous_model_hit = None;
                            self.previous_model_position = None;
                            self.paint_uv(uv);
                        }
                    } else {
                        self.previous_uv = None;
                        self.previous_model_hit = None;
                        self.previous_model_position = None;
                    }
                }
            }
        });
    }
}

impl Drop for KaliumApp {
    fn drop(&mut self) {
        if let Some(worker) = self.recovery_compaction_job.take() {
            let _ = worker.join();
        }
        let _ = self.stop_recovery_writer();
    }
}

impl eframe::App for KaliumApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.recovery_compaction_job.is_some() {
            let worker_finished = self
                .recovery_compaction_job
                .as_ref()
                .is_some_and(JoinHandle::is_finished);
            if worker_finished {
                self.finish_recovery_compaction();
            } else {
                self.show_recovery_compaction_progress(ctx);
                return;
            }
        }
        self.reap_retired_recovery_writers();
        if self.pending_history_change.is_some() && !self.advance_pending_history() {
            self.status = "Waiting for recovery journal before applying Undo/Redo".into();
            self.status_bar(ctx);
            ctx.request_repaint_after(Duration::from_millis(16));
            return;
        }
        let writer_error = self
            .recovery_journal
            .as_mut()
            .and_then(JournalWriter::take_error);
        if let Some(error) = writer_error {
            self.disable_recovery_journal(&format!("journal append failed: {error}"));
        }
        self.compact_recovery_journal_if_needed();
        if self.recovery_compaction_job.is_some() {
            self.show_recovery_compaction_progress(ctx);
            return;
        }
        if self.material_id_bake_job.is_some() {
            if ctx.input(|input| input.viewport().close_requested()) {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.material_id_bake_cancel_requested = true;
                if let Some(job) = self.material_id_bake_job.as_ref() {
                    job.cancel();
                }
            }
            self.advance_material_id_bake_job(ctx);
            self.material_id_bake_dialog(ctx);
            self.status_bar(ctx);
            ctx.request_repaint();
            return;
        }
        if self.ao_bake_job.is_some() {
            if ctx.input(|input| input.viewport().close_requested()) {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                if let Some(job) = self.ao_bake_job.as_ref() {
                    job.cancel();
                }
            }
            self.advance_ao_bake_job(ctx);
            self.ao_bake_dialog(ctx);
            self.status_bar(ctx);
            ctx.request_repaint();
            return;
        }
        if self.export_job.is_some() {
            if ctx.input(|input| input.viewport().close_requested()) {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                if let Some(job) = self.export_job.as_mut() {
                    job.cancel();
                }
            }
            self.advance_export_job(ctx);
            self.export_job_dialog(ctx);
            self.status_bar(ctx);
            ctx.request_repaint();
            return;
        }
        let close_requested = ctx.input(|input| input.viewport().close_requested());
        if close_requested && !self.allow_close {
            self.finish_stroke();
            if self.unsaved {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.pending_destructive
                    .get_or_insert(DestructiveAction::Close);
            } else if self.pending_destructive.is_some() {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            } else {
                self.persist_workspace_now();
            }
        }
        let down = ctx.input(|i| i.pointer.primary_down());
        if !down {
            self.finish_stroke();
        }
        self.menu(ctx);
        if std::mem::take(&mut self.pending_material_id_export_dialog) {
            if let Some(path) = rfd::FileDialog::new()
                .add_filter("Material-ID PNG", &["png"])
                .set_file_name("Material-ID.png")
                .save_file()
            {
                if let Err(error) = self.start_material_id_bake_to_path(path) {
                    self.status = format!("Could not start material-ID bake: {error:#}");
                }
            }
            if self.material_id_bake_job.is_some() {
                self.advance_material_id_bake_job(ctx);
                self.material_id_bake_dialog(ctx);
                self.status_bar(ctx);
                ctx.request_repaint();
                return;
            }
        }
        if self.pending_history_change.is_some() {
            self.status = "Waiting for recovery journal before applying Undo/Redo".into();
            self.status_bar(ctx);
            ctx.request_repaint_after(Duration::from_millis(16));
            return;
        }
        self.unsaved_changes_dialog(ctx);
        self.journal_recovery_dialog(ctx);
        self.export_options_dialog(ctx);
        self.tool_shelf(ctx);
        self.dock_shortcuts(ctx);
        self.status_bar(ctx);
        self.refresh_textures(ctx);
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show(ctx, |ui| {
                // Temporarily take the tree: TabViewer gets exclusive access to app state.
                let mut dock = std::mem::replace(&mut self.dock, DockState::new(vec![]));
                let style = crate::painter_classic::PainterClassic::dock_style(ui.style());
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
    fn id(&mut self, pane: &mut Pane) -> egui::Id {
        // Contextual titles may change with selection; pane state must not.
        egui::Id::new(("kalium-dock-pane", *pane))
    }
    fn title(&mut self, pane: &mut Pane) -> egui::WidgetText {
        if *pane == Pane::Properties {
            let kind = match self
                .doc
                .layer_tree
                .find_node(self.doc.layer_tree.selected_node_id)
            {
                Some(LayerNode::Fill(_)) => "FILL",
                Some(LayerNode::Group(_)) => "GROUP",
                _ => "PAINT",
            };
            format!("PROPERTIES — {kind}").into()
        } else {
            pane.label().into()
        }
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
            Pane::Activity => self.activity(ui),
        }
    }
}

#[cfg(test)]
mod unsaved_change_tests {
    use super::{append_screen_damage, ScreenRect, MAX_PENDING_SCREEN_RECTS};
    use super::{
        autosave_session_lock_path, brush_preset, egui, import_brush_preset_to_shelf,
        paths_refer_to_same_file, project_id_for_save_target, project_recovery_lock_path,
        recovery_journal_disabled_path, recovery_journal_path, should_continue_after_choice,
        Action, Canvas, DestructiveAction, DestructiveChoice, DocumentEvent, JournalRecoveryChoice,
        JournalWriter, KaliumApp, LayerCommand, LayerImageTarget, LayerRecoveryProperties,
        PendingStroke, RecentProjectAction, RECOVERY_IMAGE_PATCH_BYTES,
    };
    use crate::{
        activity_log::{ActivityLevel, ActivityLog},
        document::{BlendMode, Channel, Document},
        export,
        history::{History, HistoryDirection, LayerProperties},
        layout,
        mesh::Mesh,
        paint::{Brush, BrushMode},
        project,
        recent_projects::RecentProjects,
        recovery::{self, Journal},
        recovery_store::RecoveryGenerationStore,
        renderer::{EnvironmentPreset, Viewport},
    };

    #[test]
    fn pending_screen_damage_merges_overlap_and_bounds_disjoint_regions() {
        let mut damage = Vec::new();
        assert!(append_screen_damage(
            &mut damage,
            [
                ScreenRect {
                    x: 0,
                    y: 0,
                    width: 10,
                    height: 10,
                },
                ScreenRect {
                    x: 5,
                    y: 5,
                    width: 10,
                    height: 10,
                },
                ScreenRect {
                    x: 100,
                    y: 100,
                    width: 4,
                    height: 4,
                },
            ]
        ));
        assert_eq!(damage.len(), 2);
        assert!(damage.contains(&ScreenRect {
            x: 0,
            y: 0,
            width: 15,
            height: 15,
        }));
        assert!(damage.contains(&ScreenRect {
            x: 100,
            y: 100,
            width: 4,
            height: 4,
        }));

        let mut full = (0..MAX_PENDING_SCREEN_RECTS)
            .map(|index| ScreenRect {
                x: index * 2,
                y: 0,
                width: 1,
                height: 1,
            })
            .collect::<Vec<_>>();
        assert!(!append_screen_damage(
            &mut full,
            [ScreenRect {
                x: MAX_PENDING_SCREEN_RECTS * 2,
                y: 0,
                width: 1,
                height: 1,
            }]
        ));
    }

    const JOURNAL_SCHEMA: u32 = 1;
    use std::{
        collections::HashSet,
        fs,
        path::PathBuf,
        process::{Command, Stdio},
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
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
            let material_id_output = self.0.with_extension("id.png");
            let _ = fs::remove_file(&material_id_output);
            let _ = fs::remove_file(PathBuf::from(format!(
                "{}.json",
                material_id_output.display()
            )));
            let _ = fs::remove_file(self.0.with_extension("expected"));
            let _ = fs::remove_file(self.0.with_extension("ready"));
            let _ = fs::remove_dir_all(self.0.with_extension("exports"));
            let journal_path = recovery_journal_path(&self.0);
            let _ = fs::remove_file(recovery_journal_disabled_path(&journal_path));
            let _ = fs::remove_file(&journal_path);
            let _ = RecoveryGenerationStore::from_journal_path(&journal_path).remove_all();
            if let Ok(lock_path) = project_recovery_lock_path(&self.0) {
                let _ = fs::remove_file(lock_path);
            }
        }
    }

    struct TempShelf(PathBuf);

    impl TempShelf {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "kalium-app-brush-shelf-{}-{nonce}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for TempShelf {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn test_app() -> KaliumApp {
        KaliumApp {
            doc: Document::new(64),
            brush: Brush::default(),
            channel: Channel::BaseColor,
            solo_channel: false,
            solo_layer: false,
            collapsed_layer_groups: HashSet::new(),
            show_uv_wire: true,
            gpu_preview_enabled: false,
            gpu_viewport: None,
            gpu_scene_dirty: true,
            view: Viewport::new(128, 128),
            uv_zoom: 1.0,
            uv_pan: egui::Vec2::ZERO,
            uv_coverage: None,
            uv_clipping: true,
            material: None,
            model_texture: None,
            uv_texture: None,
            paint_dirty: true,
            model_dirty: true,
            model_partial_dirty: Vec::new(),
            history: History::new(),
            previous_uv: None,
            previous_model_hit: None,
            previous_model_position: None,
            pending_stroke: None,
            file_path: None,
            recovery_journal: None,
            retired_recovery_writers: Vec::new(),
            recovery_journal_path: None,
            recovery_compaction_retry_after: None,
            recovery_compaction_job: None,
            pending_journal_recovery: None,
            autosave_session_path: None,
            autosave_session_lock: None,
            autosave_session_lock_path: None,
            project_recovery_lock: None,
            project_recovery_lock_path: None,
            pending_open_project_lock: None,
            recovery_unavailable: None,
            recovery_data_rejected: false,
            rejected_recovery_sources: HashSet::new(),
            pending_history_change: None,
            allow_recovery_change: false,
            unsaved: false,
            pending_destructive: None,
            allow_close: false,
            status: "Ready".into(),
            activity_log: None,
            activity_log_error: None,
            recent_projects: None,
            recent_projects_error: None,
            dock: layout::default_workspace(),
            workspace_path: None,
            last_layout_save: Instant::now(),
            saved_layout: Vec::new(),
            asset_search: String::new(),
            shelf_presets: Vec::new(),
            shelf_skipped_files: 0,
            shelf_error: None,
            show_tool_shelf: true,
            texture_set_name_input: "Default".into(),
            auto_switch_on_paint: true,
            pending_export_dir: None,
            pending_export_all: false,
            export_job: None,
            export_job_dir: None,
            export_job_progress: None,
            export_job_all: false,
            ao_bake_job: None,
            ao_bake_progress: None,
            ao_bake_set_id: None,
            material_id_bake_job: None,
            material_id_bake_set_id: None,
            material_id_bake_set_name: None,
            material_id_bake_output: None,
            material_id_bake_metadata: None,
            pending_material_id_export_dialog: false,
            material_id_bake_cancel_requested: false,
            ao_settings: crate::bake::AmbientOcclusionBakeSettings::default(),
            export_padding_radius: 0,
            export_normal_y: crate::export::NormalYConvention::OpenGl,
            export_profile: export::EngineExportProfile::Custom,
            export_output_size: None,
        }
    }

    fn collect_shape_text(shape: &egui::Shape, output: &mut String) {
        match shape {
            egui::Shape::Text(text) => {
                output.push_str(text.galley.text());
                output.push('\n');
            }
            egui::Shape::Vec(shapes) => {
                for shape in shapes {
                    collect_shape_text(shape, output);
                }
            }
            _ => {}
        }
    }

    #[test]
    fn activity_pane_renders_persisted_entries_and_is_in_default_workspace() {
        assert!(layout::default_workspace()
            .find_tab(&layout::Pane::Activity)
            .is_some());
        let temp = TempShelf::new();
        let mut log = ActivityLog::load(temp.path().join("activity-v1.json")).unwrap();
        log.record(ActivityLevel::Info, "Project", "Saved test project")
            .unwrap();
        let mut app = test_app();
        app.activity_log = Some(log);

        let context = egui::Context::default();
        let output = context.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(640.0, 480.0),
                )),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| app.activity(ui));
            },
        );
        let mut rendered = String::new();
        for clipped in &output.shapes {
            collect_shape_text(&clipped.shape, &mut rendered);
        }
        assert!(rendered.contains("Saved test project"));
        assert!(rendered.contains("Project"));
        assert!(rendered.contains("UTC"));
    }

    #[test]
    fn approved_close_flushes_recent_workspace_changes() {
        let temp = TempShelf::new();
        let path = temp.path().join("workspace-v1.json");
        let mut app = test_app();
        app.workspace_path = Some(path.clone());
        app.saved_layout = layout::encode_workspace(&app.dock).unwrap();
        app.dock = egui_dock::DockState::new(vec![layout::Pane::Layers]);
        app.last_layout_save = Instant::now();

        app.approve_close(&egui::Context::default());

        let persisted = layout::decode_workspace(&fs::read(path).unwrap()).unwrap();
        assert!(persisted.find_tab(&layout::Pane::Layers).is_some());
        assert!(persisted.find_tab(&layout::Pane::Viewport3D).is_none());
        assert_eq!(
            app.saved_layout,
            layout::encode_workspace(&app.dock).unwrap()
        );
        assert!(app.allow_close);
    }

    #[test]
    fn recent_projects_save_and_primary_open_promote_only_accepted_paths() {
        let shelf = TempShelf::new();
        let first = shelf.path().join("first.kalium");
        let second = shelf.path().join("second.kalium");
        let storage = shelf.path().join("recent.json");
        project::save(&Document::new(64), &second).unwrap();
        let mut app = test_app();
        app.recent_projects = Some(RecentProjects::empty(storage.clone()));
        app.save_project_to_path(first.clone()).unwrap();
        assert_eq!(
            app.recent_projects.as_ref().unwrap().entries(),
            &[first.canonicalize().unwrap()]
        );
        assert!(!app.unsaved);
        app.request_open_project(second.clone()).unwrap();
        assert!(app.recovery_journal.is_some());
        assert_eq!(app.file_path.as_ref(), Some(&second));
        assert_eq!(
            app.recent_projects.as_ref().unwrap().entries(),
            &[
                second.canonicalize().unwrap(),
                first.canonicalize().unwrap()
            ]
        );
        app.save_project_to_path(second.clone()).unwrap();
        assert_eq!(
            RecentProjects::load(storage).unwrap().entries(),
            app.recent_projects.as_ref().unwrap().entries()
        );
    }

    #[test]
    fn recent_projects_dirty_open_cancel_discard_and_save_share_lifecycle() {
        let shelf = TempShelf::new();
        let first = shelf.path().join("first.kalium");
        let second = shelf.path().join("second.kalium");
        project::save(&Document::new(64), &first).unwrap();
        project::save(&Document::new(64), &second).unwrap();
        let mut app = test_app();
        app.recent_projects = Some(RecentProjects::empty(shelf.path().join("recent.json")));
        let original_id = app.doc.project_id;
        app.unsaved = true;
        app.request_open_project(first.clone()).unwrap();
        assert!(app.pending_open_project_lock.is_some());
        assert!(app.recent_projects.as_ref().unwrap().entries().is_empty());
        app.resolve_destructive_choice(DestructiveChoice::Cancel, &egui::Context::default());
        assert!(app.pending_open_project_lock.is_none());
        assert_eq!(app.doc.project_id, original_id);
        assert!(app.recent_projects.as_ref().unwrap().entries().is_empty());
        let contender = test_app();
        drop(contender.provisional_project_recovery_lock(&first).unwrap());

        app.request_open_project(first.clone()).unwrap();
        app.resolve_destructive_choice(DestructiveChoice::Discard, &egui::Context::default());
        assert_eq!(app.file_path.as_ref(), Some(&first));
        assert_eq!(
            app.recent_projects.as_ref().unwrap().entries(),
            &[first.canonicalize().unwrap()]
        );
        app.doc.active_set_name = "Saved before switch".into();
        app.unsaved = true;
        app.request_open_project(second.clone()).unwrap();
        assert_eq!(app.recent_projects.as_ref().unwrap().entries().len(), 1);
        app.doc.size = 63;
        app.resolve_destructive_choice(DestructiveChoice::Save, &egui::Context::default());
        assert!(app.status.contains("Error saving project"));
        assert!(app.pending_destructive.is_some());
        assert!(app.pending_open_project_lock.is_some());
        assert_eq!(app.file_path.as_ref(), Some(&first));
        assert_eq!(app.recent_projects.as_ref().unwrap().entries().len(), 1);
        app.doc.size = 64;
        app.resolve_destructive_choice(DestructiveChoice::Save, &egui::Context::default());
        assert_eq!(
            project::load(&first).unwrap().active_set_name,
            "Saved before switch"
        );
        assert_eq!(app.file_path.as_ref(), Some(&second));
        assert_eq!(
            app.recent_projects.as_ref().unwrap().entries(),
            &[
                second.canonicalize().unwrap(),
                first.canonicalize().unwrap()
            ]
        );
    }

    #[test]
    fn recent_projects_recovery_source_excluded_until_save_as() {
        let shelf = TempShelf::new();
        let primary = shelf.path().join("damaged.kalium");
        let temporary = shelf.path().join("damaged.kalium.tmp");
        let output = shelf.path().join("preserved.kalium");
        fs::write(&primary, b"damaged primary").unwrap();
        project::save(&Document::new(64), &temporary).unwrap();
        let mut app = test_app();
        app.recent_projects = Some(RecentProjects::empty(shelf.path().join("recent.json")));
        app.request_open_project(primary.clone()).unwrap();
        assert!(app.file_path.is_none());
        assert!(app.unsaved);
        assert!(app.recent_projects.as_ref().unwrap().entries().is_empty());
        assert_eq!(fs::read(primary).unwrap(), b"damaged primary");
        app.install_recovered_project(Document::new(64), shelf.path().join("checkpoint.kalium"));
        assert!(app.recent_projects.as_ref().unwrap().entries().is_empty());
        app.save_project_to_path(output.clone()).unwrap();
        assert_eq!(
            app.recent_projects.as_ref().unwrap().entries(),
            &[output.canonicalize().unwrap()]
        );
    }

    #[test]
    fn recent_projects_missing_invalid_and_locked_opens_preserve_current_project() {
        let shelf = TempShelf::new();
        let target = shelf.path().join("target.kalium");
        project::save(&Document::new(64), &target).unwrap();
        let mut owner = test_app();
        owner.request_open_project(target.clone()).unwrap();
        let mut app = test_app();
        app.recent_projects = Some(RecentProjects::empty(shelf.path().join("recent.json")));
        let current_id = app.doc.project_id;
        assert!(app.request_open_project(target).is_err());
        let invalid = shelf.path().join("invalid.kalium");
        fs::write(&invalid, b"invalid").unwrap();
        assert!(app.request_open_project(invalid).is_err());
        let missing = shelf.path().join("missing.kalium");
        app.perform_recent_project_action(RecentProjectAction::Open(missing.clone()));
        assert!(app.status.contains("no longer available"));
        assert!(!project_recovery_lock_path(&missing).unwrap().exists());
        assert_eq!(app.doc.project_id, current_id);
        assert!(app.file_path.is_none());
        assert!(app.pending_open_project_lock.is_none());
        assert!(app.recent_projects.as_ref().unwrap().entries().is_empty());
    }

    #[test]
    fn recent_projects_storage_failure_is_nonfatal_to_save_and_open() {
        let shelf = TempShelf::new();
        let blocker = shelf.path().join("not-a-directory");
        fs::write(&blocker, b"keep").unwrap();
        let output = shelf.path().join("saved.kalium");
        let mut app = test_app();
        app.recent_projects = Some(RecentProjects::empty(blocker.join("recent.json")));
        app.unsaved = true;
        let message = app.save_project_to_path(output.clone()).unwrap();
        assert!(message.contains("Recent projects could not be saved"));
        assert!(!app.unsaved);
        assert!(app.recovery_journal.is_some());
        assert_eq!(
            project::load(&output).unwrap().project_id,
            app.doc.project_id
        );
        assert!(app.recent_projects.as_ref().unwrap().entries().is_empty());
        app.request_open_project(output.clone()).unwrap();
        assert_eq!(app.file_path.as_ref(), Some(&output));
        assert!(app.recovery_journal.is_some());
        assert!(app.recent_projects_error.is_some());
        assert_eq!(fs::read(blocker).unwrap(), b"keep");
    }

    #[test]
    fn recent_projects_successful_save_promotes_even_when_recovery_reset_fails() {
        let shelf = TempShelf::new();
        let output = shelf.path().join("saved.kalium");
        let journal = recovery_journal_path(&output);
        let mut store_name = journal.as_os_str().to_os_string();
        store_name.push(".store");
        fs::write(PathBuf::from(store_name), b"block recovery directory").unwrap();
        let mut app = test_app();
        app.recent_projects = Some(RecentProjects::empty(shelf.path().join("recent.json")));
        let message = app.save_project_to_path(output.clone()).unwrap();
        assert!(message.contains("Crash recovery is unavailable"));
        assert!(app.recovery_journal.is_none());
        assert_eq!(
            project::load(&output).unwrap().project_id,
            app.doc.project_id
        );
        assert_eq!(
            app.recent_projects.as_ref().unwrap().entries(),
            &[output.canonicalize().unwrap()]
        );
    }

    #[test]
    fn recent_projects_pending_recovery_rejects_open_without_changing_journal() {
        let source = TempProject::new();
        let shelf = TempShelf::new();
        let target = shelf.path().join("target.kalium");
        project::save(&Document::new(64), &target).unwrap();
        let mut app = app_with_pending_layer_opacity_recovery(&source.0, 0.31);
        app.recent_projects = Some(RecentProjects::empty(shelf.path().join("recent.json")));
        let journal_path = app
            .pending_journal_recovery
            .as_ref()
            .unwrap()
            .journal_path
            .clone();
        let journal_bytes = fs::read(&journal_path).unwrap();
        let project_id = app.doc.project_id;
        app.perform_recent_project_action(RecentProjectAction::Open(target));
        assert!(app.status.contains("Restore or discard"));
        assert!(app.pending_journal_recovery.is_some());
        assert_eq!(app.doc.project_id, project_id);
        assert_eq!(fs::read(journal_path).unwrap(), journal_bytes);
        assert!(app.recent_projects.as_ref().unwrap().entries().is_empty());
    }

    #[test]
    fn recent_projects_pointer_open_and_missing_entry_removal_follow_menu_controls() {
        fn frame(app: &mut KaliumApp, ctx: &egui::Context, events: Vec<egui::Event>) {
            let mut action = None;
            let _ = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(520.0, 300.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default()
                        .show(ctx, |ui| action = app.recent_projects_menu(ui));
                },
            );
            if let Some(action) = action {
                app.perform_recent_project_action(action);
            }
        }
        fn click(app: &mut KaliumApp, ctx: &egui::Context, key: &str) {
            let position = test_control_rect(ctx, key).center();
            frame(app, ctx, vec![egui::Event::PointerMoved(position)]);
            for pressed in [true, false] {
                frame(
                    app,
                    ctx,
                    vec![
                        egui::Event::PointerMoved(position),
                        egui::Event::PointerButton {
                            pos: position,
                            button: egui::PointerButton::Primary,
                            pressed,
                            modifiers: egui::Modifiers::NONE,
                        },
                    ],
                );
            }
        }
        let shelf = TempShelf::new();
        let target = shelf.path().join("target.kalium");
        let missing = shelf.path().join("missing.kalium");
        project::save(&Document::new(64), &target).unwrap();
        fs::write(&missing, b"later removed").unwrap();
        let mut recent = RecentProjects::empty(shelf.path().join("recent.json"));
        recent.record(&target).unwrap();
        recent.record(&missing).unwrap();
        let missing = missing.canonicalize().unwrap();
        fs::remove_file(&missing).unwrap();
        let mut app = test_app();
        app.recent_projects = Some(recent);
        app.unsaved = true;
        let initial_id = app.doc.project_id;
        let ctx = egui::Context::default();
        frame(&mut app, &ctx, Vec::new());
        click(&mut app, &ctx, "test-recent-open-0");
        assert!(
            app.pending_destructive.is_none(),
            "missing file row must be disabled"
        );
        assert_eq!(app.doc.project_id, initial_id);
        click(&mut app, &ctx, "test-recent-open-1");
        assert!(
            app.pending_destructive.is_some(),
            "recent click must use guarded Open"
        );
        assert_eq!(app.doc.project_id, initial_id);
        assert_eq!(app.recent_projects.as_ref().unwrap().entries()[0], missing);
        app.resolve_destructive_choice(DestructiveChoice::Discard, &ctx);
        assert_eq!(
            app.doc.project_id,
            project::load(&target).unwrap().project_id
        );
        assert_eq!(
            app.recent_projects.as_ref().unwrap().entries()[0],
            target.canonicalize().unwrap()
        );
        frame(&mut app, &ctx, Vec::new());
        click(&mut app, &ctx, "test-recent-remove-1");
        assert_eq!(
            app.recent_projects.as_ref().unwrap().entries(),
            &[target.canonicalize().unwrap()]
        );
        assert!(target.is_file(), "removal affects history only");

        // A file can disappear after a valid row is displayed but before dispatch.
        let disappearing = shelf.path().join("disappearing.kalium");
        project::save(&Document::new(64), &disappearing).unwrap();
        app.recent_projects
            .as_mut()
            .unwrap()
            .record(&disappearing)
            .unwrap();
        frame(&mut app, &ctx, Vec::new());
        let current_id = app.doc.project_id;
        fs::remove_file(&disappearing).unwrap();
        app.perform_recent_project_action(RecentProjectAction::Open(disappearing));
        assert!(app.status.contains("no longer available"));
        assert_eq!(app.doc.project_id, current_id);
        assert!(app.pending_destructive.is_none());
    }

    #[test]
    fn native_save_cannot_overwrite_or_create_a_mislabeled_spp_project() {
        let shelf = TempShelf::new();
        let mut app = test_app();
        let identity = app.doc.project_id;
        let existing = shelf.path().join("original.SPP");
        let original = b"\x89HDF\r\n\x1a\nsynthetic preservation fixture";
        fs::write(&existing, original).unwrap();
        for path in [existing.clone(), shelf.path().join("new.spp")] {
            let error = app.save_project_to_path(path.clone()).unwrap_err();
            assert!(error.to_string().contains("SPP writing is not implemented"));
            assert!(!path.with_extension("spp.tmp").exists());
        }
        assert_eq!(fs::read(existing).unwrap(), original);
        assert!(!shelf.path().join("new.spp").exists());
        assert_eq!(app.doc.project_id, identity);
        assert!(app.file_path.is_none());
    }

    #[test]
    fn contextual_properties_title_keeps_a_stable_dock_identity() {
        let mut app = test_app();
        let mut pane = layout::Pane::Properties;
        let identity = egui_dock::TabViewer::id(&mut app, &mut pane);
        assert_eq!(
            egui_dock::TabViewer::title(&mut app, &mut pane).text(),
            "PROPERTIES — PAINT"
        );
        for (command, title) in [
            (LayerCommand::AddFill, "PROPERTIES — FILL"),
            (LayerCommand::AddGroup, "PROPERTIES — GROUP"),
        ] {
            app.layer_command(command);
            assert_eq!(
                egui_dock::TabViewer::title(&mut app, &mut pane).text(),
                title
            );
            assert_eq!(egui_dock::TabViewer::id(&mut app, &mut pane), identity);
        }
        for mut other in layout::Pane::ALL
            .into_iter()
            .filter(|pane| *pane != layout::Pane::Properties)
        {
            assert_ne!(egui_dock::TabViewer::id(&mut app, &mut other), identity);
        }
    }

    #[test]
    fn dock_shortcut_buttons_restore_panes_without_changing_project_content() {
        fn frame(app: &mut KaliumApp, ctx: &egui::Context, events: Vec<egui::Event>) {
            let _ = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1920.0, 1080.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ctx| app.dock_shortcuts(ctx),
            );
        }
        let mut app = test_app();
        let identity = app.doc.project_id;
        let original = app.doc.composite(Channel::BaseColor);
        let ctx = egui::Context::default();
        crate::painter_classic::PainterClassic::apply(&ctx);
        for (pane, glyph) in [
            (layout::Pane::TextureSettings, "settings"),
            (layout::Pane::Properties, "material"),
            (layout::Pane::Activity, "history"),
            (layout::Pane::BrushStudio, "paint"),
        ] {
            layout::set_pane_visible(&mut app.dock, pane, false);
            frame(&mut app, &ctx, Vec::new());
            let position = test_control_rect(&ctx, &format!("dock-shortcut-{glyph}")).center();
            frame(&mut app, &ctx, vec![egui::Event::PointerMoved(position)]);
            for pressed in [true, false] {
                frame(
                    &mut app,
                    &ctx,
                    vec![
                        egui::Event::PointerMoved(position),
                        egui::Event::PointerButton {
                            pos: position,
                            button: egui::PointerButton::Primary,
                            pressed,
                            modifiers: egui::Modifiers::NONE,
                        },
                    ],
                );
            }
            assert!(app.dock.find_tab(&pane).is_some());
        }
        assert!(layout::Pane::ALL
            .iter()
            .all(|pane| app.dock.find_tab(pane).is_some()));
        assert_eq!(app.doc.project_id, identity);
        assert_eq!(app.doc.composite(Channel::BaseColor), original);
        assert!(!app.unsaved);
        assert!(!app.history.can_undo());
    }

    #[test]
    fn wide_model_canvas_fills_dock_with_bounded_reference_buffer_and_picking() {
        let mut app = test_app();
        let ctx = egui::Context::default();
        crate::painter_classic::PainterClassic::apply(&ctx);
        let _ = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1360.0, 1000.0),
                )),
                events: Vec::new(),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| app.view_pane(ui, Canvas::Model));
            },
        );
        let rect = canvas_rect(&ctx, Canvas::Model);
        assert!(rect.width() > 1300.0);
        assert!(app.view.width <= 1100 && app.view.height <= 900);
        assert!(
            (rect.aspect_ratio() - app.view.width as f32 / app.view.height as f32).abs() < 0.003
        );
        let (x, y, uv) = (0..app.view.height)
            .step_by(8)
            .find_map(|y| {
                (0..app.view.width)
                    .step_by(8)
                    .find_map(|x| app.view.pick(x, y).map(|uv| (x, y, uv)))
            })
            .unwrap();
        let position = egui::pos2(
            rect.left() + (x as f32 + 0.5) / app.view.width as f32 * rect.width(),
            rect.top() + (y as f32 + 0.5) / app.view.height as f32 * rect.height(),
        );
        let pixel = (
            ((uv[0] * app.doc.size as f32) as u32).min(app.doc.size - 1),
            ((uv[1] * app.doc.size as f32) as u32).min(app.doc.size - 1),
        );
        let before = app
            .doc
            .composite(Channel::BaseColor)
            .get_pixel(pixel.0, pixel.1)
            .0;
        for pressed in [true, false] {
            let _ = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1360.0, 1000.0),
                    )),
                    events: vec![
                        egui::Event::PointerMoved(position),
                        egui::Event::PointerButton {
                            pos: position,
                            button: egui::PointerButton::Primary,
                            pressed,
                            modifiers: egui::Modifiers::NONE,
                        },
                    ],
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| app.view_pane(ui, Canvas::Model));
                },
            );
        }
        assert_ne!(
            app.doc
                .composite(Channel::BaseColor)
                .get_pixel(pixel.0, pixel.1)
                .0,
            before
        );
    }

    #[test]
    fn compact_toolbar_controls_edit_the_existing_brush_fields() {
        fn frame(app: &mut KaliumApp, ctx: &egui::Context, events: Vec<egui::Event>) {
            let _ = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1920.0, 1080.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        ui.horizontal_wrapped(|ui| app.brush_toolbar(ui));
                    });
                },
            );
        }
        let mut app = test_app();
        let original_doc = serde_json::to_value(&app.doc.mesh).unwrap();
        let ctx = egui::Context::default();
        crate::painter_classic::PainterClassic::apply(&ctx);
        for key in [
            "toolbar-radius",
            "toolbar-flow",
            "toolbar-hardness",
            "toolbar-spacing",
        ] {
            frame(&mut app, &ctx, Vec::new());
            let rect = test_control_rect(&ctx, key);
            let position = egui::pos2(rect.left() + 95.0, rect.bottom() - 7.0);
            frame(&mut app, &ctx, vec![egui::Event::PointerMoved(position)]);
            for pressed in [true, false] {
                frame(
                    &mut app,
                    &ctx,
                    vec![
                        egui::Event::PointerMoved(position),
                        egui::Event::PointerButton {
                            pos: position,
                            button: egui::PointerButton::Primary,
                            pressed,
                            modifiers: egui::Modifiers::NONE,
                        },
                    ],
                );
            }
        }
        assert!(
            app.brush.radius > 100 && app.brush.radius <= 180,
            "brush after toolbar input: {:?}",
            app.brush
        );
        assert!(app.brush.opacity > 0.8 && app.brush.opacity <= 1.0);
        assert!(app.brush.hardness > 0.8 && app.brush.hardness <= 1.0);
        assert!(app.brush.spacing > 1.0 && app.brush.spacing <= 1.5);
        assert_eq!(serde_json::to_value(&app.doc.mesh).unwrap(), original_doc);
        assert!(!app.unsaved);
    }

    #[test]
    fn unsaved_choice_modal_blocks_background_menu_shortcuts() {
        let shelf = TempShelf::new();
        let primary = shelf.path().join("current.kalium");
        let mut app = test_app();
        project::save(&app.doc, &primary).unwrap();
        app.file_path = Some(primary.clone());
        let original = fs::read(&primary).unwrap();
        app.doc.selected_layer_mut().unwrap().name = "Unsaved edit".into();
        app.unsaved = true;
        app.pending_destructive = Some(DestructiveAction::Close);
        let ctx = egui::Context::default();
        for (key, shift) in [
            (egui::Key::S, false),
            (egui::Key::S, true),
            (egui::Key::O, false),
            (egui::Key::E, false),
        ] {
            let modifiers = egui::Modifiers {
                command: true,
                ctrl: true,
                shift,
                ..Default::default()
            };
            let _ = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1200.0, 700.0),
                    )),
                    modifiers,
                    events: vec![egui::Event::Key {
                        key,
                        physical_key: None,
                        pressed: true,
                        repeat: false,
                        modifiers,
                    }],
                    ..Default::default()
                },
                |ctx| app.menu(ctx),
            );
            assert!(matches!(
                app.pending_destructive,
                Some(DestructiveAction::Close)
            ));
            assert_eq!(fs::read(&primary).unwrap(), original);
            assert_eq!(app.brush.mode, BrushMode::Paint);
            assert!(app.unsaved);
        }
        app.resolve_destructive_choice(DestructiveChoice::Cancel, &ctx);
        assert!(app.pending_destructive.is_none());
        assert!(app.unsaved);
    }

    #[test]
    fn recent_projects_selection_closes_both_file_menu_and_submenu() {
        fn frame(app: &mut KaliumApp, ctx: &egui::Context, events: Vec<egui::Event>) {
            let _ = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(900.0, 600.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ctx| app.menu(ctx),
            );
        }
        fn click(app: &mut KaliumApp, ctx: &egui::Context, key: &str) {
            let position = test_control_rect(ctx, key).center();
            frame(app, ctx, vec![egui::Event::PointerMoved(position)]);
            for pressed in [true, false] {
                frame(
                    app,
                    ctx,
                    vec![
                        egui::Event::PointerMoved(position),
                        egui::Event::PointerButton {
                            pos: position,
                            button: egui::PointerButton::Primary,
                            pressed,
                            modifiers: egui::Modifiers::NONE,
                        },
                    ],
                );
            }
        }
        let shelf = TempShelf::new();
        let target = shelf.path().join("target.kalium");
        project::save(&Document::new(64), &target).unwrap();
        let mut recent = RecentProjects::empty(shelf.path().join("recent.json"));
        recent.record(&target).unwrap();
        let mut app = test_app();
        app.recent_projects = Some(recent);
        let ctx = egui::Context::default();
        frame(&mut app, &ctx, Vec::new());
        click(&mut app, &ctx, "test-file-menu");
        click(&mut app, &ctx, "test-file-recent-menu");
        frame(&mut app, &ctx, Vec::new());
        click(&mut app, &ctx, "test-recent-open-0");
        assert_eq!(app.file_path.as_ref(), Some(&target));
        ctx.data_mut(|data| {
            data.remove::<egui::Rect>(egui::Id::new("test-file-recent-menu"));
            data.remove::<egui::Rect>(egui::Id::new("test-recent-open-0"));
        });
        frame(&mut app, &ctx, Vec::new());
        assert!(
            ctx.data(|data| data.get_temp::<egui::Rect>(egui::Id::new("test-file-recent-menu")))
                .is_none(),
            "File menu should close after opening a project"
        );
        assert!(
            ctx.data(|data| data.get_temp::<egui::Rect>(egui::Id::new("test-recent-open-0")))
                .is_none(),
            "Recent submenu should close after opening a project"
        );
    }

    #[test]
    fn recent_projects_legacy_primary_reopen_restores_edits_and_migrates_identity() {
        let shelf = TempShelf::new();
        let primary = shelf.path().join("legacy-demo.kalium");
        let source = include_bytes!("../examples/Kalium-Demo.kalium");
        fs::write(&primary, source).unwrap();
        let history_path = shelf.path().join("recent.json");
        let mut app = test_app();
        app.recent_projects = Some(RecentProjects::empty(history_path.clone()));
        app.request_open_project(primary.clone()).unwrap();
        let identity = app.doc.project_id;
        let event = DocumentEvent::LayerOpacity {
            set_id: app.doc.active_set_id,
            layer_id: app.doc.selected_layer().unwrap().id,
            opacity: 0.31,
        };
        event.apply(&mut app.doc).unwrap();
        app.change_with_recovery_events(&[event]);
        drop(app);

        let mut reopened = test_app();
        reopened.recent_projects = Some(RecentProjects::load(history_path).unwrap());
        reopened.request_open_project(primary.clone()).unwrap();
        assert_eq!(
            reopened.doc.project_id, identity,
            "legacy recovery needs stable identity before migration/save"
        );
        assert!(reopened.pending_journal_recovery.is_some());
        reopened.resolve_journal_recovery(JournalRecoveryChoice::Restore);
        assert_eq!(reopened.doc.selected_layer().unwrap().opacity, 0.31);
        assert!(reopened.recovery_journal.is_some());
        assert_eq!(
            fs::read(&primary).unwrap(),
            source,
            "opening and restoring must not modify the legacy source archive"
        );
        reopened.save_project_to_path(primary.clone()).unwrap();
        assert_eq!(
            reopened.recent_projects.as_ref().unwrap().entries().len(),
            1
        );
        drop(reopened);
        let migrated = project::load(&primary).unwrap();
        assert_eq!(migrated.project_id, identity);
        assert_eq!(migrated.selected_layer().unwrap().opacity, 0.31);
    }

    #[test]
    fn rejected_discard_preserves_journal_when_primary_baseline_disappears() {
        let temp = TempProject::new();
        let mut app = app_with_pending_layer_opacity_recovery(&temp.0, 0.31);
        let journal_path = app
            .pending_journal_recovery
            .as_ref()
            .unwrap()
            .journal_path
            .clone();
        let original = fs::read(&journal_path).unwrap();
        fs::remove_file(&temp.0).unwrap();
        app.resolve_journal_recovery(JournalRecoveryChoice::Discard);
        assert!(app.status.contains("discard failed"));
        assert!(app.recovery_data_rejected);
        assert!(app.recovery_journal.is_none());
        assert_eq!(fs::read(&journal_path).unwrap(), original);
        assert!(!recovery_journal_disabled_path(&journal_path).exists());
    }

    #[test]
    fn rejected_replay_preserves_original_journal_through_save_and_discard() {
        let shelf = TempShelf::new();
        let primary = shelf.path().join("replay.kalium");
        let baseline = Document::new(64);
        project::save(&baseline, &primary).unwrap();
        let journal_path = recovery_journal_path(&primary);
        let mut journal = Journal::create(
            &journal_path,
            baseline.project_id,
            project::file_digest(&primary).unwrap(),
            1,
        )
        .unwrap();
        journal
            .append(
                &recovery::encode_event_batch_bounded(&[DocumentEvent::LayerOpacity {
                    set_id: baseline.active_set_id,
                    layer_id: u32::MAX,
                    opacity: 0.31,
                }])
                .unwrap(),
            )
            .unwrap();
        drop(journal);
        let original = fs::read(&journal_path).unwrap();
        let mut app = test_app();
        app.request_open_project(primary.clone()).unwrap();
        assert!(app.pending_journal_recovery.is_some());
        app.resolve_journal_recovery(JournalRecoveryChoice::Restore);
        assert!(app.recovery_data_rejected);
        assert!(app.recovery_journal.is_none());
        assert_eq!(app.doc.selected_layer().unwrap().opacity, 1.0);
        assert_eq!(fs::read(&journal_path).unwrap(), original);
        assert!(!recovery_journal_disabled_path(&journal_path).exists());
        assert!(app
            .save_project_to_path(primary)
            .unwrap()
            .contains("preserved"));
        let other = shelf.path().join("other.kalium");
        project::save(&Document::new(64), &other).unwrap();
        app.unsaved = true;
        app.request_open_project(other).unwrap();
        app.resolve_destructive_choice(DestructiveChoice::Discard, &egui::Context::default());
        assert_eq!(fs::read(journal_path).unwrap(), original);
    }

    #[test]
    fn rejected_save_sequence_overflow_preserves_generation_through_discard() {
        let shelf = TempShelf::new();
        let primary = shelf.path().join("overflow.kalium");
        let baseline = Document::new(64);
        project::save(&baseline, &primary).unwrap();
        let mut app = test_app();
        app.request_open_project(primary.clone()).unwrap();
        app.stop_recovery_writer().unwrap();
        let store = RecoveryGenerationStore::from_journal_path(&recovery_journal_path(&primary));
        let generation = store
            .publish_snapshot(baseline.project_id, 1, u64::MAX, |path| {
                project::save(&baseline, path)
            })
            .unwrap();
        let mut journal = Journal::open(
            &generation.journal_path,
            baseline.project_id,
            generation.baseline_digest,
            1,
        )
        .unwrap()
        .0;
        journal
            .append(
                &recovery::encode_event_batch_bounded(&[DocumentEvent::LayerOpacity {
                    set_id: baseline.active_set_id,
                    layer_id: baseline.selected_layer().unwrap().id,
                    opacity: 0.31,
                }])
                .unwrap(),
            )
            .unwrap();
        drop(journal);
        let original = fs::read(&generation.journal_path).unwrap();
        let manifest = generation
            .journal_path
            .parent()
            .unwrap()
            .join("manifest.json");
        let original_manifest = fs::read(&manifest).unwrap();
        assert!(app
            .save_project_to_path(primary)
            .unwrap()
            .contains("preserved"));
        assert!(app.recovery_data_rejected);
        let other = shelf.path().join("other.kalium");
        project::save(&Document::new(64), &other).unwrap();
        app.unsaved = true;
        app.request_open_project(other).unwrap();
        app.resolve_destructive_choice(DestructiveChoice::Discard, &egui::Context::default());
        assert_eq!(fs::read(generation.journal_path).unwrap(), original);
        assert_eq!(fs::read(manifest).unwrap(), original_manifest);
    }

    #[test]
    fn legacy_rejected_random_id_journal_survives_save_and_save_as() {
        let shelf = TempShelf::new();
        let primary = shelf.path().join("legacy.kalium");
        fs::write(&primary, include_bytes!("../examples/Kalium-Demo.kalium")).unwrap();
        let baseline = project::load(&primary).unwrap();
        let identity = baseline.project_id;
        let original_opacity = baseline.selected_layer().unwrap().opacity;
        let journal_path = recovery_journal_path(&primary);
        let mut old = Journal::create(
            &journal_path,
            uuid::Uuid::new_v4(),
            project::file_digest(&primary).unwrap(),
            1,
        )
        .unwrap();
        old.append(
            &recovery::encode_event_batch_bounded(&[DocumentEvent::LayerOpacity {
                set_id: baseline.active_set_id,
                layer_id: baseline.selected_layer().unwrap().id,
                opacity: 0.31,
            }])
            .unwrap(),
        )
        .unwrap();
        drop(old);
        let original_journal = fs::read(&journal_path).unwrap();
        let mut app = test_app();
        app.request_open_project(primary.clone()).unwrap();
        assert!(app.recovery_data_rejected);
        assert!(app.pending_journal_recovery.is_none());
        let message = app.save_project_to_path(primary.clone()).unwrap();
        assert!(message.contains("preserved"));
        assert_eq!(project::load(&primary).unwrap().project_id, identity);
        assert_eq!(fs::read(&journal_path).unwrap(), original_journal);
        let fork = shelf.path().join("preserved-work.kalium");
        app.save_project_to_path(fork).unwrap();
        assert!(app.recovery_journal.is_some());
        assert!(!app.recovery_data_rejected);
        assert_eq!(fs::read(&journal_path).unwrap(), original_journal);
        app.request_open_project(primary).unwrap();
        assert!(app.recovery_data_rejected);
        assert!(app.pending_journal_recovery.is_none());
        assert_eq!(app.doc.selected_layer().unwrap().opacity, original_opacity);
        assert_eq!(fs::read(journal_path).unwrap(), original_journal);
    }

    #[test]
    fn legacy_rejected_generation_survives_save_and_discarded_document_transition() {
        fn files(directory: &std::path::Path) -> Vec<(PathBuf, Vec<u8>)> {
            let mut entries = fs::read_dir(directory)
                .unwrap()
                .map(|entry| {
                    let path = entry.unwrap().path();
                    let bytes = fs::read(&path).unwrap();
                    (path, bytes)
                })
                .collect::<Vec<_>>();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            entries
        }
        let shelf = TempShelf::new();
        let primary = shelf.path().join("legacy.kalium");
        fs::write(&primary, include_bytes!("../examples/Kalium-Demo.kalium")).unwrap();
        let mut old_baseline = project::load(&primary).unwrap();
        let identity = old_baseline.project_id;
        old_baseline.project_id = uuid::Uuid::new_v4();
        let journal_path = recovery_journal_path(&primary);
        let store = RecoveryGenerationStore::from_journal_path(&journal_path);
        let generation = store
            .publish_snapshot(old_baseline.project_id, 1, 0, |path| {
                project::save(&old_baseline, path)
            })
            .unwrap();
        let mut old = Journal::open(
            &generation.journal_path,
            old_baseline.project_id,
            generation.baseline_digest,
            1,
        )
        .unwrap()
        .0;
        old.append(
            &recovery::encode_event_batch_bounded(&[DocumentEvent::LayerOpacity {
                set_id: old_baseline.active_set_id,
                layer_id: old_baseline.selected_layer().unwrap().id,
                opacity: 0.31,
            }])
            .unwrap(),
        )
        .unwrap();
        drop(old);
        let directory = generation.journal_path.parent().unwrap();
        let before = files(directory);
        let mut app = test_app();
        app.request_open_project(primary.clone()).unwrap();
        assert!(app.recovery_data_rejected);
        assert!(app.pending_journal_recovery.is_none());
        assert!(app
            .save_project_to_path(primary.clone())
            .unwrap()
            .contains("preserved"));
        assert_eq!(project::load(&primary).unwrap().project_id, identity);
        assert_eq!(
            files(directory),
            before,
            "manifest, checkpoint and journal must remain byte-identical"
        );
        let other = shelf.path().join("other.kalium");
        project::save(&Document::new(64), &other).unwrap();
        app.unsaved = true;
        app.request_open_project(other).unwrap();
        app.resolve_destructive_choice(DestructiveChoice::Discard, &egui::Context::default());
        assert_eq!(
            files(directory),
            before,
            "Discard affects the current document's edits, not rejected external recovery data"
        );
        drop(app);
        let mut reopened = test_app();
        reopened.request_open_project(primary).unwrap();
        assert!(reopened.recovery_data_rejected);
        assert!(reopened.pending_journal_recovery.is_none());
        assert_eq!(files(directory), before);
    }

    #[test]
    fn legacy_rejected_autosave_survives_save_as_and_open_without_deleting_baseline() {
        let shelf = TempShelf::new();
        let baseline_path = shelf.path().join("autosave-session.kalium");
        let source = include_bytes!("../examples/Kalium-Demo.kalium");
        fs::write(&baseline_path, source).unwrap();
        let baseline = project::load(&baseline_path).unwrap();
        let journal_path = recovery_journal_path(&baseline_path);
        let mut old = Journal::create(
            &journal_path,
            uuid::Uuid::new_v4(),
            project::file_digest(&baseline_path).unwrap(),
            1,
        )
        .unwrap();
        old.append(
            &recovery::encode_event_batch_bounded(&[DocumentEvent::LayerOpacity {
                set_id: baseline.active_set_id,
                layer_id: baseline.selected_layer().unwrap().id,
                opacity: 0.31,
            }])
            .unwrap(),
        )
        .unwrap();
        drop(old);
        let original_journal = fs::read(&journal_path).unwrap();
        let target = shelf.path().join("saved.kalium");
        let mut app = test_app();
        app.initialize_autosave_session_at(&baseline_path).unwrap();
        assert!(app.recovery_data_rejected);
        app.save_project_to_path(target.clone()).unwrap();
        assert!(app.recovery_journal.is_some());
        assert!(app.autosave_session_path.is_none());
        assert!(app.autosave_session_lock.is_none());
        assert_eq!(fs::read(&baseline_path).unwrap(), source);
        assert_eq!(fs::read(&journal_path).unwrap(), original_journal);
        drop(app);

        let mut reopened = test_app();
        reopened
            .initialize_autosave_session_at(&baseline_path)
            .unwrap();
        assert!(reopened.recovery_data_rejected);
        reopened.request_open_project(target).unwrap();
        assert!(reopened.autosave_session_path.is_none());
        assert!(reopened.autosave_session_lock.is_none());
        assert_eq!(fs::read(&baseline_path).unwrap(), source);
        assert_eq!(fs::read(&journal_path).unwrap(), original_journal);
        // New-session setup cannot reload the preserved baseline over a fresh document.
        reopened.doc = Document::new(64);
        let fresh_identity = reopened.doc.project_id;
        assert!(reopened
            .initialize_autosave_session_at(&baseline_path)
            .is_err());
        assert_eq!(reopened.doc.project_id, fresh_identity);
        assert_eq!(fs::read(baseline_path).unwrap(), source);
        assert_eq!(fs::read(journal_path).unwrap(), original_journal);
    }

    fn run_settings_panel_frame(
        app: &mut KaliumApp,
        ctx: &egui::Context,
        texture_settings: bool,
        events: Vec<egui::Event>,
    ) {
        let screen_rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(800.0, 1000.0));
        let _ = ctx.run(
            egui::RawInput {
                screen_rect: Some(screen_rect),
                events,
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    if texture_settings {
                        app.texture_settings(ui);
                    } else {
                        app.tools(ui);
                    }
                });
            },
        );
    }

    fn run_viewport_toolbar_frame(
        app: &mut KaliumApp,
        ctx: &egui::Context,
        events: Vec<egui::Event>,
    ) {
        let screen_rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(900.0, 700.0));
        let _ = ctx.run(
            egui::RawInput {
                screen_rect: Some(screen_rect),
                events,
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| app.view_pane(ui, Canvas::Model));
            },
        );
    }

    fn test_control_rect(ctx: &egui::Context, key: &str) -> egui::Rect {
        ctx.data(|data| data.get_temp::<egui::Rect>(egui::Id::new(key)))
            .expect("control geometry should be captured after a UI frame")
    }

    #[test]
    fn asset_search_filters_and_selects_complete_bundled_brush_preset() {
        let mut app = test_app();
        let expected = brush_preset::load_bytes(include_bytes!("../examples/Hard-Detail.kbrush"))
            .unwrap()
            .1;

        let ctx = egui::Context::default();
        let screen_rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(480.0, 640.0));
        let mut run_assets = |events| {
            let _ = ctx.run(
                egui::RawInput {
                    screen_rect: Some(screen_rect),
                    events,
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| app.assets(ui));
                },
            );
        };
        run_assets(Vec::new());
        let search_rect = test_control_rect(&ctx, "asset-preset-search");
        let search_pos = search_rect.center();
        for pressed in [true, false] {
            run_assets(vec![
                egui::Event::PointerMoved(search_pos),
                egui::Event::PointerButton {
                    pos: search_pos,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                },
            ]);
        }
        run_assets(vec![egui::Event::Text("hard".into())]);
        let hard_rect = ctx
            .data(|data| {
                data.get_temp::<egui::Rect>(egui::Id::new(("asset-preset-card", "Hard Detail")))
            })
            .expect("the matching preset should be shown");
        assert_eq!(
            ctx.data(|data| data.get_temp::<usize>(egui::Id::new("asset-preset-visible-count"))),
            Some(1)
        );

        for pressed in [true, false] {
            let pos = hard_rect.center();
            run_assets(vec![
                egui::Event::PointerMoved(pos),
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                },
            ]);
        }

        assert_eq!(app.asset_search, "hard");
        assert_eq!(app.status, "Selected brush: Hard Detail");
        assert_eq!(
            serde_json::to_value(&app.brush).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
    }

    #[test]
    fn asset_search_shows_no_preset_when_query_has_no_match() {
        let mut app = test_app();
        app.asset_search = "no matching brush".into();
        let ctx = egui::Context::default();
        let _ = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(480.0, 640.0),
                )),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| app.assets(ui));
            },
        );
        assert_eq!(
            ctx.data(|data| data.get_temp::<usize>(egui::Id::new("asset-preset-visible-count"))),
            Some(0)
        );
    }

    #[test]
    fn local_brush_preset_import_is_bounded_non_overwriting_and_selectable() {
        let shelf = TempShelf::new();
        let source = shelf.path().join("outside/Local Detail.kbrush");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        let expected = Brush {
            radius: 37,
            opacity: 0.63,
            ..Brush::default()
        };
        brush_preset::save(&expected, "Local Detail", &source).unwrap();

        let imported = import_brush_preset_to_shelf(&source, shelf.path()).unwrap();
        let imported_again = import_brush_preset_to_shelf(&source, shelf.path()).unwrap();
        assert_eq!(imported.file_name().unwrap(), "Local-Detail.kbrush");
        assert_eq!(imported_again.file_name().unwrap(), "Local-Detail-1.kbrush");
        let scan = brush_preset::scan_shelf(shelf.path()).unwrap();
        assert_eq!(scan.presets.len(), 2);
        assert!(scan
            .presets
            .iter()
            .all(|preset| preset.name == "Local Detail"));

        let mut app = test_app();
        app.shelf_presets = scan.presets;
        app.asset_search = "local".into();
        let ctx = egui::Context::default();
        let screen_rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(480.0, 640.0));
        let _ = ctx.run(
            egui::RawInput {
                screen_rect: Some(screen_rect),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| app.assets(ui));
            },
        );
        let card = ctx
            .data(|data| {
                data.get_temp::<egui::Rect>(egui::Id::new(("asset-preset-card", "Local Detail")))
            })
            .expect("local preset should appear in search results");
        let pos = card.center();
        for pressed in [true, false] {
            let _ = ctx.run(
                egui::RawInput {
                    screen_rect: Some(screen_rect),
                    events: vec![
                        egui::Event::PointerMoved(pos),
                        egui::Event::PointerButton {
                            pos,
                            button: egui::PointerButton::Primary,
                            pressed,
                            modifiers: egui::Modifiers::NONE,
                        },
                    ],
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| app.assets(ui));
                },
            );
        }
        assert_eq!(app.status, "Selected brush: Local Detail");
        assert_eq!(
            serde_json::to_value(&app.brush).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
    }

    fn click_control_slider(
        app: &mut KaliumApp,
        ctx: &egui::Context,
        texture_settings: bool,
        key: &str,
    ) {
        run_settings_panel_frame(app, ctx, texture_settings, Vec::new());
        let rect = test_control_rect(ctx, key);
        let position = egui::pos2(rect.left() + rect.width() * 0.15, rect.center().y);
        run_settings_panel_frame(
            app,
            ctx,
            texture_settings,
            vec![egui::Event::PointerMoved(position)],
        );
        let rect = test_control_rect(ctx, key);
        let position = egui::pos2(rect.left() + rect.width() * 0.15, rect.center().y);
        run_settings_panel_frame(
            app,
            ctx,
            texture_settings,
            vec![
                egui::Event::PointerMoved(position),
                egui::Event::PointerButton {
                    pos: position,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
        run_settings_panel_frame(
            app,
            ctx,
            texture_settings,
            vec![
                egui::Event::PointerMoved(position),
                egui::Event::PointerButton {
                    pos: position,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
    }

    #[test]
    fn history_shortcuts_respect_focused_text_editors() {
        fn with_history_entry() -> KaliumApp {
            let mut app = test_app();
            let layer_id = app.doc.selected_layer().expect("selected paint layer").id;
            let before =
                LayerProperties::capture(app.doc.selected_layer().expect("selected paint layer"));
            app.doc
                .selected_layer_mut()
                .expect("selected paint layer")
                .opacity = 0.25;
            let after =
                LayerProperties::capture(app.doc.selected_layer().expect("selected paint layer"));
            app.history.record_properties(layer_id, before, after);
            app
        }

        fn run_frame(
            app: &mut KaliumApp,
            ctx: &egui::Context,
            screen_rect: egui::Rect,
            events: Vec<egui::Event>,
            text: &mut String,
            show_text: bool,
        ) -> Option<egui::Rect> {
            let mut text_rect = None;
            let modifiers = events
                .iter()
                .find_map(|event| match event {
                    egui::Event::Key { modifiers, .. } => Some(*modifiers),
                    _ => None,
                })
                .unwrap_or_default();
            let _ = ctx.run(
                egui::RawInput {
                    screen_rect: Some(screen_rect),
                    modifiers,
                    events,
                    ..Default::default()
                },
                |ctx| {
                    // The real app processes these global shortcuts before its
                    // dock panels, using focus retained from the prior frame.
                    app.menu(ctx);
                    if show_text {
                        egui::CentralPanel::default().show(ctx, |ui| {
                            text_rect = Some(ui.add(egui::TextEdit::singleline(text)).rect);
                        });
                    }
                },
            );
            text_rect
        }

        fn key_event(key: egui::Key, shift: bool) -> egui::Event {
            let modifiers = egui::Modifiers {
                ctrl: true,
                command: true,
                shift,
                ..Default::default()
            };
            egui::Event::Key {
                key,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers,
            }
        }

        let screen_rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(800.0, 600.0));
        let ctx = egui::Context::default();
        let mut app = with_history_entry();
        let mut text = String::new();
        let rect = run_frame(&mut app, &ctx, screen_rect, Vec::new(), &mut text, true).unwrap();
        let pos = rect.center();
        run_frame(
            &mut app,
            &ctx,
            screen_rect,
            vec![
                egui::Event::PointerMoved(pos),
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
            &mut text,
            true,
        );
        run_frame(
            &mut app,
            &ctx,
            screen_rect,
            vec![egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }],
            &mut text,
            true,
        );
        run_frame(&mut app, &ctx, screen_rect, Vec::new(), &mut text, true);

        run_frame(
            &mut app,
            &ctx,
            screen_rect,
            vec![key_event(egui::Key::Z, false)],
            &mut text,
            true,
        );
        assert_eq!(
            app.doc
                .selected_layer()
                .expect("selected paint layer")
                .opacity,
            0.25
        );
        assert!(app.history.can_undo());
        assert!(!app.history.can_redo());

        // A focused editor also owns both supported Redo key combinations.
        assert!(app.history.undo(&mut app.doc));
        run_frame(
            &mut app,
            &ctx,
            screen_rect,
            vec![key_event(egui::Key::Y, false)],
            &mut text,
            true,
        );
        assert_eq!(
            app.doc
                .selected_layer()
                .expect("selected paint layer")
                .opacity,
            1.0
        );
        assert!(app.history.can_redo());
        run_frame(
            &mut app,
            &ctx,
            screen_rect,
            vec![key_event(egui::Key::Z, true)],
            &mut text,
            true,
        );
        assert_eq!(
            app.doc
                .selected_layer()
                .expect("selected paint layer")
                .opacity,
            1.0
        );
        assert!(app.history.can_redo());

        // Without a text editor, the same global shortcut still changes history.
        let mut unfocused = with_history_entry();
        let unfocused_ctx = egui::Context::default();
        run_frame(
            &mut unfocused,
            &unfocused_ctx,
            screen_rect,
            Vec::new(),
            &mut String::new(),
            false,
        );
        run_frame(
            &mut unfocused,
            &unfocused_ctx,
            screen_rect,
            vec![key_event(egui::Key::Z, false)],
            &mut String::new(),
            false,
        );
        assert_eq!(
            unfocused
                .doc
                .selected_layer()
                .expect("selected paint layer")
                .opacity,
            1.0
        );
        assert!(unfocused.history.can_redo());
    }

    #[test]
    fn egui_lighting_control_updates_reference_render_state() {
        let mut app = test_app();
        app.model_dirty = false;
        let before = app.view.lighting_settings();
        let ctx = egui::Context::default();
        click_control_slider(&mut app, &ctx, false, "test-lighting-intensity");
        assert_ne!(app.view.lighting_settings(), before);
        assert!(app.model_dirty);
    }

    #[test]
    fn egui_environment_preset_updates_reference_render_state() {
        let mut app = test_app();
        app.model_dirty = false;
        let ctx = egui::Context::default();
        run_settings_panel_frame(&mut app, &ctx, false, Vec::new());
        let environment_rect = test_control_rect(&ctx, "test-lighting-environment");
        let click = |app: &mut KaliumApp, position: egui::Pos2| {
            for pressed in [true, false] {
                run_settings_panel_frame(
                    app,
                    &ctx,
                    false,
                    vec![
                        egui::Event::PointerMoved(position),
                        egui::Event::PointerButton {
                            pos: position,
                            button: egui::PointerButton::Primary,
                            pressed,
                            modifiers: egui::Modifiers::NONE,
                        },
                    ],
                );
            }
        };

        click(&mut app, environment_rect.center());
        run_settings_panel_frame(&mut app, &ctx, false, Vec::new());
        let warm_rect = test_control_rect(&ctx, "test-lighting-environment-warm");
        click(&mut app, warm_rect.center());

        assert_eq!(
            app.view.lighting_settings().environment,
            EnvironmentPreset::Warm
        );
        assert!(app.model_dirty);
    }

    #[test]
    fn viewport_toolbar_resets_camera_and_selects_preview_environment() {
        let mut app = test_app();
        let ctx = egui::Context::default();
        let initial_camera = (app.view.yaw, app.view.pitch, app.view.distance);
        app.view.orbit(60.0, -25.0);
        app.view.zoom(120.0);
        app.model_dirty = false;
        run_viewport_toolbar_frame(&mut app, &ctx, Vec::new());
        let reset = test_control_rect(&ctx, "test-viewport-reset-camera");
        let click = |app: &mut KaliumApp, position: egui::Pos2| {
            for pressed in [true, false] {
                run_viewport_toolbar_frame(
                    app,
                    &ctx,
                    vec![
                        egui::Event::PointerMoved(position),
                        egui::Event::PointerButton {
                            pos: position,
                            button: egui::PointerButton::Primary,
                            pressed,
                            modifiers: egui::Modifiers::NONE,
                        },
                    ],
                );
            }
        };
        click(&mut app, reset.center());
        assert_eq!(
            (app.view.yaw, app.view.pitch, app.view.distance),
            initial_camera
        );
        assert!(app.model_dirty);

        app.model_dirty = false;
        let environment = test_control_rect(&ctx, "test-viewport-environment");
        click(&mut app, environment.center());
        run_viewport_toolbar_frame(&mut app, &ctx, Vec::new());
        let cool = test_control_rect(&ctx, "test-viewport-environment-cool");
        click(&mut app, cool.center());
        assert_eq!(
            app.view.lighting_settings().environment,
            EnvironmentPreset::Cool
        );
        assert!(app.model_dirty);
    }

    #[test]
    fn egui_ao_control_persists_the_active_set_setting() {
        let mut app = test_app();
        let before = app.ao_settings;
        let ctx = egui::Context::default();
        click_control_slider(&mut app, &ctx, true, "test-ao-samples");
        click_control_slider(&mut app, &ctx, true, "test-ao-seed");
        assert_ne!(app.ao_settings, before);
        assert_ne!(app.ao_settings.seed, before.seed);
        assert!(app.unsaved);
        assert_eq!(
            serde_json::from_value::<crate::bake::AmbientOcclusionBakeSettings>(
                app.doc.active_set_extensions[crate::bake::AO_SETTINGS_EXTENSION_KEY].clone()
            )
            .unwrap(),
            app.ao_settings
        );
    }

    #[test]
    fn app_export_job_advances_and_cancel_reports_partial_output() {
        let temp = TempProject::new();
        let output = temp.0.with_extension("exports");
        let mut app = test_app();
        app.doc.add_texture_set("Second Set").unwrap();
        app.export_job = Some(
            crate::export::ExportJob::new(
                &app.doc,
                &output,
                crate::export::ExportOptions::default(),
            )
            .unwrap(),
        );
        app.export_job_dir = Some(output.clone());
        app.export_job_all = true;
        let ctx = egui::Context::default();

        app.advance_export_job(&ctx);
        let progress = app.export_job_progress.as_ref().unwrap();
        assert_eq!(progress.state, crate::export::ExportJobState::Running);
        assert_eq!(progress.completed_maps, 1);
        assert_eq!(progress.total_maps, 22);
        assert!(output.join("001_Default/Kalium_BaseColor.png").is_file());
        assert!(app.export_job.is_some());

        app.export_job.as_mut().unwrap().cancel();
        app.advance_export_job(&ctx);
        assert!(app.export_job.is_none());
        assert!(app.export_job_progress.is_none());
        assert!(app.status.contains("canceled after 1 of 22 maps"));
        assert!(app.status.contains("partial files remain"));
        assert!(output.join("001_Default/Kalium_BaseColor.png").is_file());
        assert!(!output.join("001_Default/Kalium_Roughness.png").exists());
    }

    #[test]
    fn material_id_settings_button_requests_a_save_path_from_a_headless_click() {
        let mut app = test_app();
        let ctx = egui::Context::default();
        run_settings_panel_frame(&mut app, &ctx, true, Vec::new());
        let rect = test_control_rect(&ctx, "test-material-id-export");
        let position = rect.center();
        for pressed in [true, false] {
            run_settings_panel_frame(
                &mut app,
                &ctx,
                true,
                vec![
                    egui::Event::PointerMoved(position),
                    egui::Event::PointerButton {
                        pos: position,
                        button: egui::PointerButton::Primary,
                        pressed,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
            );
        }
        assert!(app.pending_material_id_export_dialog);
        assert!(app.material_id_bake_job.is_none());
        assert!(!app.unsaved);
    }

    #[test]
    fn material_id_bake_exports_snapshot_png_and_sidecar_without_document_edits() {
        let temp = TempProject::new();
        let output = temp.0.with_extension("id.png");
        let mut app = test_app();
        let original_set_id = app.doc.active_set_id;
        let exported_set_id = app.doc.add_texture_set("Second Set").unwrap();
        app.doc.try_resize(128).unwrap();
        let set_id = app.doc.active_set_id;
        assert_eq!(set_id, exported_set_id);
        let set_name = app.doc.active_set_name.clone();
        let size = app.doc.size;
        let mesh = app.doc.mesh.clone();
        let bindings = app.doc.material_bindings.clone();
        let expected = crate::bake::bake_material_id(
            &mesh,
            size,
            &bindings,
            set_id,
            crate::document::MAX_DECODED_BYTES,
            &std::sync::atomic::AtomicBool::new(false),
        )
        .unwrap();
        app.start_material_id_bake_to_path(output.clone()).unwrap();
        // Simulate a set switch while the worker owns its immutable snapshot.
        app.doc.activate_texture_set(original_set_id).unwrap();
        let before_pixels = app.doc.composite(Channel::BaseColor);
        let before_layer_count = app.doc.layer_tree.roots.len();
        let before_selected_id = app.doc.selected_layer().expect("selected paint layer").id;
        let before_decoded_bytes = app.doc.decoded_bytes();
        let before_unsaved = app.unsaved;
        let before_undo = app.history.can_undo();

        let ctx = egui::Context::default();
        let deadline = Instant::now() + Duration::from_secs(5);
        while app.material_id_bake_job.is_some() && Instant::now() < deadline {
            app.advance_material_id_bake_job(&ctx);
            std::thread::yield_now();
        }
        assert!(
            app.material_id_bake_job.is_none(),
            "worker should finish promptly"
        );
        assert!(output.is_file());
        let decoded = image::open(&output).unwrap().to_rgba8();
        assert_eq!(decoded.dimensions(), (size, size));
        assert_eq!(decoded, expected);
        let sidecar = PathBuf::from(format!("{}.json", output.display()));
        let metadata: serde_json::Value =
            serde_json::from_slice(&fs::read(&sidecar).unwrap()).unwrap();
        assert_eq!(metadata["texture_set"]["id"], set_id);
        assert_eq!(metadata["texture_set"]["name"], set_name);
        assert_eq!(metadata["texture_set"]["active_in_source_project"], true);
        assert_eq!(metadata["dimensions"]["width"], size);
        assert_eq!(metadata["encoding"]["slot_index_bits"], 24);
        assert_eq!(
            metadata["material_slots"].as_array().unwrap().len(),
            mesh.material_count()
        );
        assert!(app.status.contains("Exported material IDs"));
        assert_eq!(app.doc.composite(Channel::BaseColor), before_pixels);
        assert_eq!(app.doc.layer_tree.roots.len(), before_layer_count);
        assert_eq!(
            app.doc.selected_layer().expect("selected paint layer").id,
            before_selected_id
        );
        assert_eq!(app.doc.decoded_bytes(), before_decoded_bytes);
        assert_eq!(app.doc.active_set_id, original_set_id);
        assert_eq!(app.unsaved, before_unsaved);
        assert_eq!(app.history.can_undo(), before_undo);
    }

    #[test]
    fn canceled_material_id_bake_produces_no_png_or_sidecar_and_keeps_document() {
        let temp = TempProject::new();
        let output = temp.0.with_extension("id.png");
        let mut app = test_app();
        let before_pixels = app.doc.composite(Channel::BaseColor);
        let before_layers = app.doc.layer_tree.roots.len();
        app.start_material_id_bake_to_path(output.clone()).unwrap();
        app.material_id_bake_cancel_requested = true;
        app.material_id_bake_job.as_ref().unwrap().cancel();

        let ctx = egui::Context::default();
        let deadline = Instant::now() + Duration::from_secs(5);
        while app.material_id_bake_job.is_some() && Instant::now() < deadline {
            app.advance_material_id_bake_job(&ctx);
            std::thread::yield_now();
        }
        assert!(app.material_id_bake_job.is_none());
        assert!(!output.exists());
        assert!(!PathBuf::from(format!("{}.json", output.display())).exists());
        assert!(app.status.contains("canceled"));
        assert_eq!(app.doc.layer_tree.roots.len(), before_layers);
        assert_eq!(app.doc.composite(Channel::BaseColor), before_pixels);
        assert!(!app.unsaved);
    }

    #[test]
    fn material_id_bake_rejects_bad_path_and_output_failure_without_document_changes() {
        let temp = TempProject::new();
        let invalid_path = temp.0.with_extension("id.jpg");
        let output = temp.0.with_extension("id.png");
        fs::create_dir(&output).unwrap();
        let mut app = test_app();
        let before_pixels = app.doc.composite(Channel::BaseColor);
        let before_bytes = app.doc.decoded_bytes();

        assert!(app.start_material_id_bake_to_path(invalid_path).is_err());
        assert!(app.material_id_bake_job.is_none());
        app.start_material_id_bake_to_path(output.clone()).unwrap();
        let ctx = egui::Context::default();
        let deadline = Instant::now() + Duration::from_secs(5);
        while app.material_id_bake_job.is_some() && Instant::now() < deadline {
            app.advance_material_id_bake_job(&ctx);
            std::thread::yield_now();
        }
        assert!(app.material_id_bake_job.is_none());
        assert!(app.status.contains("Material-ID export failed"));
        assert!(output.is_dir());
        assert!(!PathBuf::from(format!("{}.json", output.display())).exists());
        assert_eq!(app.doc.decoded_bytes(), before_bytes);
        assert_eq!(app.doc.composite(Channel::BaseColor), before_pixels);
        assert!(!app.unsaved);
        fs::remove_dir_all(output).unwrap();
    }

    fn run_canvas_frame(
        app: &mut KaliumApp,
        ctx: &egui::Context,
        canvas: Canvas,
        events: Vec<egui::Event>,
    ) {
        let screen_rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(640.0, 480.0));
        let _ = ctx.run(
            egui::RawInput {
                screen_rect: Some(screen_rect),
                events,
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| app.view_pane(ui, canvas));
            },
        );
    }

    fn canvas_rect(ctx: &egui::Context, canvas: Canvas) -> egui::Rect {
        let id = match canvas {
            Canvas::Model => egui::Id::new("view-pane-model-canvas-rect"),
            Canvas::UV => egui::Id::new("view-pane-uv-canvas-rect"),
        };
        ctx.data(|data| data.get_temp::<egui::Rect>(id))
            .expect("canvas geometry should be captured after a render pass")
    }

    fn run_journal_dialog_frame(
        app: &mut KaliumApp,
        ctx: &egui::Context,
        events: Vec<egui::Event>,
    ) {
        let screen_rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(640.0, 480.0));
        let _ = ctx.run(
            egui::RawInput {
                screen_rect: Some(screen_rect),
                events,
                ..Default::default()
            },
            |ctx| app.journal_recovery_dialog(ctx),
        );
    }

    fn app_with_pending_layer_opacity_recovery(path: &std::path::Path, opacity: f32) -> KaliumApp {
        let baseline = Document::new(64);
        project::save(&baseline, path).unwrap();
        let mut app = test_app();
        app.install_open_project(baseline, path.to_owned(), false);
        let event = DocumentEvent::LayerOpacity {
            set_id: app.doc.active_set_id,
            layer_id: app.doc.selected_layer().expect("selected paint layer").id,
            opacity,
        };
        event.apply(&mut app.doc).unwrap();
        app.change_with_recovery_events(std::slice::from_ref(&event));
        drop(app);

        let mut reopened = test_app();
        reopened.install_open_project(project::load(path).unwrap(), path.to_owned(), false);
        assert!(reopened.pending_journal_recovery.is_some());
        reopened
    }

    #[test]
    fn clearing_an_absent_channel_is_a_true_noop() {
        let mut app = test_app();
        app.channel = Channel::BaseColor;
        app.unsaved = false;

        let first_layer_id = app.doc.selected_layer().expect("selected paint layer").id;
        app.doc.add_layer().unwrap();
        let second_layer_id = app.doc.selected_layer().expect("selected paint layer").id;
        let before = crate::history::LayerProperties::capture(
            app.doc.selected_layer().expect("selected paint layer"),
        );
        app.doc
            .selected_layer_mut()
            .expect("selected paint layer")
            .visible = false;
        let after = crate::history::LayerProperties::capture(
            app.doc.selected_layer().expect("selected paint layer"),
        );
        app.history
            .record_properties(second_layer_id, before, after);
        let before = crate::history::LayerProperties::capture(&app.doc.layer_tree.roots[0]);
        app.doc.layer_tree.roots[0].opacity = 0.5;
        let after = crate::history::LayerProperties::capture(&app.doc.layer_tree.roots[0]);
        app.history.record_properties(first_layer_id, before, after);
        assert!(app.history.undo(&mut app.doc));
        assert!(app.history.can_undo());
        assert!(app.history.can_redo());
        app.unsaved = false;

        app.perform(Action::ClearChannel);

        assert!(!app
            .doc
            .selected_layer()
            .expect("selected paint layer")
            .pixels
            .contains_key(&Channel::BaseColor));
        assert!(!app.unsaved);
        assert!(app.history.can_undo());
        assert!(app.history.can_redo());
    }

    #[test]
    fn egui_model_stroke_paints_both_uv_charts_across_a_seam_without_bridging() {
        let mut app = test_app();
        app.doc = Document::new(128);
        app.doc.mesh = crate::mesh::Mesh {
            name: "UV seam test".into(),
            vertices: vec![
                crate::mesh::Vertex {
                    position: [0.0, 0.0, 0.0],
                    normal: [0.0, 0.0, 1.0],
                    uv: [0.05, 0.05],
                    tangent: None,
                    extensions: Default::default(),
                },
                crate::mesh::Vertex {
                    position: [1.0, 0.0, 0.0],
                    normal: [0.0, 0.0, 1.0],
                    uv: [0.25, 0.05],
                    tangent: None,
                    extensions: Default::default(),
                },
                crate::mesh::Vertex {
                    position: [1.0, 1.0, 0.0],
                    normal: [0.0, 0.0, 1.0],
                    uv: [0.25, 0.25],
                    tangent: None,
                    extensions: Default::default(),
                },
                crate::mesh::Vertex {
                    position: [0.00005, 0.0, 0.0],
                    normal: [0.0, 0.0, 1.0],
                    uv: [0.75, 0.75],
                    tangent: None,
                    extensions: Default::default(),
                },
                crate::mesh::Vertex {
                    position: [1.00005, 1.0, 0.0],
                    normal: [0.0, 0.0, 1.0],
                    uv: [0.95, 0.95],
                    tangent: None,
                    extensions: Default::default(),
                },
                crate::mesh::Vertex {
                    position: [0.0, 1.0, 0.0],
                    normal: [0.0, 0.0, 1.0],
                    uv: [0.75, 0.95],
                    tangent: None,
                    extensions: Default::default(),
                },
            ],
            indices: vec![0, 1, 2, 3, 4, 5],
            materials: vec!["Default".into()],
            triangle_materials: vec![0, 0],
            extra_uv_sets: Default::default(),
            extensions: Default::default(),
        };
        app.uv_clipping = false;
        app.brush.radius = 5;
        app.brush.opacity = 1.0;
        app.brush.hardness = 1.0;
        app.brush.color = [230, 35, 20, 255];
        let ctx = egui::Context::default();
        run_canvas_frame(&mut app, &ctx, Canvas::Model, Vec::new());
        let canvas = canvas_rect(&ctx, Canvas::Model);
        let before = app.doc.composite(Channel::BaseColor);

        let mut seam_pair = None;
        'search: for y in 0..app.view.height {
            for x in 0..app.view.width {
                let Some(first) = app.view.pick_surface(x, y) else {
                    continue;
                };
                for (nx, ny) in [(x.saturating_add(1), y), (x, y.saturating_add(1))] {
                    if nx >= app.view.width || ny >= app.view.height {
                        continue;
                    }
                    if let Some(second) = app.view.pick_surface(nx, ny) {
                        if first.triangle_index == 0 && second.triangle_index == 1 {
                            seam_pair = Some(((x, y), (nx, ny)));
                            break 'search;
                        }
                        if first.triangle_index == 1 && second.triangle_index == 0 {
                            seam_pair = Some(((nx, ny), (x, y)));
                            break 'search;
                        }
                    }
                }
            }
        }
        let (first, second) = seam_pair.expect("rendered triangles should share a visible seam");
        let screen_position = |(x, y): (usize, usize)| {
            egui::pos2(
                canvas.left() + (x as f32 + 0.5) * canvas.width() / app.view.width as f32,
                canvas.top() + (y as f32 + 0.5) * canvas.height() / app.view.height as f32,
            )
        };
        let first_pos = screen_position(first);
        let second_pos = screen_position(second);
        run_canvas_frame(
            &mut app,
            &ctx,
            Canvas::Model,
            vec![
                egui::Event::PointerMoved(first_pos),
                egui::Event::PointerButton {
                    pos: first_pos,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
        run_canvas_frame(
            &mut app,
            &ctx,
            Canvas::Model,
            vec![egui::Event::PointerMoved(second_pos)],
        );
        run_canvas_frame(
            &mut app,
            &ctx,
            Canvas::Model,
            vec![
                egui::Event::PointerMoved(second_pos),
                egui::Event::PointerButton {
                    pos: second_pos,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
        app.finish_stroke();

        let image = app.doc.composite(Channel::BaseColor);
        let changed = |x: usize, y: usize| {
            image.get_pixel(x as u32, y as u32) != before.get_pixel(x as u32, y as u32)
        };
        assert!(
            (0..48).any(|x| (0..48).any(|y| changed(x, y))),
            "stroke should paint the first UV chart"
        );
        assert!(
            (80..128).any(|x| (80..128).any(|y| changed(x, y))),
            "stroke should paint the second UV chart"
        );
        assert!(
            !(48..80).any(|x| (48..80).any(|y| changed(x, y))),
            "stroke must not bridge the atlas gap"
        );
        let after = image;
        assert!(
            app.history.can_undo(),
            "the model drag should commit one undoable stroke"
        );
        assert!(app.history.undo(&mut app.doc));
        assert_eq!(app.doc.composite(Channel::BaseColor), before);
        assert!(app.history.redo(&mut app.doc));
        assert_eq!(app.doc.composite(Channel::BaseColor), after);
    }

    #[test]
    fn sparse_model_pointer_jump_resamples_intermediate_seam_triangle() {
        fn vertex(position: [f32; 3], uv: [f32; 2]) -> crate::mesh::Vertex {
            crate::mesh::Vertex {
                position,
                normal: [0.0, 0.0, 1.0],
                uv,
                tangent: None,
                extensions: Default::default(),
            }
        }
        let mut app = test_app();
        app.doc = Document::new(128);
        let p0 = [0.0, 0.0, 0.0];
        let p1 = [1.0, 0.0, 0.0];
        let p2 = [1.0, 1.0, 0.0];
        let p3 = [0.0, 1.0, 0.0];
        let middle = [0.0, 0.5, 0.0];
        app.doc.mesh = crate::mesh::Mesh {
            name: "Sparse three-chart seam test".into(),
            vertices: vec![
                vertex(p0, [0.05, 0.05]),
                vertex(p1, [0.25, 0.05]),
                vertex(p2, [0.25, 0.25]),
                vertex(p0, [0.40, 0.40]),
                vertex(p2, [0.60, 0.40]),
                vertex(middle, [0.40, 0.60]),
                vertex(middle, [0.75, 0.75]),
                vertex(p2, [0.95, 0.75]),
                vertex(p3, [0.75, 0.95]),
            ],
            indices: vec![0, 1, 2, 3, 4, 5, 6, 7, 8],
            materials: vec!["Default".into()],
            triangle_materials: vec![0; 3],
            extra_uv_sets: Default::default(),
            extensions: Default::default(),
        };
        app.uv_clipping = false;
        app.brush.radius = 3;
        app.brush.opacity = 1.0;
        app.brush.hardness = 1.0;
        app.brush.color = [230, 35, 20, 255];
        let ctx = egui::Context::default();
        run_canvas_frame(&mut app, &ctx, Canvas::Model, Vec::new());
        let canvas = canvas_rect(&ctx, Canvas::Model);
        let before = app.doc.composite(Channel::BaseColor);
        let mut endpoints = [Vec::new(), Vec::new()];
        for y in (0..app.view.height).step_by(2) {
            for x in (0..app.view.width).step_by(2) {
                if let Some(hit) = app.view.pick_surface(x, y) {
                    let candidate = match hit.triangle_index {
                        0 if endpoints[0].len() < 200 => Some((x, y)),
                        2 if endpoints[1].len() < 200 => Some((x, y)),
                        _ => None,
                    };
                    if let Some(candidate) = candidate {
                        endpoints[if hit.triangle_index == 0 { 0 } else { 1 }].push(candidate);
                    }
                }
            }
        }
        let mut pair = None;
        'find: for &(sx, sy) in &endpoints[0] {
            for &(ex, ey) in &endpoints[1] {
                let dx = ex as f32 - sx as f32;
                let dy = ey as f32 - sy as f32;
                let steps = dx.hypot(dy).ceil().max(1.0) as usize;
                if steps < 8 {
                    continue;
                }
                let mut seen_middle = false;
                let mut all_surface = true;
                for step in 1..=steps {
                    let t = step as f32 / steps as f32;
                    let x = (sx as f32 + dx * t).floor() as usize;
                    let y = (sy as f32 + dy * t).floor() as usize;
                    match app.view.pick_surface(x, y) {
                        Some(hit) => seen_middle |= hit.triangle_index == 1,
                        None => all_surface = false,
                    }
                }
                if all_surface && seen_middle {
                    pair = Some(((sx, sy), (ex, ey)));
                    break 'find;
                }
            }
        }
        let ((sx, sy), (ex, ey)) = pair.expect("a visible drag should cross the middle triangle");
        let to_screen = |x: usize, y: usize| {
            egui::pos2(
                canvas.left() + (x as f32 + 0.5) * canvas.width() / app.view.width as f32,
                canvas.top() + (y as f32 + 0.5) * canvas.height() / app.view.height as f32,
            )
        };
        let start = to_screen(sx, sy);
        let end = to_screen(ex, ey);
        run_canvas_frame(
            &mut app,
            &ctx,
            Canvas::Model,
            vec![
                egui::Event::PointerMoved(start),
                egui::Event::PointerButton {
                    pos: start,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
        run_canvas_frame(
            &mut app,
            &ctx,
            Canvas::Model,
            vec![egui::Event::PointerMoved(end)],
        );
        run_canvas_frame(
            &mut app,
            &ctx,
            Canvas::Model,
            vec![
                egui::Event::PointerMoved(end),
                egui::Event::PointerButton {
                    pos: end,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
        app.finish_stroke();

        let image = app.doc.composite(Channel::BaseColor);
        let changed_in = |xr: std::ops::Range<u32>, yr: std::ops::Range<u32>| {
            yr.into_iter().any(|y| {
                xr.clone()
                    .any(|x| image.get_pixel(x, y) != before.get_pixel(x, y))
            })
        };
        assert!(changed_in(0..40, 0..40), "first UV chart was skipped");
        assert!(changed_in(48..80, 48..80), "middle UV chart was skipped");
        assert!(changed_in(88..128, 88..128), "last UV chart was skipped");
        assert_eq!(image.get_pixel(40, 10), before.get_pixel(40, 10));
        let after = image;
        assert!(app.history.undo(&mut app.doc));
        assert_eq!(app.doc.composite(Channel::BaseColor), before);
        assert!(app.history.redo(&mut app.doc));
        assert_eq!(app.doc.composite(Channel::BaseColor), after);
    }

    #[test]
    fn two_material_masked_layer_workflow_paints_saves_reopens_and_exports() {
        let temp = TempProject::new();
        let export_dir = temp.0.with_extension("exports");
        let _cleanup = TempProject(export_dir.clone());
        let fixture =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/TwoMaterials.obj");
        let mut app = test_app();
        app.doc
            .import_mesh(Mesh::from_file(&fixture).unwrap())
            .unwrap();
        let leather_set = app.doc.active_set_id;
        let metal_set = app.doc.add_texture_set("Metal").unwrap();
        app.doc.bind_material(0, Some(leather_set)).unwrap();
        app.doc.bind_material(1, Some(metal_set)).unwrap();
        app.doc.activate_texture_set(leather_set).unwrap();
        app.rebuild_coverage();
        app.auto_switch_on_paint = false;
        app.uv_clipping = true;
        app.brush.radius = 5;
        app.brush.opacity = 1.0;
        app.brush.hardness = 1.0;
        app.brush.color = [225, 32, 48, 255];
        app.model_dirty = true;

        let ctx = egui::Context::default();
        app.refresh_textures(&ctx);
        run_canvas_frame(&mut app, &ctx, Canvas::Model, Vec::new());
        let canvas = canvas_rect(&ctx, Canvas::Model);
        let mut adjacent = None;
        'search: for y in 0..app.view.height {
            for x in 0..app.view.width {
                let Some(a) = app.view.pick_surface(x, y) else {
                    continue;
                };
                if a.material_slot != 0 {
                    continue;
                }
                for (nx, ny) in [(x + 1, y), (x, y + 1)] {
                    if nx < app.view.width && ny < app.view.height {
                        if let Some(b) = app.view.pick_surface(nx, ny) {
                            if b.material_slot == 0 && a.triangle_index != b.triangle_index {
                                adjacent = Some(((x, y), (nx, ny)));
                                break 'search;
                            }
                        }
                    }
                }
            }
        }
        let (first, second) = adjacent.expect("Leather's two adjacent triangles should be visible");
        let screen = |(x, y): (usize, usize)| {
            egui::pos2(
                canvas.left() + (x as f32 + 0.5) * canvas.width() / app.view.width as f32,
                canvas.top() + (y as f32 + 0.5) * canvas.height() / app.view.height as f32,
            )
        };
        let first = screen(first);
        let second = screen(second);
        let drag = |app: &mut KaliumApp, color: [u8; 4]| {
            app.brush.color = color;
            run_canvas_frame(
                app,
                &ctx,
                Canvas::Model,
                vec![
                    egui::Event::PointerMoved(first),
                    egui::Event::PointerButton {
                        pos: first,
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
            );
            run_canvas_frame(
                app,
                &ctx,
                Canvas::Model,
                vec![egui::Event::PointerMoved(second)],
            );
            run_canvas_frame(
                app,
                &ctx,
                Canvas::Model,
                vec![
                    egui::Event::PointerMoved(second),
                    egui::Event::PointerButton {
                        pos: second,
                        button: egui::PointerButton::Primary,
                        pressed: false,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
            );
            app.finish_stroke();
        };

        app.doc.add_layer().unwrap();
        app.doc
            .selected_layer_mut()
            .expect("selected paint layer")
            .name = "Masked red stroke".into();
        app.doc
            .selected_layer_mut()
            .expect("selected paint layer")
            .mask
            .pixels_mut()
            .for_each(|pixel| pixel.0[0] = 190);
        drag(&mut app, [225, 32, 48, 255]);
        let first_layer_id = app.doc.selected_layer().expect("selected paint layer").id;

        app.doc.add_layer().unwrap();
        app.doc
            .selected_layer_mut()
            .expect("selected paint layer")
            .name = "Masked gold stroke".into();
        app.doc
            .selected_layer_mut()
            .expect("selected paint layer")
            .mask
            .pixels_mut()
            .for_each(|pixel| pixel.0[0] = 128);
        drag(&mut app, [245, 180, 32, 255]);
        let second_layer_id = app.doc.selected_layer().expect("selected paint layer").id;
        let metal_before = app
            .doc
            .composite_for_set(metal_set, Channel::BaseColor)
            .unwrap();
        let leather_composite = app
            .doc
            .composite_for_set(leather_set, Channel::BaseColor)
            .unwrap();
        assert!(leather_composite.pixels().any(|pixel| pixel.0[3] > 0));
        assert_eq!(
            app.doc
                .composite_for_set(metal_set, Channel::BaseColor)
                .unwrap(),
            metal_before,
            "painting the Leather material must not alter Metal"
        );
        assert_eq!(app.doc.active_set_id, leather_set);
        assert!(app.doc.layer_tree.roots.iter().any(|layer| {
            layer.id == first_layer_id && layer.mask.pixels().any(|pixel| pixel.0[0] == 190)
        }));
        assert!(app.doc.layer_tree.roots.iter().any(|layer| {
            layer.id == second_layer_id && layer.mask.pixels().any(|pixel| pixel.0[0] == 128)
        }));

        project::save(&app.doc, &temp.0).unwrap();
        let reopened = project::load(&temp.0).unwrap();
        assert_eq!(
            reopened
                .composite_for_set(leather_set, Channel::BaseColor)
                .unwrap(),
            leather_composite,
            "the stacked masked-layer composite must survive save and reopen exactly"
        );
        assert_eq!(
            reopened
                .composite_for_set(metal_set, Channel::BaseColor)
                .unwrap(),
            metal_before
        );
        export::export_all_texture_sets(&reopened, &export_dir).unwrap();
        let exported_leather = image::open(export_dir.join("001_Default/Kalium_BaseColor.png"))
            .unwrap()
            .to_rgba8();
        let exported_metal = image::open(export_dir.join("002_Metal/Kalium_BaseColor.png"))
            .unwrap()
            .to_rgba8();
        assert_eq!(exported_leather, leather_composite);
        assert_eq!(exported_metal, metal_before);
    }

    #[test]
    fn egui_uv_drag_commits_one_stroke_and_journal_restores_it() {
        let temp = TempProject::new();
        let baseline = Document::new(64);
        let project_id = baseline.project_id;
        project::save(&baseline, &temp.0).unwrap();
        let baseline_digest = project::file_digest(&temp.0).unwrap();

        let mut app = test_app();
        app.install_open_project(baseline, temp.0.clone(), false);
        app.uv_clipping = false;
        app.brush.radius = 5;
        app.brush.opacity = 1.0;
        app.brush.hardness = 1.0;
        app.brush.color = [210, 40, 70, 255];
        app.brush.tip =
            crate::paint::BrushTip::alpha(3, 3, vec![0, 0, 0, 0, 255, 0, 0, 0, 0]).unwrap();
        let ctx = egui::Context::default();
        app.refresh_textures(&ctx);
        run_canvas_frame(&mut app, &ctx, Canvas::UV, Vec::new());
        let canvas = canvas_rect(&ctx, Canvas::UV);
        let start = canvas.center();
        let moved = start + egui::vec2(28.0, 0.0);

        run_canvas_frame(
            &mut app,
            &ctx,
            Canvas::UV,
            vec![egui::Event::PointerMoved(start)],
        );
        assert!(app.pending_stroke.is_none());
        assert!(!app
            .doc
            .selected_layer()
            .expect("selected paint layer")
            .pixels
            .contains_key(&Channel::BaseColor));

        run_canvas_frame(
            &mut app,
            &ctx,
            Canvas::UV,
            vec![
                egui::Event::PointerMoved(start),
                egui::Event::PointerButton {
                    pos: start,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
        assert!(app.pending_stroke.is_some());
        assert!(app
            .doc
            .selected_layer()
            .expect("selected paint layer")
            .pixels[&Channel::BaseColor]
            .pixels()
            .any(|pixel| pixel.0[3] > 0));
        assert!(app.recovery_journal.is_some());

        run_canvas_frame(
            &mut app,
            &ctx,
            Canvas::UV,
            vec![egui::Event::PointerMoved(moved)],
        );
        assert!(app.pending_stroke.is_some());
        run_canvas_frame(
            &mut app,
            &ctx,
            Canvas::UV,
            vec![
                egui::Event::PointerMoved(moved),
                egui::Event::PointerButton {
                    pos: moved,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
        app.finish_stroke();

        assert!(app.pending_stroke.is_none());
        assert!(app.unsaved);
        assert!(app.recovery_journal.is_some());
        let painted = app.doc.composite(Channel::BaseColor);
        assert_eq!(
            crate::recovery::scan(
                recovery_journal_path(&temp.0),
                project_id,
                baseline_digest,
                1,
            )
            .unwrap()
            .len(),
            1,
            "one gesture should append one atomic journal record"
        );
        assert!(app.history.undo(&mut app.doc));
        assert!(!app
            .doc
            .selected_layer()
            .expect("selected paint layer")
            .pixels
            .contains_key(&Channel::BaseColor));
        assert!(
            !app.history.undo(&mut app.doc),
            "one gesture is one undo item"
        );
        assert!(app.history.redo(&mut app.doc));
        assert!(!app.history.redo(&mut app.doc));
        assert_eq!(app.doc.composite(Channel::BaseColor), painted);
        drop(app);

        let mut reopened = test_app();
        reopened.install_open_project(project::load(&temp.0).unwrap(), temp.0.clone(), false);
        assert!(
            reopened.pending_journal_recovery.is_some(),
            "status: {}; recovery: {:?}",
            reopened.status,
            reopened.recovery_unavailable
        );
        reopened.resolve_journal_recovery(JournalRecoveryChoice::Restore);
        assert!(reopened.unsaved);
        assert!(reopened.recovery_journal.is_some());
        assert!(reopened
            .doc
            .selected_layer()
            .expect("selected paint layer")
            .pixels
            .get(&Channel::BaseColor)
            .is_some_and(|image| image.pixels().any(|pixel| pixel.0[3] > 0)));
        assert_eq!(reopened.doc.composite(Channel::BaseColor), painted);
    }

    fn assert_canvas_drag_started_outside_does_not_paint(canvas_kind: Canvas) {
        let temp = TempProject::new();
        let baseline = Document::new(64);
        let project_id = baseline.project_id;
        project::save(&baseline, &temp.0).unwrap();
        let baseline_digest = project::file_digest(&temp.0).unwrap();

        let mut app = test_app();
        app.install_open_project(baseline, temp.0.clone(), false);
        app.uv_clipping = false;
        app.brush.radius = 3;
        app.brush.opacity = 1.0;
        app.brush.hardness = 1.0;
        app.brush.color = [220, 30, 50, 255];
        let ctx = egui::Context::default();
        app.refresh_textures(&ctx);
        run_canvas_frame(&mut app, &ctx, canvas_kind, Vec::new());
        let canvas = canvas_rect(&ctx, canvas_kind);
        let outside = egui::pos2(canvas.left() - 8.0, canvas.center().y);
        assert!(
            egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(640.0, 480.0)).contains(outside)
        );

        let target = match canvas_kind {
            Canvas::UV => canvas.center(),
            Canvas::Model => {
                let hit = (0..app.view.height)
                    .flat_map(|y| (0..app.view.width).map(move |x| (x, y)))
                    .find(|&(x, y)| app.view.pick_surface(x, y).is_some())
                    .expect("demo mesh should provide a pickable model pixel");
                egui::pos2(
                    canvas.left() + (hit.0 as f32 + 0.5) * canvas.width() / app.view.width as f32,
                    canvas.top() + (hit.1 as f32 + 0.5) * canvas.height() / app.view.height as f32,
                )
            }
        };
        let initial_paint_dirty = app.paint_dirty;
        let initial_model_dirty = app.model_dirty;

        run_canvas_frame(
            &mut app,
            &ctx,
            canvas_kind,
            vec![
                egui::Event::PointerMoved(outside),
                egui::Event::PointerButton {
                    pos: outside,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
        run_canvas_frame(
            &mut app,
            &ctx,
            canvas_kind,
            vec![egui::Event::PointerMoved(target)],
        );
        assert!(app.pending_stroke.is_none());
        assert!(!app.history.can_undo());
        assert!(!app.unsaved);
        assert_eq!(app.paint_dirty, initial_paint_dirty);
        assert_eq!(app.model_dirty, initial_model_dirty);
        assert!(app
            .doc
            .selected_layer()
            .expect("selected paint layer")
            .pixels
            .is_empty());
        assert!(app.recovery_journal.is_some());

        run_canvas_frame(
            &mut app,
            &ctx,
            canvas_kind,
            vec![
                egui::Event::PointerMoved(target),
                egui::Event::PointerButton {
                    pos: target,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
        assert!(app.pending_stroke.is_none());
        assert!(!app.history.can_undo());
        assert!(!app.unsaved);
        assert!(app
            .doc
            .selected_layer()
            .expect("selected paint layer")
            .pixels
            .is_empty());
        drop(app);

        assert!(crate::recovery::scan(
            recovery_journal_path(&temp.0),
            project_id,
            baseline_digest,
            1,
        )
        .unwrap()
        .is_empty());
    }

    #[test]
    fn egui_uv_canvas_does_not_paint_a_drag_started_outside() {
        assert_canvas_drag_started_outside_does_not_paint(Canvas::UV);
    }

    #[test]
    fn egui_model_canvas_does_not_paint_a_drag_started_outside() {
        assert_canvas_drag_started_outside_does_not_paint(Canvas::Model);
    }

    #[test]
    fn egui_uv_zoom_and_pan_keep_pointer_mapping_and_paint_target() {
        let temp = TempProject::new();
        let mut app = test_app();
        app.uv_clipping = false;
        app.brush.radius = 0;
        app.brush.opacity = 1.0;
        app.brush.hardness = 1.0;
        app.brush.color = [231, 37, 19, 255];
        let ctx = egui::Context::default();
        app.refresh_textures(&ctx);
        run_canvas_frame(&mut app, &ctx, Canvas::UV, Vec::new());

        let viewport = canvas_rect(&ctx, Canvas::UV);
        let image_before = app.uv_image_rect(viewport);
        let anchor_uv = [0.31, 0.42];
        let anchor = egui::pos2(
            image_before.left() + anchor_uv[0] * image_before.width(),
            image_before.top() + anchor_uv[1] * image_before.height(),
        );
        run_canvas_frame(
            &mut app,
            &ctx,
            Canvas::UV,
            vec![
                egui::Event::PointerMoved(anchor),
                egui::Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Point,
                    delta: egui::vec2(0.0, 300.0),
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
        assert!(app.uv_zoom > 1.0, "wheel over UV view should zoom in");
        let zoomed = app.uv_image_rect(canvas_rect(&ctx, Canvas::UV));
        let anchored_uv = [
            (anchor.x - zoomed.left()) / zoomed.width(),
            (anchor.y - zoomed.top()) / zoomed.height(),
        ];
        assert!((anchored_uv[0] - anchor_uv[0]).abs() < 0.001);
        assert!((anchored_uv[1] - anchor_uv[1]).abs() < 0.001);

        let pan_delta = egui::vec2(18.0, -12.0);
        let previous_pan = app.uv_pan;
        run_canvas_frame(
            &mut app,
            &ctx,
            Canvas::UV,
            vec![
                egui::Event::PointerMoved(anchor),
                egui::Event::PointerButton {
                    pos: anchor,
                    button: egui::PointerButton::Secondary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
        run_canvas_frame(
            &mut app,
            &ctx,
            Canvas::UV,
            vec![egui::Event::PointerMoved(anchor + pan_delta)],
        );
        let panned_viewport = canvas_rect(&ctx, Canvas::UV);
        let panned = app.uv_image_rect(panned_viewport);
        assert!((app.uv_pan - previous_pan - pan_delta).length() < 0.1);
        let pan_uv = [0.5, 0.5];
        let paint_at = egui::pos2(
            panned.left() + pan_uv[0] * panned.width(),
            panned.top() + pan_uv[1] * panned.height(),
        );
        assert!(panned_viewport.contains(paint_at));
        run_canvas_frame(
            &mut app,
            &ctx,
            Canvas::UV,
            vec![egui::Event::PointerButton {
                pos: anchor + pan_delta,
                button: egui::PointerButton::Secondary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );
        run_canvas_frame(
            &mut app,
            &ctx,
            Canvas::UV,
            vec![
                egui::Event::PointerMoved(paint_at),
                egui::Event::PointerButton {
                    pos: paint_at,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
        run_canvas_frame(
            &mut app,
            &ctx,
            Canvas::UV,
            vec![egui::Event::PointerButton {
                pos: paint_at,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );
        app.finish_stroke();

        let painted = app.doc.composite(Channel::BaseColor);
        assert_eq!(painted.get_pixel(32, 32).0, [231, 37, 19, 255]);
        project::save(&app.doc, &temp.0).unwrap();
        assert_eq!(
            project::load(&temp.0)
                .unwrap()
                .composite(Channel::BaseColor),
            painted
        );
    }

    #[test]
    fn egui_uv_stroke_reentry_after_leaving_canvas_does_not_bridge_gap() {
        let temp = TempProject::new();
        let baseline = Document::new(64);
        let project_id = baseline.project_id;
        project::save(&baseline, &temp.0).unwrap();
        let baseline_digest = project::file_digest(&temp.0).unwrap();

        let mut app = test_app();
        app.install_open_project(baseline, temp.0.clone(), false);
        app.uv_clipping = false;
        app.brush.radius = 2;
        app.brush.opacity = 1.0;
        app.brush.hardness = 1.0;
        app.brush.color = [220, 30, 50, 255];
        let ctx = egui::Context::default();
        app.refresh_textures(&ctx);
        run_canvas_frame(&mut app, &ctx, Canvas::UV, Vec::new());
        let canvas = canvas_rect(&ctx, Canvas::UV);
        let start = egui::pos2(canvas.left() + canvas.width() * 0.2, canvas.center().y);
        let outside = egui::pos2(canvas.left() - 8.0, canvas.center().y);
        let destination = egui::pos2(canvas.left() + canvas.width() * 0.8, canvas.center().y);

        run_canvas_frame(
            &mut app,
            &ctx,
            Canvas::UV,
            vec![
                egui::Event::PointerMoved(start),
                egui::Event::PointerButton {
                    pos: start,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
        assert!(app.pending_stroke.is_some());
        run_canvas_frame(
            &mut app,
            &ctx,
            Canvas::UV,
            vec![egui::Event::PointerMoved(outside)],
        );
        assert!(app.pending_stroke.is_some());
        assert!(app.previous_uv.is_none());
        run_canvas_frame(
            &mut app,
            &ctx,
            Canvas::UV,
            vec![egui::Event::PointerMoved(destination)],
        );
        assert!(app.pending_stroke.is_some());

        let painted = &app
            .doc
            .selected_layer()
            .expect("selected paint layer")
            .pixels[&Channel::BaseColor];
        assert!(painted.get_pixel(13, 32).0[3] > 0);
        assert!(painted.get_pixel(51, 32).0[3] > 0);
        assert_eq!(painted.get_pixel(32, 32).0[3], 0);

        run_canvas_frame(
            &mut app,
            &ctx,
            Canvas::UV,
            vec![
                egui::Event::PointerMoved(destination),
                egui::Event::PointerButton {
                    pos: destination,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
        app.finish_stroke();
        assert!(app.pending_stroke.is_none());
        assert!(app.history.can_undo());
        assert!(app.unsaved);
        assert!(app.recovery_journal.is_some());
        assert_eq!(
            crate::recovery::scan(
                recovery_journal_path(&temp.0),
                project_id,
                baseline_digest,
                1,
            )
            .unwrap()
            .len(),
            1,
            "leave/re-enter remains one stroke and one journal batch"
        );
        assert!(app.history.undo(&mut app.doc));
        assert!(!app.history.can_undo());
        assert!(!app
            .doc
            .selected_layer()
            .expect("selected paint layer")
            .pixels
            .contains_key(&Channel::BaseColor));
    }

    #[test]
    fn recovery_process_helper() {
        let Ok(mode) = std::env::var("KALIUM_RECOVERY_PROCESS_MODE") else {
            return;
        };
        let project_path = PathBuf::from(
            std::env::var_os("KALIUM_RECOVERY_PROCESS_PROJECT")
                .expect("helper project path is required"),
        );
        let expected_path = PathBuf::from(
            std::env::var_os("KALIUM_RECOVERY_PROCESS_EXPECTED")
                .expect("helper expected-pixels path is required"),
        );
        match mode.as_str() {
            "writer" => {
                let ready_path = PathBuf::from(
                    std::env::var_os("KALIUM_RECOVERY_PROCESS_READY")
                        .expect("writer ready path is required"),
                );
                let mut app = test_app();
                app.install_open_project(
                    project::load(&project_path).expect("load baseline project"),
                    project_path,
                    false,
                );
                app.uv_clipping = false;
                app.brush.radius = 5;
                app.brush.opacity = 1.0;
                app.brush.hardness = 1.0;
                app.brush.color = [210, 40, 70, 255];
                let ctx = egui::Context::default();
                app.refresh_textures(&ctx);
                run_canvas_frame(&mut app, &ctx, Canvas::UV, Vec::new());
                let point = canvas_rect(&ctx, Canvas::UV).center();
                run_canvas_frame(
                    &mut app,
                    &ctx,
                    Canvas::UV,
                    vec![
                        egui::Event::PointerMoved(point),
                        egui::Event::PointerButton {
                            pos: point,
                            button: egui::PointerButton::Primary,
                            pressed: true,
                            modifiers: egui::Modifiers::NONE,
                        },
                    ],
                );
                run_canvas_frame(
                    &mut app,
                    &ctx,
                    Canvas::UV,
                    vec![egui::Event::PointerButton {
                        pos: point,
                        button: egui::PointerButton::Primary,
                        pressed: false,
                        modifiers: egui::Modifiers::NONE,
                    }],
                );
                app.finish_stroke();
                assert!(app.recovery_journal.is_some());
                assert!(app.unsaved);
                fs::write(
                    expected_path,
                    app.doc
                        .selected_layer()
                        .expect("selected paint layer")
                        .pixels[&Channel::BaseColor]
                        .as_raw(),
                )
                .expect("write expected pixels");
                fs::write(ready_path, b"durable stroke journaled")
                    .expect("signal durable edit to parent");
                loop {
                    std::thread::park_timeout(std::time::Duration::from_secs(30));
                }
            }
            "reader" => {
                let baseline_digest = project::file_digest(&project_path).unwrap();
                let mut app = test_app();
                app.install_open_project(
                    project::load(&project_path).expect("load baseline project"),
                    project_path.clone(),
                    false,
                );
                assert!(app.pending_journal_recovery.is_some());
                app.resolve_journal_recovery(JournalRecoveryChoice::Restore);
                app.doc.validate().expect("restored document validates");
                let expected = fs::read(expected_path).expect("read expected pixels");
                assert_eq!(
                    app.doc
                        .selected_layer()
                        .expect("selected paint layer")
                        .pixels[&Channel::BaseColor]
                        .as_raw(),
                    &expected
                );
                assert!(app.unsaved);
                assert!(app.recovery_journal.is_some());
                assert_eq!(
                    project::file_digest(&project_path).unwrap(),
                    baseline_digest
                );
            }
            other => panic!("unknown recovery helper mode {other}"),
        }
    }

    #[test]
    fn incomplete_texture_import_process_helper() {
        let Ok(mode) = std::env::var("KALIUM_IMPORT_TRANSACTION_PROCESS_MODE") else {
            return;
        };
        let project_path = PathBuf::from(
            std::env::var_os("KALIUM_IMPORT_TRANSACTION_PROCESS_PROJECT")
                .expect("helper project path is required"),
        );
        match mode.as_str() {
            "writer" => {
                let ready_path = PathBuf::from(
                    std::env::var_os("KALIUM_IMPORT_TRANSACTION_PROCESS_READY")
                        .expect("writer ready path is required"),
                );
                let mut app = test_app();
                app.install_open_project(
                    project::load(&project_path).expect("load baseline project"),
                    project_path,
                    false,
                );
                let set_id = app.doc.active_set_id;
                let source_layer_id = app.doc.selected_layer().expect("selected paint layer").id;
                app.append_recovery_batch(&[DocumentEvent::LayerImagePatch {
                    set_id,
                    layer_id: source_layer_id,
                    target: LayerImageTarget::Channel(Channel::BaseColor),
                    x: 0,
                    y: 0,
                    width: 1,
                    height: 1,
                    bytes: vec![220, 35, 70, 255],
                }]);
                let imported_layer_id = app.doc.layer_tree.next_node_id;
                app.append_recovery_batch_inner(
                    &[DocumentEvent::TextureImportBegin {
                        set_id,
                        layer_id: imported_layer_id,
                        channel: Channel::Roughness,
                    }],
                    false,
                );
                app.append_recovery_batch_inner(
                    &[DocumentEvent::LayerCreate {
                        set_id,
                        layer_id: imported_layer_id,
                        to_index: 1,
                        name: "Imported Roughness".into(),
                    }],
                    false,
                );
                app.append_recovery_batch_inner(
                    &[DocumentEvent::LayerImagePatch {
                        set_id,
                        layer_id: imported_layer_id,
                        target: LayerImageTarget::Channel(Channel::Roughness),
                        x: 0,
                        y: 0,
                        width: 64,
                        height: 16,
                        bytes: vec![90; 64 * 16 * 4],
                    }],
                    false,
                );
                app.append_recovery_batch_inner(
                    &[DocumentEvent::LayerSelection {
                        set_id,
                        layer_id: imported_layer_id,
                    }],
                    false,
                );
                assert!(app.recovery_journal.is_some());
                fs::write(&ready_path, b"incomplete import transaction is durable")
                    .expect("signal durable transaction prefix to parent");
                loop {
                    std::thread::park_timeout(std::time::Duration::from_secs(30));
                }
            }
            "reader" => {
                let mut app = test_app();
                app.install_open_project(
                    project::load(&project_path).expect("load baseline project"),
                    project_path.clone(),
                    false,
                );
                assert!(app.pending_journal_recovery.is_some());
                app.resolve_journal_recovery(JournalRecoveryChoice::Restore);
                app.doc.validate().expect("restored document validates");
                assert_eq!(
                    app.doc.layer_tree.roots.len(),
                    1,
                    "partial import must be discarded"
                );
                assert_eq!(
                    app.doc
                        .selected_layer()
                        .expect("selected paint layer")
                        .pixels[&Channel::BaseColor]
                        .get_pixel(0, 0)
                        .0,
                    [220, 35, 70, 255],
                    "earlier committed edit remains recoverable"
                );
                assert!(!app
                    .doc
                    .selected_layer()
                    .expect("selected paint layer")
                    .pixels
                    .contains_key(&Channel::Roughness));
                assert!(app.recovery_journal.is_some());
                let records = recovery::scan(
                    recovery_journal_path(&project_path),
                    app.doc.project_id,
                    project::file_digest(&project_path).unwrap(),
                    1,
                )
                .expect("truncated journal scans");
                assert_eq!(
                    records.len(),
                    1,
                    "incomplete transaction is removed on open"
                );
            }
            other => panic!("unknown import transaction helper mode {other}"),
        }
    }

    #[test]
    fn incomplete_mesh_replace_process_helper() {
        let Ok(mode) = std::env::var("KALIUM_MESH_RECOVERY_PROCESS_MODE") else {
            return;
        };
        let project_path = PathBuf::from(
            std::env::var_os("KALIUM_MESH_RECOVERY_PROCESS_PROJECT")
                .expect("mesh recovery helper project path is required"),
        );
        match mode.as_str() {
            "writer" => {
                let mut app = test_app();
                app.install_open_project(
                    project::load(&project_path).expect("load mesh recovery baseline"),
                    project_path,
                    false,
                );
                let set_id = app.doc.active_set_id;
                let layer_id = app.doc.selected_layer().expect("selected paint layer").id;
                app.append_recovery_batch(&[DocumentEvent::LayerImagePatch {
                    set_id,
                    layer_id,
                    target: LayerImageTarget::Channel(Channel::BaseColor),
                    x: 0,
                    y: 0,
                    width: 1,
                    height: 1,
                    bytes: vec![220, 35, 70, 255],
                }]);

                let mut mesh = Mesh::demo_cube();
                mesh.extensions.insert(
                    "future_mesh_metadata".into(),
                    serde_json::Value::String("m".repeat(5 * 1024 * 1024)),
                );
                app.install_imported_mesh(mesh)
                    .expect("large mesh import proceeds after first durable chunk");
                panic!("test hook should park after the first durable mesh chunk");
            }
            "reader" => {
                let baseline_digest = project::file_digest(&project_path).unwrap();
                let mut app = test_app();
                app.install_open_project(
                    project::load(&project_path).expect("load mesh recovery baseline"),
                    project_path.clone(),
                    false,
                );
                assert!(app.pending_journal_recovery.is_some());
                app.resolve_journal_recovery(JournalRecoveryChoice::Restore);
                assert_eq!(app.doc.mesh.name, Mesh::demo_cube().name);
                assert_eq!(
                    app.doc
                        .selected_layer()
                        .expect("selected paint layer")
                        .pixels[&Channel::BaseColor]
                        .get_pixel(0, 0),
                    &image::Rgba([220, 35, 70, 255])
                );
                assert!(app.recovery_journal.is_some());
                assert_eq!(
                    project::file_digest(&project_path).unwrap(),
                    baseline_digest
                );
            }
            other => panic!("unknown mesh recovery helper mode {other}"),
        }
    }

    #[test]
    fn incomplete_snapshot_async_process_helper() {
        let Ok(mode) = std::env::var("KALIUM_SNAPSHOT_RECOVERY_PROCESS_MODE") else {
            return;
        };
        let project_path = PathBuf::from(
            std::env::var_os("KALIUM_SNAPSHOT_RECOVERY_PROCESS_PROJECT")
                .expect("snapshot recovery helper project path is required"),
        );
        match mode.as_str() {
            "writer" => {
                let mut app = test_app();
                app.install_open_project(
                    project::load(&project_path).expect("load snapshot recovery baseline"),
                    project_path.clone(),
                    false,
                );
                app.layer_command(LayerCommand::Add);
                let texture_path = project_path.with_extension("snapshot.png");
                image::RgbaImage::from_pixel(2048, 2048, image::Rgba([23, 47, 89, 255]))
                    .save(&texture_path)
                    .expect("write large snapshot texture");
                app.import_texture_path(&texture_path)
                    .expect("import large snapshot texture");
                app.layer_command(LayerCommand::Remove);
                project::save(
                    &app.doc,
                    &project_path.with_extension("snapshot.expected.kalium"),
                )
                .expect("write expected state");
                app.apply_history_impl(HistoryDirection::Undo, false);
                panic!("test hook should park after the first async snapshot batch");
            }
            "reader" => {
                let mut app = test_app();
                app.install_open_project(
                    project::load(&project_path).expect("load snapshot reader baseline"),
                    project_path.clone(),
                    false,
                );
                assert!(app.pending_journal_recovery.is_some());
                app.resolve_journal_recovery(JournalRecoveryChoice::Restore);
                let expected =
                    project::load(&project_path.with_extension("snapshot.expected.kalium"))
                        .unwrap();
                assert_eq!(
                    app.doc.layer_tree.roots.len(),
                    expected.layer_tree.roots.len()
                );
                assert_eq!(
                    app.doc
                        .layer_tree
                        .roots
                        .iter()
                        .map(|layer| layer.id)
                        .collect::<Vec<_>>(),
                    expected
                        .layer_tree
                        .roots
                        .iter()
                        .map(|layer| layer.id)
                        .collect::<Vec<_>>()
                );
                assert_eq!(
                    app.doc.selected_layer().expect("selected paint layer").name,
                    expected
                        .selected_layer()
                        .expect("selected paint layer")
                        .name
                );
            }
            other => panic!("unknown snapshot recovery helper mode {other}"),
        }
    }

    #[test]
    fn process_kill_discards_incomplete_async_snapshot_and_keeps_prior_edits() {
        let temp = TempProject::new();
        let ready_path = temp.0.with_extension("snapshot-ready");
        project::save(&Document::new(2048), &temp.0).unwrap();
        let journal_path = recovery_journal_path(&temp.0);
        let helper = || {
            let mut command = Command::new(std::env::current_exe().expect("test executable path"));
            command
                .args([
                    "--exact",
                    "app::unsaved_change_tests::incomplete_snapshot_async_process_helper",
                    "--nocapture",
                ])
                .env("KALIUM_SNAPSHOT_RECOVERY_PROCESS_PROJECT", &temp.0)
                .stdout(Stdio::null());
            command
        };

        let mut writer = helper()
            .env("KALIUM_SNAPSHOT_RECOVERY_PROCESS_MODE", "writer")
            .env("KALIUM_SNAPSHOT_RECOVERY_PROCESS_READY", &ready_path)
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn async snapshot writer child");
        let deadline = Instant::now() + Duration::from_secs(90);
        while !ready_path.exists() {
            if let Some(status) = writer.try_wait().expect("poll snapshot writer process") {
                panic!("snapshot writer exited before durable-ready signal: {status}");
            }
            if Instant::now() >= deadline {
                let _ = writer.kill();
                let output = writer
                    .wait_with_output()
                    .expect("collect failed snapshot writer output");
                panic!(
                    "snapshot writer did not signal readiness; stderr: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            fs::read(&ready_path).unwrap(),
            b"first async snapshot recovery batch synced"
        );
        writer
            .kill()
            .expect("terminate snapshot writer without app shutdown");
        assert!(!writer.wait().expect("reap killed snapshot child").success());

        let before_relaunch_len = fs::metadata(&journal_path).unwrap().len();
        let reader = helper()
            .env("KALIUM_SNAPSHOT_RECOVERY_PROCESS_MODE", "reader")
            .output()
            .expect("spawn snapshot recovery reader child");
        assert!(
            reader.status.success(),
            "snapshot recovery reader failed: {}",
            String::from_utf8_lossy(&reader.stderr)
        );
        assert!(fs::metadata(&journal_path).unwrap().len() < before_relaunch_len);
        let _ = fs::remove_file(temp.0.with_extension("snapshot.png"));
        let _ = fs::remove_file(temp.0.with_extension("snapshot.expected.kalium"));
    }

    #[test]
    fn process_kill_drops_incomplete_import_transaction_and_keeps_prior_edits() {
        let temp = TempProject::new();
        let ready_path = temp.0.with_extension("import-ready");
        project::save(&Document::new(64), &temp.0).unwrap();
        let journal_path = recovery_journal_path(&temp.0);
        let helper = || {
            let mut command = Command::new(std::env::current_exe().expect("test executable path"));
            command
                .args([
                    "--exact",
                    "app::unsaved_change_tests::incomplete_texture_import_process_helper",
                    "--nocapture",
                ])
                .env("KALIUM_IMPORT_TRANSACTION_PROCESS_PROJECT", &temp.0)
                .stdout(Stdio::null());
            command
        };

        let mut writer = helper()
            .env("KALIUM_IMPORT_TRANSACTION_PROCESS_MODE", "writer")
            .env("KALIUM_IMPORT_TRANSACTION_PROCESS_READY", &ready_path)
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn import transaction writer child");
        let deadline = Instant::now() + std::time::Duration::from_secs(20);
        let expected_ready = b"incomplete import transaction is durable";
        while fs::read(&ready_path).unwrap_or_default() != expected_ready {
            if let Some(status) = writer.try_wait().expect("poll writer process") {
                panic!("writer child exited before durable-ready signal: {status}");
            }
            if Instant::now() >= deadline {
                let _ = writer.kill();
                let output = writer
                    .wait_with_output()
                    .expect("collect failed writer output");
                panic!(
                    "writer child did not signal readiness; stderr: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(fs::read(&ready_path).unwrap(), expected_ready);
        writer
            .kill()
            .expect("terminate writer without app shutdown");
        let killed = writer.wait().expect("reap killed writer child");
        assert!(
            !killed.success(),
            "writer must be terminated before cleanup"
        );

        let before_relaunch_len = fs::metadata(&journal_path).unwrap().len();
        let reader = helper()
            .env("KALIUM_IMPORT_TRANSACTION_PROCESS_MODE", "reader")
            .output()
            .expect("spawn import transaction recovery child");
        assert!(
            reader.status.success(),
            "reader child failed: {}",
            String::from_utf8_lossy(&reader.stderr)
        );
        assert!(fs::metadata(&journal_path).unwrap().len() < before_relaunch_len);
    }

    #[test]
    fn process_kill_drops_incomplete_mesh_replace_and_keeps_prior_edits() {
        let temp = TempProject::new();
        let ready_path = temp.0.with_extension("mesh-ready");
        project::save(&Document::new(64), &temp.0).unwrap();
        let journal_path = recovery_journal_path(&temp.0);
        let helper = || {
            let mut command = Command::new(std::env::current_exe().expect("test executable path"));
            command
                .args([
                    "--exact",
                    "app::unsaved_change_tests::incomplete_mesh_replace_process_helper",
                    "--nocapture",
                ])
                .env("KALIUM_MESH_RECOVERY_PROCESS_PROJECT", &temp.0)
                .stdout(Stdio::null());
            command
        };

        let mut writer = helper()
            .env("KALIUM_MESH_RECOVERY_PROCESS_MODE", "writer")
            .env("KALIUM_MESH_RECOVERY_PROCESS_READY", &ready_path)
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn mesh recovery writer child");
        let deadline = Instant::now() + std::time::Duration::from_secs(20);
        while !ready_path.exists() {
            if let Some(status) = writer.try_wait().expect("poll mesh writer process") {
                panic!("mesh writer child exited before durable-ready signal: {status}");
            }
            if Instant::now() >= deadline {
                let _ = writer.kill();
                let output = writer
                    .wait_with_output()
                    .expect("collect failed mesh writer output");
                panic!(
                    "mesh writer child did not signal readiness; stderr: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(
            fs::read(&ready_path).unwrap(),
            b"first async mesh recovery chunk synced"
        );
        writer
            .kill()
            .expect("terminate mesh writer without app shutdown");
        let killed = writer.wait().expect("reap killed mesh writer child");
        assert!(
            !killed.success(),
            "mesh writer must be terminated before cleanup"
        );

        let before_relaunch_len = fs::metadata(&journal_path).unwrap().len();
        let reader = helper()
            .env("KALIUM_MESH_RECOVERY_PROCESS_MODE", "reader")
            .output()
            .expect("spawn mesh recovery reader child");
        assert!(
            reader.status.success(),
            "mesh recovery reader failed: {}",
            String::from_utf8_lossy(&reader.stderr)
        );
        assert!(fs::metadata(&journal_path).unwrap().len() < before_relaunch_len);
    }

    #[test]
    fn process_kill_then_relaunch_restores_durable_prefix_after_torn_tail() {
        let temp = TempProject::new();
        let expected_path = temp.0.with_extension("expected");
        let ready_path = temp.0.with_extension("ready");
        project::save(&Document::new(64), &temp.0).unwrap();
        let baseline_digest = project::file_digest(&temp.0).unwrap();
        let journal_path = recovery_journal_path(&temp.0);
        let helper = || {
            let mut command = Command::new(std::env::current_exe().expect("test executable path"));
            command
                .args([
                    "--exact",
                    "app::unsaved_change_tests::recovery_process_helper",
                    "--nocapture",
                ])
                .env("KALIUM_RECOVERY_PROCESS_PROJECT", &temp.0)
                .env("KALIUM_RECOVERY_PROCESS_EXPECTED", &expected_path)
                .stdout(Stdio::null());
            command
        };

        let mut writer = helper()
            .env("KALIUM_RECOVERY_PROCESS_MODE", "writer")
            .env("KALIUM_RECOVERY_PROCESS_READY", &ready_path)
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn recovery writer child");
        let deadline = Instant::now() + std::time::Duration::from_secs(20);
        while !ready_path.exists() {
            if let Some(status) = writer.try_wait().expect("poll writer process") {
                panic!("writer child exited before durable-ready signal: {status}");
            }
            if Instant::now() >= deadline {
                let _ = writer.kill();
                let output = writer
                    .wait_with_output()
                    .expect("collect failed writer output");
                panic!(
                    "writer child did not signal readiness; stderr: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(fs::read(&ready_path).unwrap(), b"durable stroke journaled");
        writer
            .kill()
            .expect("terminate writer without app shutdown");
        let killed = writer.wait().expect("reap killed writer child");
        assert!(
            !killed.success(),
            "writer must be terminated before cleanup"
        );

        let valid_len = fs::metadata(&journal_path).unwrap().len();
        {
            let mut journal = fs::OpenOptions::new()
                .append(true)
                .open(&journal_path)
                .unwrap();
            use std::io::Write as _;
            journal.write_all(&[32, 0, 0, 0, 0xAA]).unwrap();
            journal.sync_all().unwrap();
        }
        let reader = helper()
            .env("KALIUM_RECOVERY_PROCESS_MODE", "reader")
            .output()
            .expect("spawn recovery reader child");
        assert!(
            reader.status.success(),
            "reader child failed: {}",
            String::from_utf8_lossy(&reader.stderr)
        );
        assert_eq!(fs::metadata(&journal_path).unwrap().len(), valid_len);
        assert_eq!(project::file_digest(&temp.0).unwrap(), baseline_digest);
    }

    #[test]
    fn egui_model_paint_auto_switch_is_journaled_before_the_stroke() {
        let temp = TempProject::new();
        let mut baseline = Document::new(64);
        let active_set_id = baseline.active_set_id;
        let bound_set_id = baseline.add_texture_set("Bound Material").unwrap();
        baseline.material_bindings = vec![Some(bound_set_id)];
        baseline.activate_texture_set(active_set_id).unwrap();
        project::save(&baseline, &temp.0).unwrap();
        let project_id = baseline.project_id;
        let baseline_digest = project::file_digest(&temp.0).unwrap();

        let mut app = test_app();
        app.install_open_project(baseline, temp.0.clone(), false);
        app.uv_clipping = false;
        app.brush.radius = 5;
        app.brush.opacity = 1.0;
        app.brush.hardness = 1.0;
        app.brush.color = [30, 170, 80, 255];
        let ctx = egui::Context::default();
        run_canvas_frame(&mut app, &ctx, Canvas::Model, Vec::new());
        let canvas = canvas_rect(&ctx, Canvas::Model);

        let hit = (0..app.view.height)
            .flat_map(|y| (0..app.view.width).map(move |x| (x, y)))
            .find(|&(x, y)| {
                app.view.pick_material_slot(x, y) == Some(0) && app.view.pick(x, y).is_some()
            })
            .expect("demo cube should provide a pickable pixel for its material slot");
        let position = egui::pos2(
            canvas.left() + (hit.0 as f32 + 0.5) * canvas.width() / app.view.width as f32,
            canvas.top() + (hit.1 as f32 + 0.5) * canvas.height() / app.view.height as f32,
        );

        run_canvas_frame(
            &mut app,
            &ctx,
            Canvas::Model,
            vec![
                egui::Event::PointerMoved(position),
                egui::Event::PointerButton {
                    pos: position,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
        assert_eq!(app.doc.active_set_id, bound_set_id);
        assert!(app.pending_stroke.is_some());
        assert!(app.recovery_journal.is_some());
        assert!(!recovery_journal_disabled_path(&recovery_journal_path(&temp.0)).exists());

        let moved = position + egui::vec2(9.0, 0.0);
        run_canvas_frame(
            &mut app,
            &ctx,
            Canvas::Model,
            vec![egui::Event::PointerMoved(moved)],
        );
        run_canvas_frame(
            &mut app,
            &ctx,
            Canvas::Model,
            vec![
                egui::Event::PointerMoved(moved),
                egui::Event::PointerButton {
                    pos: moved,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
        app.finish_stroke();
        assert_eq!(app.doc.active_set_id, bound_set_id);
        assert!(app.recovery_journal.is_some());
        let records = crate::recovery::scan(
            recovery_journal_path(&temp.0),
            project_id,
            baseline_digest,
            1,
        )
        .unwrap();
        assert_eq!(
            records.len(),
            2,
            "switch and stroke are separately journaled"
        );
        let selection = crate::recovery::decode_event_batch_bounded(&records[0].payload).unwrap();
        assert!(
            matches!(
                selection.first(),
                Some(crate::recovery::DocumentEvent::TextureSetSelect { set_id }) if *set_id == bound_set_id
            ),
            "texture-set selection must lead the first recovery batch: {selection:?}"
        );
        assert!(
            selection.iter().all(|event| matches!(
                event,
                crate::recovery::DocumentEvent::TextureSetSelect { set_id }
                    | crate::recovery::DocumentEvent::LayerSelection { set_id, .. }
                    if *set_id == bound_set_id
            )),
            "unexpected events bundled with auto-selection: {selection:?}"
        );
        let stroke = crate::recovery::decode_event_batch_bounded(&records[1].payload).unwrap();
        assert!(stroke.iter().any(|event| matches!(
            event,
            crate::recovery::DocumentEvent::LayerImagePatch { set_id, .. }
                if *set_id == bound_set_id
        )));
        drop(app);

        let mut reopened = test_app();
        reopened.install_open_project(project::load(&temp.0).unwrap(), temp.0.clone(), false);
        assert!(
            reopened.pending_journal_recovery.is_some(),
            "{}; recovery unavailable: {:?}",
            reopened.status,
            reopened.recovery_unavailable
        );
        reopened.resolve_journal_recovery(JournalRecoveryChoice::Restore);
        assert_eq!(reopened.doc.active_set_id, bound_set_id);
        assert!(reopened
            .doc
            .selected_layer()
            .expect("selected paint layer")
            .pixels
            .get(&Channel::BaseColor)
            .is_some_and(|image| image.pixels().any(|pixel| pixel.0[3] > 0)));
        assert!(reopened.recovery_journal.is_some());
    }

    #[test]
    fn imported_texture_layer_recovers_name_channel_pixels_and_selection() {
        let temp = TempProject::new();
        let texture_path = temp.0.with_extension("png");
        let imported_name = texture_path
            .file_stem()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let mut source = image::RgbaImage::new(16, 16);
        for (x, y, pixel) in source.enumerate_pixels_mut() {
            *pixel = image::Rgba([(x * 11) as u8, (y * 13) as u8, 77, 255]);
        }
        source.save(&texture_path).unwrap();

        let baseline = Document::new(1024);
        let original_layer_id = baseline.selected_layer().expect("selected paint layer").id;
        project::save(&baseline, &temp.0).unwrap();
        let project_id = baseline.project_id;
        let baseline_digest = project::file_digest(&temp.0).unwrap();
        let mut app = test_app();
        app.install_open_project(baseline, temp.0.clone(), false);
        app.channel = Channel::Roughness;
        app.import_texture_path(&texture_path).unwrap();
        let imported = app.doc.selected_layer().expect("selected paint layer");
        assert_eq!(imported.name, imported_name);
        assert_ne!(imported.id, original_layer_id);
        assert_eq!(
            imported.pixels[&Channel::Roughness].dimensions(),
            (1024, 1024)
        );
        let expected_pixels = imported.pixels[&Channel::Roughness].as_raw().clone();
        assert!(app.recovery_journal.is_some());
        assert!(app.unsaved);

        let records = crate::recovery::scan(
            recovery_journal_path(&temp.0),
            project_id,
            baseline_digest,
            1,
        )
        .unwrap();
        assert_eq!(
            records.len(),
            20,
            "transaction markers, create, 16 strips, selection, and commit"
        );
        let begin = crate::recovery::decode_event_batch_bounded(&records[0].payload).unwrap();
        assert!(matches!(
            begin.first(),
            Some(crate::recovery::DocumentEvent::TextureImportBegin { set_id, layer_id, channel })
                if *set_id == app.doc.active_set_id && *layer_id == imported.id && *channel == Channel::Roughness
        ));
        let create = crate::recovery::decode_event_batch_bounded(&records[1].payload).unwrap();
        assert!(matches!(
            create.first(),
            Some(crate::recovery::DocumentEvent::LayerCreate { layer_id, name, .. })
                if *layer_id == imported.id && name == &imported_name
        ));
        let mut next_y = 0;
        for record in records.iter().skip(2).take(16) {
            let patch = crate::recovery::decode_event_batch_bounded(&record.payload).unwrap();
            let image_patch = patch.iter().find_map(|event| match event {
                crate::recovery::DocumentEvent::LayerImagePatch {
                    layer_id,
                    target: LayerImageTarget::Channel(Channel::Roughness),
                    y,
                    height,
                    width,
                    bytes,
                    ..
                } if *layer_id == imported.id => Some((*y, *height, *width, bytes.len())),
                _ => None,
            });
            let (y, height, width, bytes) = image_patch.expect("row-strip image patch");
            assert_eq!(y, next_y);
            assert_eq!(width, 1024);
            assert!(bytes <= RECOVERY_IMAGE_PATCH_BYTES);
            next_y += height;
        }
        assert_eq!(next_y, 1024);
        assert!(matches!(
            crate::recovery::decode_event_batch_bounded(&records[18].payload)
                .unwrap()
                .first(),
            Some(crate::recovery::DocumentEvent::LayerSelection { set_id, layer_id })
                if *set_id == app.doc.active_set_id && *layer_id == imported.id
        ));
        let commit = crate::recovery::decode_event_batch_bounded(&records[19].payload).unwrap();
        assert!(matches!(
            commit.first(),
            Some(crate::recovery::DocumentEvent::TextureImportCommit { set_id, layer_id, channel })
                if *set_id == app.doc.active_set_id && *layer_id == imported.id && *channel == Channel::Roughness
        ));
        drop(app);

        let mut reopened = test_app();
        reopened.install_open_project(project::load(&temp.0).unwrap(), temp.0.clone(), false);
        assert!(reopened.pending_journal_recovery.is_some());
        reopened.resolve_journal_recovery(JournalRecoveryChoice::Restore);
        let restored = reopened.doc.selected_layer().expect("selected paint layer");
        assert_eq!(restored.name, imported_name);
        assert_ne!(restored.id, original_layer_id);
        let restored_image = &restored.pixels[&Channel::Roughness];
        assert_eq!(restored_image.dimensions(), (1024, 1024));
        assert_eq!(restored_image.get_pixel(0, 0).0, [0, 0, 77, 255]);
        assert_eq!(restored_image.as_raw(), &expected_pixels);
        assert!(reopened.recovery_journal.is_some());
        let _ = fs::remove_file(texture_path);
    }

    #[test]
    fn imported_texture_journal_cap_failure_disables_partial_recovery() {
        let temp = TempProject::new();
        let texture_path = temp.0.with_extension("png");
        image::RgbaImage::from_pixel(16, 16, image::Rgba([35, 80, 145, 255]))
            .save(&texture_path)
            .unwrap();

        let baseline = Document::new(64);
        let project_id = baseline.project_id;
        project::save(&baseline, &temp.0).unwrap();
        let baseline_digest = project::file_digest(&temp.0).unwrap();
        let mut app = test_app();
        app.install_open_project(baseline, temp.0.clone(), false);
        app.stop_recovery_writer().unwrap();
        let (mut journal, _) = Journal::open(
            recovery_journal_path(&temp.0),
            project_id,
            baseline_digest,
            JOURNAL_SCHEMA,
        )
        .unwrap();
        journal.set_remaining_capacity_for_test(10 * 1024);
        app.recovery_journal = Some(
            JournalWriter::new(
                journal,
                super::RECOVERY_WRITER_QUEUE_ITEMS,
                super::RECOVERY_WRITER_QUEUE_BYTES,
            )
            .unwrap(),
        );

        // The transaction markers and LayerCreate fit, while the image patch
        // does not. This simulates a journal near its 256-MiB cap.
        app.import_texture_path(&texture_path).unwrap();
        assert!(app
            .doc
            .selected_layer()
            .expect("selected paint layer")
            .pixels
            .contains_key(&Channel::BaseColor));
        assert!(app.recovery_journal.is_none());
        let journal_path = recovery_journal_path(&temp.0);
        assert!(recovery_journal_disabled_path(&journal_path).exists());
        let records = crate::recovery::scan(journal_path, project_id, baseline_digest, 1).unwrap();
        assert_eq!(
            records.len(),
            2,
            "begin and create are durable before the rejected patch"
        );
        assert_eq!(
            crate::recovery::committed_record_count(&records).unwrap(),
            0
        );
        drop(app);

        let mut reopened = test_app();
        reopened.install_open_project(project::load(&temp.0).unwrap(), temp.0.clone(), false);
        assert!(reopened.pending_journal_recovery.is_none());
        assert!(reopened.recovery_journal.is_none());
        assert!(reopened.status.contains("recovery is paused"));
        let _ = fs::remove_file(texture_path);
    }

    #[test]
    fn successful_project_save_is_added_to_persistent_activity_history() {
        let temp = TempShelf::new();
        let log_path = temp.path().join("activity-v1.json");
        let mut app = test_app();
        app.activity_log = Some(ActivityLog::load(&log_path).unwrap());
        let project_path = temp.path().join("artist-work.kalium");

        let message = app.save_project_to_path(project_path.clone()).unwrap();

        assert!(message.starts_with("Saved "));
        let entries: Vec<_> = ActivityLog::load(log_path)
            .unwrap()
            .entries_newest_first()
            .cloned()
            .collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].level, ActivityLevel::Info);
        assert_eq!(entries[0].category, "Project");
        assert!(entries[0].message.contains("artist-work.kalium"));
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
            .expect("selected paint layer")
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
            .expect("selected paint layer")
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
        let expected_channel = app
            .doc
            .selected_layer()
            .expect("selected paint layer")
            .pixels[&Channel::BaseColor]
            .clone();
        let expected_mask = app
            .doc
            .selected_layer()
            .expect("selected paint layer")
            .mask
            .clone();
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
            reopened
                .doc
                .selected_layer()
                .expect("selected paint layer")
                .pixels[&Channel::BaseColor],
            expected_channel
        );
        assert_eq!(
            reopened
                .doc
                .selected_layer()
                .expect("selected paint layer")
                .mask,
            expected_mask
        );
        assert!(reopened.unsaved);
        assert!(reopened.recovery_journal.is_some());
    }

    #[test]
    fn oversized_flood_fill_is_canceled_before_document_or_history_changes() {
        let mut app = test_app();
        let selected_before = app.doc.selected_layer().expect("selected paint layer").id;
        app.doc.add_layer().unwrap();
        let first_added_layer = app.doc.selected_layer().expect("selected paint layer").id;
        app.history
            .record_added_layer(&app.doc, first_added_layer, selected_before, None);
        let selected_before_second = first_added_layer;
        app.doc.add_layer().unwrap();
        let second_added_layer = app.doc.selected_layer().expect("selected paint layer").id;
        app.history
            .record_added_layer(&app.doc, second_added_layer, selected_before_second, None);
        assert!(app.history.undo(&mut app.doc));
        assert!(app.history.can_undo());
        assert!(app.history.can_redo());

        // The guard runs before image capture or flood-fill allocation. Using
        // the larger logical size here avoids allocating a 4K document in the test.
        app.doc.size = 4096;
        app.brush.mode = BrushMode::Bucket;
        app.brush.color = [180, 40, 20, 255];
        let prior_layer_id = app.doc.selected_layer().expect("selected paint layer").id;
        let prior_dirty = (app.unsaved, app.paint_dirty, app.model_dirty);

        app.fill_uv([0.5, 0.5]);

        assert_eq!(
            app.doc.selected_layer().expect("selected paint layer").id,
            prior_layer_id
        );
        assert!(app
            .doc
            .selected_layer()
            .expect("selected paint layer")
            .pixels
            .is_empty());
        assert_eq!((app.unsaved, app.paint_dirty, app.model_dirty), prior_dirty);
        assert!(app.history.can_undo());
        assert!(app.history.can_redo());
        assert!(app.status.contains("canceled"));
        app.doc.size = 64;
    }

    #[test]
    fn uniform_fill_undo_redo_and_recovery_preserve_present_and_absent_maps() {
        let temp = TempProject::new();
        let mut baseline = Document::new(64);
        let before_base: Vec<u8> = (0..64 * 64 * 4).map(|byte| (byte % 239) as u8).collect();
        baseline
            .selected_layer_mut()
            .expect("selected paint layer")
            .pixels
            .insert(
                Channel::BaseColor,
                image::RgbaImage::from_raw(64, 64, before_base.clone()).unwrap(),
            );
        let project_id = baseline.project_id;
        project::save(&baseline, &temp.0).unwrap();
        let baseline_digest = project::file_digest(&temp.0).unwrap();

        let mut app = test_app();
        app.install_open_project(baseline, temp.0.clone(), false);
        app.brush.color = [19, 53, 117, 255];
        app.perform(Action::Fill);
        let expected_base = app
            .doc
            .selected_layer()
            .expect("selected paint layer")
            .pixels[&Channel::BaseColor]
            .clone();
        app.apply_history(HistoryDirection::Undo);
        assert_eq!(
            app.doc
                .selected_layer()
                .expect("selected paint layer")
                .pixels[&Channel::BaseColor]
                .as_raw(),
            &before_base
        );
        app.apply_history(HistoryDirection::Redo);

        app.channel = Channel::Roughness;
        app.brush.grayscale = 83;
        app.perform(Action::Fill);
        assert!(app
            .doc
            .selected_layer()
            .expect("selected paint layer")
            .pixels
            .contains_key(&Channel::Roughness));
        app.apply_history(HistoryDirection::Undo);
        assert!(!app
            .doc
            .selected_layer()
            .expect("selected paint layer")
            .pixels
            .contains_key(&Channel::Roughness));
        app.apply_history(HistoryDirection::Redo);
        let expected_roughness = app
            .doc
            .selected_layer()
            .expect("selected paint layer")
            .pixels[&Channel::Roughness]
            .clone();
        drop(app);

        let records = crate::recovery::scan(
            recovery_journal_path(&temp.0),
            project_id,
            baseline_digest,
            1,
        )
        .unwrap();
        let events: Vec<_> = records
            .iter()
            .flat_map(|record| {
                crate::recovery::decode_event_batch_bounded(&record.payload).unwrap()
            })
            .collect();
        assert!(events.iter().any(|event| matches!(
            event,
            DocumentEvent::LayerImageFill {
                channel: Channel::BaseColor,
                ..
            }
        )));
        assert!(events
            .iter()
            .any(|event| matches!(event, DocumentEvent::LayerSnapshotBegin { .. })));
        assert!(events.iter().any(|event| matches!(
            event,
            DocumentEvent::LayerChannelRemove {
                channel: Channel::Roughness,
                ..
            }
        )));

        let mut reopened = test_app();
        reopened.install_open_project(project::load(&temp.0).unwrap(), temp.0.clone(), false);
        assert!(reopened.pending_journal_recovery.is_some());
        reopened.resolve_journal_recovery(JournalRecoveryChoice::Restore);
        assert_eq!(
            reopened.status,
            "Unsaved edits restored from the recovery journal"
        );
        assert_eq!(
            reopened
                .doc
                .selected_layer()
                .expect("selected paint layer")
                .pixels[&Channel::BaseColor],
            expected_base
        );
        assert_eq!(
            reopened
                .doc
                .selected_layer()
                .expect("selected paint layer")
                .pixels[&Channel::Roughness],
            expected_roughness
        );
        assert!(reopened.unsaved);
    }

    #[test]
    fn unsupported_project_edit_blocks_partial_journal_recovery() {
        let temp = TempProject::new();
        let baseline = Document::new(64);
        project::save(&baseline, &temp.0).unwrap();

        let mut app = test_app();
        app.install_open_project(baseline, temp.0.clone(), false);
        let added_set_id = app.doc.add_texture_set("New Texture Set").unwrap();
        app.change();
        assert_eq!(app.doc.texture_set_count(), 2);
        assert_eq!(app.doc.active_set_id, added_set_id);
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
    fn failed_disabled_marker_invalidates_residual_journal_before_relaunch() {
        let temp = TempProject::new();
        let baseline = Document::new(64);
        project::save(&baseline, &temp.0).unwrap();
        let journal_path = recovery_journal_path(&temp.0);
        let disabled_path = recovery_journal_disabled_path(&journal_path);

        let mut app = test_app();
        app.install_open_project(baseline, temp.0.clone(), false);
        app.append_recovery_batch(&[crate::recovery::DocumentEvent::LayerOpacity {
            set_id: app.doc.active_set_id,
            layer_id: app.doc.selected_layer().expect("selected paint layer").id,
            opacity: 0.43,
        }]);
        fs::create_dir(&disabled_path).unwrap();
        app.disable_recovery_journal("injected unsupported edit");
        assert!(app.recovery_journal.is_none());
        assert!(app.status.contains("header was invalidated"));
        assert!(journal_path.is_file());
        assert_eq!(fs::read(&journal_path).unwrap()[0], 0);
        fs::remove_dir(&disabled_path).unwrap();
        drop(app);

        let mut reopened = test_app();
        reopened.install_open_project(project::load(&temp.0).unwrap(), temp.0.clone(), false);
        assert!(reopened.pending_journal_recovery.is_none());
        assert!(reopened.recovery_journal.is_none());
        assert!(reopened.recovery_unavailable.is_some());
        assert!(reopened.status.contains("journal was rejected"));
    }

    #[test]
    fn duplicated_populated_layer_recovers_after_reopen_and_keeps_journal_enabled() {
        let temp = TempProject::new();
        let mut baseline = Document::new(64);
        baseline.add_layer().unwrap();
        let source_id = baseline.selected_layer().expect("selected paint layer").id;
        let size = baseline.size;
        {
            let source = baseline.selected_layer_mut().expect("selected paint layer");
            source.pixels_mut(Channel::BaseColor, size).put_pixel(
                5,
                7,
                image::Rgba([17, 34, 51, 255]),
            );
            source.pixels_mut(Channel::Roughness, size).put_pixel(
                9,
                11,
                image::Rgba([87, 0, 0, 255]),
            );
            source.mask.put_pixel(13, 15, image::Luma([123]));
            source.visible = false;
            source.opacity = 0.625;
            source.blend = crate::document::BlendMode::Multiply;
            source.disabled_channels.insert(Channel::Metallic);
            source
                .extensions
                .insert("future".into(), serde_json::json!({ "preserve": true }));
        }
        let duplicate_id = baseline.layer_tree.next_node_id;
        let expected_source = baseline
            .selected_layer()
            .expect("selected paint layer")
            .clone();
        project::save(&baseline, &temp.0).unwrap();
        let project_id = baseline.project_id;
        let baseline_digest = project::file_digest(&temp.0).unwrap();

        let mut app = test_app();
        app.install_open_project(baseline, temp.0.clone(), false);
        assert!(app.recovery_journal.is_some());
        app.layer_command(LayerCommand::Duplicate);

        assert_eq!(app.doc.layer_tree.roots.len(), 3);
        assert_eq!(
            app.doc.selected_layer().expect("selected paint layer").id,
            duplicate_id
        );
        assert_eq!(
            app.doc.selected_layer().expect("selected paint layer").name,
            "Paint Layer 2 copy"
        );
        assert_eq!(app.doc.layer_tree.next_node_id, duplicate_id + 1);
        assert_eq!(
            app.doc
                .selected_layer()
                .expect("selected paint layer")
                .pixels,
            expected_source.pixels
        );
        assert_eq!(
            app.doc.selected_layer().expect("selected paint layer").mask,
            expected_source.mask
        );
        assert_eq!(
            app.doc
                .selected_layer()
                .expect("selected paint layer")
                .visible,
            expected_source.visible
        );
        assert_eq!(
            app.doc
                .selected_layer()
                .expect("selected paint layer")
                .opacity,
            expected_source.opacity
        );
        assert_eq!(
            app.doc
                .selected_layer()
                .expect("selected paint layer")
                .blend,
            expected_source.blend
        );
        assert_eq!(
            app.doc
                .selected_layer()
                .expect("selected paint layer")
                .disabled_channels,
            expected_source.disabled_channels
        );
        assert_eq!(
            app.doc
                .selected_layer()
                .expect("selected paint layer")
                .extensions,
            expected_source.extensions
        );
        assert_eq!(
            app.doc.selected_layer().expect("selected paint layer").id - 1,
            source_id
        );
        assert!(app.recovery_journal.is_some());
        assert!(app.unsaved);
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
            1,
            "layer duplication appends one compact event"
        );

        let mut reopened = test_app();
        reopened.install_open_project(project::load(&temp.0).unwrap(), temp.0.clone(), false);
        assert!(reopened.pending_journal_recovery.is_some());
        reopened.resolve_journal_recovery(JournalRecoveryChoice::Restore);

        let duplicate = reopened.doc.selected_layer().expect("selected paint layer");
        assert_eq!(reopened.doc.layer_tree.roots.len(), 3);
        assert_eq!(duplicate.id, duplicate_id);
        assert_eq!(duplicate.name, "Paint Layer 2 copy");
        assert_eq!(duplicate.pixels, expected_source.pixels);
        assert_eq!(duplicate.mask, expected_source.mask);
        assert_eq!(duplicate.visible, expected_source.visible);
        assert_eq!(duplicate.opacity, expected_source.opacity);
        assert_eq!(duplicate.blend, expected_source.blend);
        assert_eq!(
            duplicate.disabled_channels,
            expected_source.disabled_channels
        );
        assert_eq!(duplicate.extensions, expected_source.extensions);
        assert_eq!(reopened.doc.layer_tree.next_node_id, duplicate_id + 1);
        assert!(reopened.unsaved);
        assert!(reopened.recovery_journal.is_some());
    }

    #[test]
    fn duplicated_nested_group_recovers_after_reopen_with_fresh_node_ids() {
        use crate::document::Layer;
        use crate::layer_tree::{FillLayer, LayerGroup, LayerNode, LayerNodeProperties};

        let temp = TempProject::new();
        let mut baseline = Document::new(64);
        let group_id = baseline.layer_tree.next_node_id;
        let fill_id = group_id + 1;
        let paint_id = fill_id + 1;
        let mut fill_properties =
            LayerNodeProperties::new(fill_id, "Rough fill", baseline.size).unwrap();
        fill_properties.mask.put_pixel(3, 8, image::Luma([117]));
        let fill = LayerNode::Fill(FillLayer {
            properties: fill_properties,
            values: [(Channel::Roughness, [73, 0, 0, 255])]
                .into_iter()
                .collect(),
        });
        let mut paint = Layer::new(paint_id, "Base paint", baseline.size);
        paint
            .pixels_mut(Channel::BaseColor, baseline.size)
            .put_pixel(9, 12, image::Rgba([13, 29, 47, 255]));
        paint.mask.put_pixel(15, 18, image::Luma([201]));
        let mut group_properties =
            LayerNodeProperties::new(group_id, "Asset group", baseline.size).unwrap();
        group_properties.opacity = 0.7;
        group_properties.mask.put_pixel(4, 6, image::Luma([89]));
        let group = LayerNode::Group(LayerGroup {
            properties: group_properties,
            children: vec![fill, LayerNode::Paint(paint)],
        });
        baseline.layer_tree.insert(None, 1, group).unwrap();
        baseline.select_node(group_id).unwrap();
        let duplicate_id = baseline.layer_tree.next_node_id;
        let project_id = baseline.project_id;
        project::save(&baseline, &temp.0).unwrap();
        let baseline_digest = project::file_digest(&temp.0).unwrap();

        let mut app = test_app();
        app.install_open_project(baseline, temp.0.clone(), false);
        assert!(app.recovery_journal.is_some());
        let ctx = egui::Context::default();
        let screen_rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(640.0, 900.0));
        let frame = |app: &mut KaliumApp, events| {
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
        frame(&mut app, Vec::new());
        let duplicate_button = ctx
            .data(|data| data.get_temp::<egui::Rect>(egui::Id::new("test-layer-action-Duplicate")))
            .expect("Duplicate button should be visible for the selected Group")
            .center();
        for (pressed, position) in [(true, duplicate_button), (false, duplicate_button)] {
            frame(
                &mut app,
                vec![
                    egui::Event::PointerMoved(position),
                    egui::Event::PointerButton {
                        pos: position,
                        button: egui::PointerButton::Primary,
                        pressed,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
            );
        }

        assert_eq!(app.doc.layer_tree.selected_node_id, duplicate_id);
        assert_eq!(
            app.doc.layer_tree.all_node_ids(),
            [
                1,
                group_id,
                fill_id,
                paint_id,
                duplicate_id,
                duplicate_id + 1,
                duplicate_id + 2
            ]
        );
        let LayerNode::Group(copy) = app.doc.layer_tree.find_node(duplicate_id).unwrap() else {
            panic!("duplicated node should remain a Group");
        };
        assert_eq!(copy.properties.name, "Asset group copy");
        assert_eq!(copy.properties.opacity, 0.7);
        assert_eq!(copy.properties.mask.get_pixel(4, 6).0, [89]);
        assert_eq!(copy.children[0].id(), duplicate_id + 1);
        assert_eq!(copy.children[1].id(), duplicate_id + 2);
        let fill_duplicate_id = duplicate_id + 3;
        app.doc.select_node(duplicate_id + 1).unwrap();
        frame(&mut app, Vec::new());
        let duplicate_button = ctx
            .data(|data| data.get_temp::<egui::Rect>(egui::Id::new("test-layer-action-Duplicate")))
            .expect("Duplicate button should be visible for the selected Fill")
            .center();
        for (pressed, position) in [(true, duplicate_button), (false, duplicate_button)] {
            frame(
                &mut app,
                vec![
                    egui::Event::PointerMoved(position),
                    egui::Event::PointerButton {
                        pos: position,
                        button: egui::PointerButton::Primary,
                        pressed,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
            );
        }
        assert_eq!(app.doc.layer_tree.selected_node_id, fill_duplicate_id);
        assert_eq!(
            app.doc.layer_tree.all_node_ids(),
            [
                1,
                group_id,
                fill_id,
                paint_id,
                duplicate_id,
                duplicate_id + 1,
                fill_duplicate_id,
                duplicate_id + 2
            ]
        );
        let LayerNode::Fill(fill_copy) = app.doc.layer_tree.find_node(fill_duplicate_id).unwrap()
        else {
            panic!("duplicated Fill should retain its type");
        };
        assert_eq!(fill_copy.properties.name, "Rough fill copy");
        assert_eq!(fill_copy.values[&Channel::Roughness], [73, 0, 0, 255]);
        assert_eq!(fill_copy.properties.mask.get_pixel(3, 8).0, [117]);
        assert!(app.recovery_journal.is_some());
        assert!(app.unsaved);
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
            2,
            "each subtree duplicate is journaled as one batch"
        );

        let mut reopened = test_app();
        reopened.install_open_project(project::load(&temp.0).unwrap(), temp.0.clone(), false);
        assert!(reopened.pending_journal_recovery.is_some());
        reopened.resolve_journal_recovery(JournalRecoveryChoice::Restore);
        assert!(reopened.recovery_journal.is_some());
        assert_eq!(reopened.doc.layer_tree.selected_node_id, fill_duplicate_id);
        assert_eq!(
            reopened.doc.layer_tree.all_node_ids(),
            [
                1,
                group_id,
                fill_id,
                paint_id,
                duplicate_id,
                duplicate_id + 1,
                fill_duplicate_id,
                duplicate_id + 2
            ]
        );
        let LayerNode::Group(restored_copy) =
            reopened.doc.layer_tree.find_node(duplicate_id).unwrap()
        else {
            panic!("restored duplicate should remain a Group");
        };
        assert_eq!(restored_copy.properties.name, "Asset group copy");
        let LayerNode::Fill(restored_fill) = &restored_copy.children[0] else {
            panic!("restored first child should remain a Fill");
        };
        assert_eq!(restored_fill.values[&Channel::Roughness], [73, 0, 0, 255]);
        assert_eq!(restored_fill.properties.mask.get_pixel(3, 8).0, [117]);
        let LayerNode::Fill(restored_fill_copy) = &restored_copy.children[1] else {
            panic!("restored second child should remain a Fill");
        };
        assert_eq!(restored_fill_copy.properties.id, fill_duplicate_id);
        assert_eq!(restored_fill_copy.properties.name, "Rough fill copy");
        assert_eq!(
            restored_fill_copy.values[&Channel::Roughness],
            [73, 0, 0, 255]
        );
        let LayerNode::Paint(restored_paint) = &restored_copy.children[2] else {
            panic!("restored second child should remain Paint");
        };
        assert_eq!(
            restored_paint.pixels[&Channel::BaseColor]
                .get_pixel(9, 12)
                .0,
            [13, 29, 47, 255]
        );
        assert_eq!(restored_paint.mask.get_pixel(15, 18).0, [201]);
        assert!(reopened.unsaved);
    }

    #[test]
    fn texture_set_resize_recovers_after_reopen_and_keeps_journal_enabled() {
        let temp = TempProject::new();
        let mut baseline = Document::new(64);
        let active_id = baseline.active_set_id;
        baseline.add_layer().unwrap();
        baseline
            .selected_layer_mut()
            .expect("selected paint layer")
            .name = "Detail".into();
        baseline
            .selected_layer_mut()
            .expect("selected paint layer")
            .pixels_mut(Channel::BaseColor, 64)
            .put_pixel(3, 5, image::Rgba([17, 34, 51, 68]));
        baseline
            .selected_layer_mut()
            .expect("selected paint layer")
            .pixels_mut(Channel::Roughness, 64)
            .put_pixel(9, 11, image::Rgba([91, 0, 0, 255]));
        baseline
            .selected_layer_mut()
            .expect("selected paint layer")
            .mask
            .put_pixel(13, 15, image::Luma([123]));
        baseline
            .selected_layer_mut()
            .expect("selected paint layer")
            .pixels_mut(Channel::Normal, 64)
            .put_pixel(18, 21, image::Rgba([127, 200, 240, 255]));

        let inactive_id = baseline.add_texture_set("Secondary").unwrap();
        baseline
            .selected_layer_mut()
            .expect("selected paint layer")
            .pixels_mut(Channel::Metallic, 64)
            .put_pixel(6, 7, image::Rgba([177, 0, 0, 255]));
        baseline
            .selected_layer_mut()
            .expect("selected paint layer")
            .mask
            .put_pixel(8, 9, image::Luma([210]));
        let inactive_id_before = baseline.active_set_id;
        let inactive_size_before = baseline.size;
        let inactive_layers_before = baseline.layer_tree.roots.clone();
        baseline.activate_texture_set(active_id).unwrap();
        let active_before = baseline.layer_tree.roots.clone();
        project::save(&baseline, &temp.0).unwrap();
        let project_id = baseline.project_id;
        let baseline_digest = project::file_digest(&temp.0).unwrap();

        let mut app = test_app();
        app.install_open_project(baseline, temp.0.clone(), false);
        app.resize_document(128);
        assert_eq!(app.doc.size, 128);
        assert_eq!(app.doc.active_set_id, active_id);
        assert!(app.recovery_journal.is_some());
        assert!(app.unsaved);
        assert!(!app.history.can_undo());
        assert!(!app.history.can_redo());
        drop(app);

        let records = crate::recovery::scan(
            recovery_journal_path(&temp.0),
            project_id,
            baseline_digest,
            1,
        )
        .unwrap();
        assert_eq!(records.len(), 1, "resize appends one compact event");
        let events = crate::recovery::decode_event_batch_bounded(&records[0].payload).unwrap();
        assert!(
            events.contains(&crate::recovery::DocumentEvent::TextureSetResize {
                set_id: active_id,
                size: 128,
            })
        );

        let mut reopened = test_app();
        reopened.install_open_project(project::load(&temp.0).unwrap(), temp.0.clone(), false);
        assert!(reopened.pending_journal_recovery.is_some());
        reopened.resolve_journal_recovery(JournalRecoveryChoice::Restore);
        assert_eq!(reopened.doc.size, 128);
        assert_eq!(reopened.doc.active_set_id, active_id);
        assert_eq!(reopened.doc.layer_tree.roots.len(), active_before.len());
        for (actual, old) in reopened.doc.layer_tree.roots.iter().zip(&active_before) {
            assert_eq!(
                actual.mask,
                image::imageops::resize(&old.mask, 128, 128, image::imageops::FilterType::Nearest,)
            );
            assert_eq!(actual.pixels.len(), old.pixels.len());
            for (channel, pixels) in &old.pixels {
                assert_eq!(
                    actual.pixels[channel],
                    image::imageops::resize(pixels, 128, 128, image::imageops::FilterType::Nearest,)
                );
            }
        }
        let inactive_after = reopened
            .doc
            .texture_sets
            .iter()
            .find(|set| set.id == inactive_id)
            .unwrap();
        assert_eq!(inactive_after.id, inactive_id_before);
        assert_eq!(inactive_after.size, inactive_size_before);
        assert_eq!(
            inactive_after.layer_tree.roots.len(),
            inactive_layers_before.len()
        );
        assert_eq!(
            inactive_after.layer_tree.roots[0].mask,
            inactive_layers_before[0].mask
        );
        assert_eq!(
            inactive_after.layer_tree.roots[0].pixels,
            inactive_layers_before[0].pixels
        );
        assert!(reopened.recovery_journal.is_some());

        project::save(&reopened.doc, &temp.0).unwrap();
        let reloaded = project::load(&temp.0).unwrap();
        assert_eq!(reloaded.size, 128);
        assert_eq!(reloaded.active_set_id, active_id);
        assert_eq!(reloaded.texture_set_count(), 2);
        assert_eq!(
            reloaded.layer_tree.roots[0].pixels,
            reopened.doc.layer_tree.roots[0].pixels
        );
    }

    #[test]
    fn renamed_texture_set_recovers_after_reopen_and_keeps_journal_enabled() {
        let temp = TempProject::new();
        let mut baseline = Document::new(64);
        let first_set_id = baseline.active_set_id;
        let first_layer_id = baseline.selected_layer().expect("selected paint layer").id;
        let first_pixel = image::Rgba([23, 46, 69, 92]);
        baseline
            .selected_layer_mut()
            .expect("selected paint layer")
            .pixels_mut(Channel::BaseColor, 64)
            .put_pixel(2, 3, first_pixel);
        baseline
            .selected_layer_mut()
            .expect("selected paint layer")
            .mask
            .put_pixel(4, 5, image::Luma([114]));

        let second_set_id = baseline.add_texture_set("Secondary").unwrap();
        let second_layer_id = baseline.selected_layer().expect("selected paint layer").id;
        let second_pixel = image::Rgba([180, 150, 120, 90]);
        baseline
            .selected_layer_mut()
            .expect("selected paint layer")
            .pixels_mut(Channel::Metallic, 64)
            .put_pixel(6, 7, second_pixel);
        baseline
            .selected_layer_mut()
            .expect("selected paint layer")
            .mask
            .put_pixel(8, 9, image::Luma([210]));
        baseline.activate_texture_set(first_set_id).unwrap();
        project::save(&baseline, &temp.0).unwrap();
        let project_id = baseline.project_id;
        let baseline_digest = project::file_digest(&temp.0).unwrap();

        let mut app = test_app();
        app.install_open_project(baseline, temp.0.clone(), false);
        app.texture_set_name_input = "  Main Paint Set  ".into();
        app.rename_active_texture_set();

        assert_eq!(app.doc.active_set_id, first_set_id);
        assert_eq!(app.doc.active_set_name, "Main Paint Set");
        assert_eq!(app.texture_set_name_input, "Main Paint Set");
        assert_eq!(app.doc.texture_set_count(), 2);
        assert_eq!(
            app.doc.selected_layer().expect("selected paint layer").id,
            first_layer_id
        );
        assert_eq!(
            app.doc
                .selected_layer()
                .expect("selected paint layer")
                .pixels[&Channel::BaseColor]
                .get_pixel(2, 3),
            &first_pixel
        );
        assert_eq!(
            app.doc
                .selected_layer()
                .expect("selected paint layer")
                .mask
                .get_pixel(4, 5)
                .0,
            [114]
        );
        assert!(app.unsaved);
        assert!(app.recovery_journal.is_some());
        drop(app);

        let records = crate::recovery::scan(
            recovery_journal_path(&temp.0),
            project_id,
            baseline_digest,
            1,
        )
        .unwrap();
        assert_eq!(records.len(), 1, "set rename appends one compact event");

        let mut reopened = test_app();
        reopened.install_open_project(project::load(&temp.0).unwrap(), temp.0.clone(), false);
        assert!(reopened.pending_journal_recovery.is_some());
        reopened.resolve_journal_recovery(JournalRecoveryChoice::Restore);

        assert_eq!(reopened.doc.active_set_id, first_set_id);
        assert_eq!(reopened.doc.active_set_name, "Main Paint Set");
        assert_eq!(reopened.doc.texture_set_count(), 2);
        assert_eq!(
            reopened
                .doc
                .selected_layer()
                .expect("selected paint layer")
                .id,
            first_layer_id
        );
        assert_eq!(
            reopened
                .doc
                .selected_layer()
                .expect("selected paint layer")
                .pixels[&Channel::BaseColor]
                .get_pixel(2, 3),
            &first_pixel
        );
        assert_eq!(
            reopened
                .doc
                .selected_layer()
                .expect("selected paint layer")
                .mask
                .get_pixel(4, 5)
                .0,
            [114]
        );

        let second_set = reopened
            .doc
            .texture_sets
            .iter()
            .find(|set| set.id == second_set_id)
            .expect("the untouched Texture Set should remain present");
        assert_eq!(second_set.name, "Secondary");
        assert_eq!(second_set.layer_tree.roots[0].id, second_layer_id);
        assert_eq!(
            second_set.layer_tree.roots[0].pixels[&Channel::Metallic].get_pixel(6, 7),
            &second_pixel
        );
        assert_eq!(second_set.layer_tree.roots[0].mask.get_pixel(8, 9).0, [210]);
        assert!(reopened.unsaved);
        assert!(reopened.recovery_journal.is_some());
    }

    #[test]
    fn untitled_session_autosave_restores_edits_after_reopen() {
        let temp = TempProject::new();
        let baseline_path = temp.0.with_extension("autosave-session.kalium");
        let mut app = test_app();
        app.initialize_autosave_session_at(&baseline_path).unwrap();
        assert!(app.file_path.is_none(), "autosave is not the Save target");
        assert!(app.recovery_journal.is_some());

        let layer_id = app.doc.selected_layer().expect("selected paint layer").id;
        let event = DocumentEvent::LayerOpacity {
            set_id: app.doc.active_set_id,
            layer_id,
            opacity: 0.37,
        };
        event.apply(&mut app.doc).unwrap();
        app.change_with_recovery_events(std::slice::from_ref(&event));
        assert!(app.unsaved);
        drop(app);

        let mut reopened = test_app();
        reopened
            .initialize_autosave_session_at(&baseline_path)
            .unwrap();
        assert!(reopened.pending_journal_recovery.is_some());
        reopened.resolve_journal_recovery(JournalRecoveryChoice::Restore);
        assert_eq!(
            reopened
                .doc
                .selected_layer()
                .expect("selected paint layer")
                .opacity,
            0.37
        );
        assert!(reopened.unsaved);
        assert!(reopened.file_path.is_none());
        assert!(reopened.recovery_journal.is_some());

        let journal_path = recovery_journal_path(&baseline_path);
        let disabled_path = recovery_journal_disabled_path(&journal_path);
        let lock_path = autosave_session_lock_path(&baseline_path);
        drop(reopened);
        let _ = fs::remove_file(disabled_path);
        let _ = fs::remove_file(journal_path);
        let _ = fs::remove_file(baseline_path);
        let _ = fs::remove_file(lock_path);
    }

    #[test]
    fn autosave_session_lock_prevents_concurrent_owner_and_preserves_files() {
        let temp = TempProject::new();
        let baseline_path = temp.0.with_extension("autosave-lock.kalium");
        let journal_path = recovery_journal_path(&baseline_path);
        let lock_path = autosave_session_lock_path(&baseline_path);

        let mut owner = test_app();
        owner
            .initialize_autosave_session_at(&baseline_path)
            .unwrap();
        let layer_id = owner.doc.selected_layer().expect("selected paint layer").id;
        let event = DocumentEvent::LayerOpacity {
            set_id: owner.doc.active_set_id,
            layer_id,
            opacity: 0.42,
        };
        event.apply(&mut owner.doc).unwrap();
        owner.change_with_recovery_events(std::slice::from_ref(&event));
        let baseline_before = fs::read(&baseline_path).unwrap();
        let journal_before = fs::read(&journal_path).unwrap();

        let mut contender = test_app();
        let error = match contender.initialize_autosave_session_at(&baseline_path) {
            Ok(()) => panic!("second instance unexpectedly acquired the session lock"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("another Kalium instance"));
        assert!(contender.autosave_session_path.is_none());
        assert!(contender.recovery_journal.is_none());
        assert!(contender.recovery_unavailable.is_some());
        assert_eq!(fs::read(&baseline_path).unwrap(), baseline_before);
        assert_eq!(fs::read(&journal_path).unwrap(), journal_before);

        drop(owner);
        contender
            .initialize_autosave_session_at(&baseline_path)
            .unwrap();
        assert!(contender.pending_journal_recovery.is_some());
        contender.resolve_journal_recovery(JournalRecoveryChoice::Restore);
        assert_eq!(
            contender
                .doc
                .selected_layer()
                .expect("selected paint layer")
                .opacity,
            0.42
        );
        assert!(contender.recovery_journal.is_some());
        drop(contender);

        let _ = fs::remove_file(recovery_journal_disabled_path(&journal_path));
        let _ = fs::remove_file(&journal_path);
        let _ = fs::remove_file(&baseline_path);
        let _ = fs::remove_file(&lock_path);
    }

    #[test]
    fn saved_project_recovery_lock_blocks_second_journal_owner_until_release() {
        let temp = TempProject::new();
        let baseline = Document::new(64);
        project::save(&baseline, &temp.0).unwrap();
        let mut owner = test_app();
        owner.install_open_project(project::load(&temp.0).unwrap(), temp.0.clone(), false);
        let layer_id = owner.doc.selected_layer().expect("selected paint layer").id;
        owner.append_recovery_batch(&[DocumentEvent::LayerOpacity {
            set_id: owner.doc.active_set_id,
            layer_id,
            opacity: 0.39,
        }]);
        owner
            .recovery_journal
            .take()
            .expect("project journal should be active")
            .shutdown()
            .unwrap();
        let journal_path = recovery_journal_path(&temp.0);
        let bytes_before = fs::read(&journal_path).unwrap();
        let lock_path = project_recovery_lock_path(&temp.0).unwrap();

        let mut contender = test_app();
        assert!(contender
            .provisional_project_recovery_lock(&temp.0)
            .is_err());
        assert_eq!(fs::read(&journal_path).unwrap(), bytes_before);
        contender.install_open_project(project::load(&temp.0).unwrap(), temp.0.clone(), false);
        assert!(contender.recovery_journal.is_none());
        assert!(contender.pending_journal_recovery.is_none());
        assert!(contender
            .recovery_unavailable
            .as_deref()
            .is_some_and(|message| message.contains("owned by another Kalium instance")));
        assert_eq!(fs::read(&journal_path).unwrap(), bytes_before);
        drop(owner);

        contender.configure_recovery_journal_for_path(&temp.0);
        assert!(contender.pending_journal_recovery.is_some());
        contender.resolve_journal_recovery(JournalRecoveryChoice::Restore);
        assert_eq!(
            contender
                .doc
                .selected_layer()
                .expect("selected paint layer")
                .opacity,
            0.39
        );
        assert!(contender.recovery_journal.is_some());
        assert!(lock_path.is_file());
    }

    #[test]
    fn canceling_open_releases_the_provisional_project_lock() {
        let temp = TempProject::new();
        project::save(&Document::new(64), &temp.0).unwrap();
        let mut app = test_app();
        app.pending_open_project_lock = app.provisional_project_recovery_lock(&temp.0).unwrap();
        assert!(app.pending_open_project_lock.is_some());
        app.pending_destructive = Some(super::DestructiveAction::Open {
            doc: Box::new(project::load(&temp.0).unwrap()),
            path: temp.0.clone(),
            recovered: false,
        });
        app.resolve_destructive_choice(DestructiveChoice::Cancel, &egui::Context::default());
        assert!(app.pending_open_project_lock.is_none());

        let mut next = test_app();
        next.install_open_project(project::load(&temp.0).unwrap(), temp.0.clone(), false);
        assert!(next.recovery_journal.is_some());
    }

    #[test]
    fn save_as_to_locked_project_preserves_target_and_retries_after_release() {
        let temp = TempProject::new();
        let target = temp.0.with_extension("locked-target.kalium");
        let target_baseline = Document::new(64);
        project::save(&target_baseline, &target).unwrap();
        let target_bytes = fs::read(&target).unwrap();
        let mut target_owner = test_app();
        target_owner.install_open_project(target_baseline, target.clone(), false);
        let mut source = test_app();
        source.unsaved = true;
        let original_id = source.doc.project_id;
        let target_tmp = target.with_file_name(format!(
            "{}.tmp",
            target.file_name().unwrap().to_string_lossy()
        ));

        let error = source.save_project_to_path(target.clone()).unwrap_err();
        assert!(error.to_string().contains("lock"));
        assert!(source.file_path.is_none());
        assert!(source.unsaved);
        assert_eq!(source.doc.project_id, original_id);
        assert_eq!(fs::read(&target).unwrap(), target_bytes);
        assert!(!target_tmp.exists());

        drop(target_owner);
        source.save_project_to_path(target.clone()).unwrap();
        assert_eq!(source.file_path.as_deref(), Some(target.as_path()));
        assert!(!source.unsaved);
        let mut contender = test_app();
        contender.install_open_project(project::load(&target).unwrap(), target.clone(), false);
        assert!(contender.recovery_journal.is_none());
        assert!(contender
            .recovery_unavailable
            .as_deref()
            .is_some_and(|message| message.contains("owned by another Kalium instance")));
        drop(contender);
        drop(source);
        let target_journal = recovery_journal_path(&target);
        let _ = fs::remove_file(recovery_journal_disabled_path(&target_journal));
        let _ = fs::remove_file(target_journal);
        let _ = fs::remove_file(project_recovery_lock_path(&target).unwrap());
        let _ = fs::remove_file(&target);
    }

    #[test]
    fn saving_untitled_autosave_session_as_project_hands_off_recovery_atomically() {
        let temp = TempProject::new();
        let baseline_path = temp.0.with_extension("autosave-handoff.kalium");
        let session_journal = recovery_journal_path(&baseline_path);
        let session_lock = autosave_session_lock_path(&baseline_path);
        let target = temp.0.with_extension("saved-from-session.kalium");
        let target_journal = recovery_journal_path(&target);
        let target_disabled = recovery_journal_disabled_path(&target_journal);

        let mut app = test_app();
        app.initialize_autosave_session_at(&baseline_path).unwrap();
        let layer_id = app.doc.selected_layer().expect("selected paint layer").id;
        let session_edit = DocumentEvent::LayerOpacity {
            set_id: app.doc.active_set_id,
            layer_id,
            opacity: 0.37,
        };
        session_edit.apply(&mut app.doc).unwrap();
        app.change_with_recovery_events(std::slice::from_ref(&session_edit));
        let expected_saved = app.doc.composite(Channel::BaseColor);
        let original_project_id = app.doc.project_id;
        let baseline_bytes = fs::read(&baseline_path).unwrap();
        let session_journal_bytes = fs::read(&session_journal).unwrap();
        let blocking_file = temp.0.with_extension("save-parent-blocker");
        fs::write(&blocking_file, b"not a directory").unwrap();
        let failed_target = blocking_file.join("cannot-save.kalium");
        assert!(app.save_project_to_path(failed_target).is_err());
        assert_eq!(app.doc.project_id, original_project_id);
        assert!(app.file_path.is_none());
        assert!(app.unsaved);
        assert!(app.recovery_journal.is_some());
        assert_eq!(fs::read(&baseline_path).unwrap(), baseline_bytes);
        assert_eq!(fs::read(&session_journal).unwrap(), session_journal_bytes);
        let _ = fs::remove_file(&blocking_file);

        assert!(app
            .save_project_to_path(target.clone())
            .unwrap()
            .contains("Saved"));
        assert_eq!(app.file_path.as_deref(), Some(target.as_path()));
        assert!(!app.unsaved);
        assert!(app.recovery_journal.is_some());
        assert_eq!(
            project::load(&target)
                .unwrap()
                .composite(Channel::BaseColor),
            expected_saved
        );
        assert!(!target_disabled.exists());
        assert!(!baseline_path.exists());
        assert!(!session_journal.exists());
        assert!(app.autosave_session_path.is_none());

        let project_id = app.doc.project_id;
        let target_digest = project::file_digest(&target).unwrap();
        let target_generation_store = RecoveryGenerationStore::from_journal_path(&target_journal);
        let target_generation = target_generation_store
            .read_active(project_id, 1)
            .unwrap()
            .unwrap();
        assert!(target_generation.journal_path.is_file());
        assert_eq!(target_generation.baseline_digest, target_digest);
        let later_edit = DocumentEvent::LayerOpacity {
            set_id: app.doc.active_set_id,
            layer_id,
            opacity: 0.73,
        };
        later_edit.apply(&mut app.doc).unwrap();
        app.change_with_recovery_events(std::slice::from_ref(&later_edit));
        let records = crate::recovery::scan(
            &target_generation.journal_path,
            project_id,
            target_generation.baseline_digest,
            1,
        )
        .unwrap();
        assert_eq!(
            records.len(),
            1,
            "the project journal must accept later edits"
        );

        // Save As releases the untitled-session owner lock so another app can
        // claim a fresh session while this app keeps working on the saved file.
        let mut other_session = test_app();
        other_session
            .initialize_autosave_session_at(&baseline_path)
            .unwrap();
        assert!(other_session.autosave_session_path.is_some());
        drop(other_session);
        drop(app);

        let _ = fs::remove_file(recovery_journal_disabled_path(&session_journal));
        let _ = fs::remove_file(&session_journal);
        let _ = fs::remove_file(&baseline_path);
        let _ = fs::remove_file(&session_lock);
        let _ = fs::remove_file(&target_disabled);
        let _ = fs::remove_file(&target_journal);
        let _ = target_generation_store.remove_all();
        let _ = fs::remove_file(project_recovery_lock_path(&target).unwrap());
        let _ = fs::remove_file(&target);
    }

    #[test]
    fn untitled_session_autosave_survives_process_kill() {
        if let Some(path) = std::env::var_os("KALIUM_UNTITLED_AUTOSAVE_CHILD_BASELINE") {
            let path = PathBuf::from(path);
            let ready_path = PathBuf::from(
                std::env::var_os("KALIUM_UNTITLED_AUTOSAVE_CHILD_READY")
                    .expect("autosave child ready path is required"),
            );
            let mut app = test_app();
            app.initialize_autosave_session_at(&path)
                .expect("initialize autosave baseline");
            let event = DocumentEvent::LayerOpacity {
                set_id: app.doc.active_set_id,
                layer_id: app.doc.selected_layer().expect("selected paint layer").id,
                opacity: 0.61,
            };
            event.apply(&mut app.doc).expect("apply autosaved edit");
            app.change_with_recovery_events(std::slice::from_ref(&event));
            fs::write(ready_path, b"autosave edit synced").unwrap();
            loop {
                std::thread::sleep(Duration::from_secs(1));
            }
        }

        let temp = TempProject::new();
        let baseline_path = temp.0.with_extension("autosave-process.kalium");
        let ready_path = temp.0.with_extension("autosave-process-ready");
        let mut writer = Command::new(std::env::current_exe().expect("test executable path"))
            .args([
                "--exact",
                "app::unsaved_change_tests::untitled_session_autosave_survives_process_kill",
                "--nocapture",
            ])
            .env("KALIUM_UNTITLED_AUTOSAVE_CHILD_BASELINE", &baseline_path)
            .env("KALIUM_UNTITLED_AUTOSAVE_CHILD_READY", &ready_path)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn autosave writer child");
        let deadline = Instant::now() + Duration::from_secs(30);
        while !ready_path.exists() {
            if let Some(status) = writer.try_wait().expect("poll autosave writer child") {
                panic!("autosave child exited before syncing its edit: {status}");
            }
            if Instant::now() >= deadline {
                let _ = writer.kill();
                let output = writer.wait_with_output().expect("collect child output");
                panic!(
                    "autosave child did not become ready; stderr: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(fs::read(&ready_path).unwrap(), b"autosave edit synced");
        writer.kill().expect("kill autosave child without shutdown");
        assert!(!writer.wait().expect("reap autosave child").success());

        let mut reopened = test_app();
        reopened
            .initialize_autosave_session_at(&baseline_path)
            .expect("reopen autosave baseline");
        assert!(reopened.pending_journal_recovery.is_some());
        reopened.resolve_journal_recovery(JournalRecoveryChoice::Restore);
        assert_eq!(
            reopened
                .doc
                .selected_layer()
                .expect("selected paint layer")
                .opacity,
            0.61
        );
        assert!(reopened.unsaved);

        reopened.stop_recovery_writer().unwrap();
        let journal_path = recovery_journal_path(&baseline_path);
        let lock_path = autosave_session_lock_path(&baseline_path);
        let _ = fs::remove_file(recovery_journal_disabled_path(&journal_path));
        let _ = fs::remove_file(journal_path);
        let _ = fs::remove_file(baseline_path);
        let _ = fs::remove_file(lock_path);
        let _ = fs::remove_file(ready_path);
    }

    #[test]
    fn created_texture_set_and_paint_recover_after_reopen() {
        let temp = TempProject::new();
        let mut baseline = Document::new(64);
        let original_set_id = baseline.active_set_id;
        let original_layer_id = baseline.selected_layer().expect("selected paint layer").id;
        let original_pixel = image::Rgba([10, 20, 30, 255]);
        baseline
            .selected_layer_mut()
            .expect("selected paint layer")
            .pixels_mut(Channel::BaseColor, 64)
            .put_pixel(2, 3, original_pixel);
        project::save(&baseline, &temp.0).unwrap();
        let project_id = baseline.project_id;
        let baseline_digest = project::file_digest(&temp.0).unwrap();

        let mut app = test_app();
        app.install_open_project(baseline, temp.0.clone(), false);
        let created_id = app.doc.next_set_id;
        app.create_texture_set();
        let created_layer_id = app.doc.selected_layer().expect("selected paint layer").id;
        let new_pixel = image::Rgba([91, 82, 73, 255]);
        let paint_event = crate::recovery::DocumentEvent::LayerImagePatch {
            set_id: created_id,
            layer_id: created_layer_id,
            target: crate::recovery::LayerImageTarget::Channel(Channel::Roughness),
            x: 7,
            y: 9,
            width: 1,
            height: 1,
            bytes: new_pixel.0.to_vec(),
        };
        paint_event.apply(&mut app.doc).unwrap();
        app.change_with_recovery_events(&[paint_event]);

        assert_eq!(app.doc.active_set_id, created_id);
        assert_eq!(app.doc.active_set_name, format!("Texture Set {created_id}"));
        assert_eq!(app.doc.size, 64);
        assert_eq!(
            app.doc.selected_layer().expect("selected paint layer").id,
            created_layer_id
        );
        assert_eq!(
            app.doc
                .selected_layer()
                .expect("selected paint layer")
                .pixels[&Channel::Roughness]
                .get_pixel(7, 9),
            &new_pixel
        );
        assert!(app.recovery_journal.is_some());
        drop(app);

        let records = crate::recovery::scan(
            recovery_journal_path(&temp.0),
            project_id,
            baseline_digest,
            1,
        )
        .unwrap();
        assert_eq!(records.len(), 2);

        let mut reopened = test_app();
        reopened.install_open_project(project::load(&temp.0).unwrap(), temp.0.clone(), false);
        assert!(reopened.pending_journal_recovery.is_some());
        reopened.resolve_journal_recovery(JournalRecoveryChoice::Restore);
        assert_eq!(reopened.doc.active_set_id, created_id);
        assert_eq!(
            reopened.doc.active_set_name,
            format!("Texture Set {created_id}")
        );
        assert_eq!(reopened.doc.size, 64);
        assert_eq!(
            reopened
                .doc
                .selected_layer()
                .expect("selected paint layer")
                .id,
            created_layer_id
        );
        assert_eq!(
            reopened
                .doc
                .selected_layer()
                .expect("selected paint layer")
                .pixels[&Channel::Roughness]
                .get_pixel(7, 9),
            &new_pixel
        );
        assert_eq!(reopened.doc.texture_set_count(), 2);
        let original = reopened
            .doc
            .texture_sets
            .iter()
            .find(|set| set.id == original_set_id)
            .unwrap();
        assert_eq!(original.layer_tree.roots[0].id, original_layer_id);
        assert_eq!(
            original.layer_tree.roots[0].pixels[&Channel::BaseColor].get_pixel(2, 3),
            &original_pixel
        );
        assert!(reopened.unsaved);
        assert!(reopened.recovery_journal.is_some());
    }

    #[test]
    fn removed_texture_set_recovers_and_clears_only_its_bindings() {
        let temp = TempProject::new();
        let mut baseline = Document::new(64);
        baseline.mesh.materials = vec!["Primary".into(), "Secondary".into(), "Tertiary".into()];
        let first_set_id = baseline.active_set_id;
        baseline.material_bindings = vec![Some(first_set_id), None, None];
        let second_set_id = baseline.add_texture_set("Second").unwrap();
        baseline.material_bindings[1] = Some(second_set_id);
        baseline
            .selected_layer_mut()
            .expect("selected paint layer")
            .pixels_mut(Channel::Metallic, 64)
            .put_pixel(4, 5, image::Rgba([70, 0, 0, 255]));
        let third_set_id = baseline.add_texture_set("Third").unwrap();
        baseline.material_bindings[2] = Some(third_set_id);
        baseline
            .selected_layer_mut()
            .expect("selected paint layer")
            .pixels_mut(Channel::Emissive, 64)
            .put_pixel(6, 7, image::Rgba([30, 20, 10, 255]));
        baseline.activate_texture_set(third_set_id).unwrap();
        project::save(&baseline, &temp.0).unwrap();
        let project_id = baseline.project_id;
        let baseline_digest = project::file_digest(&temp.0).unwrap();

        let mut app = test_app();
        app.install_open_project(baseline, temp.0.clone(), false);
        app.remove_active_texture_set();
        assert_eq!(app.doc.active_set_id, first_set_id);
        assert_eq!(app.doc.texture_set_count(), 2);
        assert_eq!(
            app.doc.material_bindings,
            vec![Some(first_set_id), Some(second_set_id), None]
        );
        assert!(app.recovery_journal.is_some());
        drop(app);

        let records = crate::recovery::scan(
            recovery_journal_path(&temp.0),
            project_id,
            baseline_digest,
            1,
        )
        .unwrap();
        assert_eq!(records.len(), 1);

        let mut reopened = test_app();
        reopened.install_open_project(project::load(&temp.0).unwrap(), temp.0.clone(), false);
        assert!(reopened.pending_journal_recovery.is_some());
        reopened.resolve_journal_recovery(JournalRecoveryChoice::Restore);
        assert_eq!(reopened.doc.active_set_id, first_set_id);
        assert_eq!(reopened.doc.texture_set_count(), 2);
        assert_eq!(
            reopened.doc.material_bindings,
            vec![Some(first_set_id), Some(second_set_id), None]
        );
        assert!(reopened
            .doc
            .texture_sets
            .iter()
            .all(|set| set.id != third_set_id));
        let surviving_second = reopened
            .doc
            .texture_sets
            .iter()
            .find(|set| set.id == second_set_id)
            .unwrap();
        assert_eq!(
            surviving_second.layer_tree.roots[0].pixels[&Channel::Metallic].get_pixel(4, 5),
            &image::Rgba([70, 0, 0, 255])
        );
        assert!(reopened.unsaved);
        assert!(reopened.recovery_journal.is_some());
    }

    #[test]
    fn material_binding_recovers_after_reopen_without_changing_active_set() {
        let temp = TempProject::new();
        let mut baseline = Document::new(64);
        baseline.mesh.materials = vec!["Body".into(), "Trim".into()];
        let active_set_id = baseline.active_set_id;
        let second_set_id = baseline.add_texture_set("Second Set").unwrap();
        baseline.activate_texture_set(active_set_id).unwrap();
        baseline.material_bindings = vec![None, Some(active_set_id)];
        project::save(&baseline, &temp.0).unwrap();
        let project_id = baseline.project_id;
        let baseline_digest = project::file_digest(&temp.0).unwrap();

        let mut app = test_app();
        app.install_open_project(baseline, temp.0.clone(), false);
        app.set_material_binding(0, Some(second_set_id));
        assert_eq!(
            app.doc.material_bindings,
            vec![Some(second_set_id), Some(active_set_id)]
        );
        assert_eq!(app.doc.active_set_id, active_set_id);
        assert!(app.recovery_journal.is_some());
        drop(app);

        let records = crate::recovery::scan(
            recovery_journal_path(&temp.0),
            project_id,
            baseline_digest,
            1,
        )
        .unwrap();
        assert_eq!(records.len(), 1);

        let mut reopened = test_app();
        reopened.install_open_project(project::load(&temp.0).unwrap(), temp.0.clone(), false);
        assert!(reopened.pending_journal_recovery.is_some());
        reopened.resolve_journal_recovery(JournalRecoveryChoice::Restore);
        assert_eq!(reopened.doc.active_set_id, active_set_id);
        assert_eq!(
            reopened.doc.material_bindings,
            vec![Some(second_set_id), Some(active_set_id)]
        );
        assert_eq!(reopened.doc.texture_set_count(), 2);
        assert!(reopened.unsaved);
        assert!(reopened.recovery_journal.is_some());
    }

    #[test]
    fn texture_set_selection_recovers_after_reopen_and_preserves_set_content() {
        let temp = TempProject::new();
        let mut baseline = Document::new(64);
        let first_set_id = baseline.active_set_id;
        let first_layer_id = baseline.selected_layer().expect("selected paint layer").id;
        let first_pixel = image::Rgba([15, 25, 35, 255]);
        baseline
            .selected_layer_mut()
            .expect("selected paint layer")
            .pixels_mut(Channel::BaseColor, 64)
            .put_pixel(1, 2, first_pixel);
        let second_set_id = baseline.add_texture_set("Second").unwrap();
        let second_layer_id = baseline.selected_layer().expect("selected paint layer").id;
        let second_pixel = image::Rgba([55, 65, 75, 255]);
        baseline
            .selected_layer_mut()
            .expect("selected paint layer")
            .pixels_mut(Channel::Metallic, 64)
            .put_pixel(3, 4, second_pixel);
        baseline.activate_texture_set(first_set_id).unwrap();
        project::save(&baseline, &temp.0).unwrap();
        let project_id = baseline.project_id;
        let baseline_digest = project::file_digest(&temp.0).unwrap();

        let mut app = test_app();
        app.install_open_project(baseline, temp.0.clone(), false);
        app.select_texture_set(second_set_id);
        assert_eq!(app.doc.active_set_id, second_set_id);
        assert_eq!(
            app.doc.selected_layer().expect("selected paint layer").id,
            second_layer_id
        );
        assert!(app.recovery_journal.is_some());
        drop(app);

        let records = crate::recovery::scan(
            recovery_journal_path(&temp.0),
            project_id,
            baseline_digest,
            1,
        )
        .unwrap();
        assert_eq!(records.len(), 1);

        let mut reopened = test_app();
        reopened.install_open_project(project::load(&temp.0).unwrap(), temp.0.clone(), false);
        assert!(reopened.pending_journal_recovery.is_some());
        reopened.resolve_journal_recovery(JournalRecoveryChoice::Restore);
        assert_eq!(reopened.doc.active_set_id, second_set_id);
        assert_eq!(
            reopened
                .doc
                .selected_layer()
                .expect("selected paint layer")
                .id,
            second_layer_id
        );
        assert_eq!(
            reopened
                .doc
                .selected_layer()
                .expect("selected paint layer")
                .pixels[&Channel::Metallic]
                .get_pixel(3, 4),
            &second_pixel
        );
        assert_eq!(reopened.doc.texture_set_count(), 2);
        let first_set = reopened
            .doc
            .texture_sets
            .iter()
            .find(|set| set.id == first_set_id)
            .unwrap();
        assert_eq!(first_set.layer_tree.roots[0].id, first_layer_id);
        assert_eq!(
            first_set.layer_tree.roots[0].pixels[&Channel::BaseColor].get_pixel(1, 2),
            &first_pixel
        );
        assert!(reopened.unsaved);
        assert!(reopened.recovery_journal.is_some());
    }

    #[test]
    fn removed_populated_layer_recovers_after_reopen_and_keeps_journal_enabled() {
        let temp = TempProject::new();
        let mut baseline = Document::new(64);
        baseline.add_layer().unwrap();
        let removed_id = baseline.selected_layer().expect("selected paint layer").id;
        let selected_after_id = baseline.layer_tree.roots[0].id;
        let size = baseline.size;
        baseline
            .selected_layer_mut()
            .expect("selected paint layer")
            .pixels_mut(Channel::BaseColor, size)
            .put_pixel(5, 7, image::Rgba([17, 34, 51, 255]));
        baseline
            .selected_layer_mut()
            .expect("selected paint layer")
            .mask
            .put_pixel(9, 11, image::Luma([123]));
        baseline
            .selected_layer_mut()
            .expect("selected paint layer")
            .extensions
            .insert("future".into(), serde_json::json!({ "preserve": true }));
        project::save(&baseline, &temp.0).unwrap();
        let project_id = baseline.project_id;
        let baseline_digest = project::file_digest(&temp.0).unwrap();

        let mut app = test_app();
        app.install_open_project(baseline, temp.0.clone(), false);
        assert!(app.recovery_journal.is_some());
        app.layer_command(LayerCommand::Remove);

        assert_eq!(app.doc.layer_tree.roots.len(), 1);
        assert_eq!(
            app.doc.selected_layer().expect("selected paint layer").id,
            selected_after_id
        );
        assert_eq!(app.doc.layer_tree.next_node_id, removed_id + 1);
        assert!(app.recovery_journal.is_some());
        assert!(app.unsaved);
        drop(app);

        let records = crate::recovery::scan(
            recovery_journal_path(&temp.0),
            project_id,
            baseline_digest,
            1,
        )
        .unwrap();
        assert_eq!(records.len(), 1, "layer removal appends one compact event");

        let mut reopened = test_app();
        reopened.install_open_project(project::load(&temp.0).unwrap(), temp.0.clone(), false);
        assert!(reopened.pending_journal_recovery.is_some());
        reopened.resolve_journal_recovery(JournalRecoveryChoice::Restore);

        assert_eq!(reopened.doc.layer_tree.roots.len(), 1);
        assert_eq!(
            reopened
                .doc
                .selected_layer()
                .expect("selected paint layer")
                .id,
            selected_after_id
        );
        assert_eq!(reopened.doc.layer_tree.next_node_id, removed_id + 1);
        assert!(reopened.unsaved);
        assert!(reopened.recovery_journal.is_some());
    }

    #[test]
    fn removed_layer_undo_restores_full_snapshot_after_reopen() {
        let temp = TempProject::new();
        let mut baseline = Document::new(64);
        baseline.add_layer().unwrap();
        baseline.layer_tree.selected_node_id = baseline.layer_tree.roots[0].id();
        let removed_id = baseline.selected_layer().expect("selected paint layer").id;
        let next_layer_id = baseline.layer_tree.next_node_id;
        {
            let layer = baseline.selected_layer_mut().expect("selected paint layer");
            layer.name = "Layer at stack head".into();
            layer.visible = false;
            layer.opacity = 0.625;
            layer.blend = BlendMode::Multiply;
            layer.disabled_channels.insert(Channel::Metallic);
            layer.mask.put_pixel(9, 11, image::Luma([123]));
            layer.pixels_mut(Channel::BaseColor, 64).put_pixel(
                5,
                7,
                image::Rgba([17, 34, 51, 255]),
            );
            layer
                .pixels_mut(Channel::Roughness, 64)
                .put_pixel(2, 3, image::Rgba([78, 0, 0, 255]));
            layer.extensions.insert(
                "future_layer_field".into(),
                serde_json::json!({"preserve": [1, 2, 3]}),
            );
        }
        let expected = baseline
            .selected_layer()
            .expect("selected paint layer")
            .clone();
        project::save(&baseline, &temp.0).unwrap();

        let mut app = test_app();
        app.install_open_project(baseline, temp.0.clone(), false);
        app.layer_command(LayerCommand::Remove);
        assert_eq!(app.doc.layer_tree.roots.len(), 1);
        app.apply_history(HistoryDirection::Undo);
        assert_eq!(app.doc.layer_tree.roots.len(), 2);
        assert_eq!(
            app.doc.selected_layer().expect("selected paint layer").id,
            removed_id
        );
        assert!(app.recovery_journal.is_some(), "status: {}", app.status);
        drop(app);

        let mut reopened = test_app();
        reopened.install_open_project(project::load(&temp.0).unwrap(), temp.0.clone(), false);
        assert!(
            reopened.pending_journal_recovery.is_some(),
            "status: {}; recovery: {:?}",
            reopened.status,
            reopened.recovery_unavailable
        );
        reopened.resolve_journal_recovery(JournalRecoveryChoice::Restore);
        let restored = reopened.doc.selected_layer().expect("selected paint layer");
        assert_eq!(reopened.doc.layer_tree.roots.len(), 2);
        assert_eq!(restored.id, removed_id);
        assert_eq!(reopened.doc.layer_tree.next_node_id, next_layer_id);
        assert_eq!(restored.name, expected.name);
        assert!(!restored.visible);
        assert_eq!(restored.opacity, expected.opacity);
        assert_eq!(restored.blend, expected.blend);
        assert_eq!(restored.disabled_channels, expected.disabled_channels);
        assert_eq!(restored.mask.as_raw(), expected.mask.as_raw());
        for channel in [Channel::BaseColor, Channel::Roughness] {
            assert_eq!(
                restored.pixels[&channel].as_raw(),
                expected.pixels[&channel].as_raw()
            );
        }
        assert_eq!(restored.extensions, expected.extensions);
    }

    #[test]
    fn added_layer_recovery_replays_blank_layer_and_keeps_journal_enabled() {
        let temp = TempProject::new();
        let baseline = Document::new(64);
        project::save(&baseline, &temp.0).unwrap();
        let mut app = test_app();
        app.install_open_project(baseline, temp.0.clone(), false);
        let selected_before = app.doc.selected_layer().expect("selected paint layer").id;

        app.layer_command(LayerCommand::Add);

        let added = app.doc.selected_layer().expect("selected paint layer");
        let added_id = added.id;
        let added_name = added.name.clone();
        assert_eq!(app.doc.layer_tree.roots.len(), 2);
        assert_eq!(added_id, app.doc.layer_tree.next_node_id - 1);
        assert!(app.recovery_journal.is_some());
        assert!(app.unsaved);
        drop(app);

        let mut reopened = test_app();
        reopened.install_open_project(project::load(&temp.0).unwrap(), temp.0.clone(), false);
        assert!(reopened.pending_journal_recovery.is_some());
        reopened.resolve_journal_recovery(JournalRecoveryChoice::Restore);

        assert_eq!(reopened.doc.layer_tree.roots.len(), 2);
        assert_eq!(reopened.doc.layer_tree.roots[1].id, added_id);
        assert_eq!(reopened.doc.layer_tree.roots[1].name, added_name);
        assert!(reopened.doc.layer_tree.roots[1].pixels.is_empty());
        assert!(reopened.doc.layer_tree.roots[1]
            .mask
            .pixels()
            .all(|pixel| pixel.0 == [255]));
        assert_eq!(
            reopened
                .doc
                .selected_layer()
                .expect("selected paint layer")
                .id,
            added_id
        );
        assert_ne!(
            reopened
                .doc
                .selected_layer()
                .expect("selected paint layer")
                .id,
            selected_before
        );
        assert_eq!(reopened.doc.layer_tree.next_node_id, added_id + 1);
        assert!(reopened.recovery_journal.is_some());
        assert!(reopened.unsaved);
    }

    #[test]
    fn unencodable_committed_stroke_disables_recovery_after_valid_prefix() {
        let temp = TempProject::new();
        let baseline = Document::new(1024);
        let project_id = baseline.project_id;
        project::save(&baseline, &temp.0).unwrap();

        let mut app = test_app();
        app.install_open_project(baseline, temp.0.clone(), false);
        app.append_recovery_batch(&[crate::recovery::DocumentEvent::LayerVisibility {
            set_id: app.doc.active_set_id,
            layer_id: app.doc.selected_layer().expect("selected paint layer").id,
            visible: false,
        }]);
        let mut capture = History::begin_stroke(&app.doc, Channel::BaseColor, BrushMode::Paint);
        assert!(History::capture_stroke_rect(
            &mut capture,
            &app.doc,
            0,
            0,
            1023,
            1023
        ));
        for pixel in app
            .doc
            .selected_layer_mut()
            .expect("selected paint layer")
            .pixels_mut(Channel::BaseColor, 1024)
            .pixels_mut()
        {
            *pixel = image::Rgba([1, 2, 3, 255]);
        }
        app.pending_stroke = Some(PendingStroke {
            capture,
            was_unsaved: false,
        });
        app.finish_stroke();

        let journal_path = recovery_journal_path(&temp.0);
        assert_eq!(
            crate::recovery::scan(
                &journal_path,
                project_id,
                project::file_digest(&temp.0).unwrap(),
                1
            )
            .unwrap()
            .len(),
            1,
            "the valid prefix remains inspectable but must no longer be offered"
        );
        assert!(recovery_journal_disabled_path(&journal_path).exists());
        assert!(app.recovery_journal.is_none());
        drop(app);

        let mut reopened = test_app();
        reopened.install_open_project(project::load(&temp.0).unwrap(), temp.0.clone(), false);
        assert!(reopened.pending_journal_recovery.is_none());
        assert!(reopened.recovery_journal.is_none());
        assert!(reopened.status.contains("recovery is paused"));
    }

    #[test]
    fn restore_recovers_valid_prefix_after_torn_final_journal_frame() {
        let temp = TempProject::new();
        let baseline = Document::new(64);
        let project_id = baseline.project_id;
        project::save(&baseline, &temp.0).unwrap();
        let baseline_digest = project::file_digest(&temp.0).unwrap();

        let mut app = test_app();
        app.install_open_project(baseline, temp.0.clone(), false);
        let layer_id = app.doc.selected_layer().expect("selected paint layer").id;
        app.append_recovery_batch(&[crate::recovery::DocumentEvent::LayerVisibility {
            set_id: app.doc.active_set_id,
            layer_id,
            visible: false,
        }]);
        app.append_recovery_batch(&[crate::recovery::DocumentEvent::LayerOpacity {
            set_id: app.doc.active_set_id,
            layer_id,
            opacity: 0.25,
        }]);
        drop(app);

        let journal_path = recovery_journal_path(&temp.0);
        let journal_bytes = std::fs::read(&journal_path).unwrap();
        assert!(journal_bytes.len() > 2);
        std::fs::write(&journal_path, &journal_bytes[..journal_bytes.len() - 2]).unwrap();

        let mut reopened = test_app();
        reopened.install_open_project(project::load(&temp.0).unwrap(), temp.0.clone(), false);
        assert!(reopened.pending_journal_recovery.is_some());
        reopened.resolve_journal_recovery(JournalRecoveryChoice::Restore);
        let layer = reopened.doc.selected_layer().expect("selected paint layer");
        assert!(!layer.visible, "complete prefix event should be replayed");
        assert_eq!(layer.opacity, 1.0, "torn final frame must not be replayed");

        reopened.append_recovery_batch(&[crate::recovery::DocumentEvent::LayerOpacity {
            set_id: reopened.doc.active_set_id,
            layer_id,
            opacity: 0.5,
        }]);
        let records = crate::recovery::scan(&journal_path, project_id, baseline_digest, 1).unwrap();
        assert_eq!(
            records.len(),
            2,
            "journal should append after truncating its torn tail"
        );
    }

    #[test]
    fn pending_journal_recovery_blocks_save_and_destructive_file_actions() {
        let temp = TempProject::new();
        let baseline = Document::new(64);
        let project_id = baseline.project_id;
        project::save(&baseline, &temp.0).unwrap();
        let baseline_digest = project::file_digest(&temp.0).unwrap();

        let mut app = test_app();
        app.install_open_project(baseline, temp.0.clone(), false);
        let layer_id = app.doc.selected_layer().expect("selected paint layer").id;
        app.append_recovery_batch(&[crate::recovery::DocumentEvent::LayerOpacity {
            set_id: app.doc.active_set_id,
            layer_id,
            opacity: 0.25,
        }]);
        drop(app);

        let journal_path = recovery_journal_path(&temp.0);
        let before = std::fs::read(&journal_path).unwrap();
        let mut reopened = test_app();
        reopened.install_open_project(project::load(&temp.0).unwrap(), temp.0.clone(), false);
        assert!(reopened.pending_journal_recovery.is_some());

        for action in [
            Action::Save,
            Action::SaveAs,
            Action::Open,
            Action::New,
            Action::RecoverCheckpoint,
        ] {
            reopened.perform(action);
            assert!(reopened.pending_journal_recovery.is_some());
            assert_eq!(std::fs::read(&journal_path).unwrap(), before);
        }
        assert!(reopened.status.contains("Restore or discard"));
        assert_eq!(
            crate::recovery::scan(&journal_path, project_id, baseline_digest, 1)
                .unwrap()
                .len(),
            1,
            "the unreplayed journal record must remain available"
        );

        reopened.resolve_journal_recovery(JournalRecoveryChoice::Restore);
        assert_eq!(
            reopened
                .doc
                .selected_layer()
                .expect("selected paint layer")
                .opacity,
            0.25
        );
        assert!(reopened.pending_journal_recovery.is_none());
    }

    #[test]
    fn journal_recovery_modal_buttons_restore_or_discard_through_pointer_input() {
        let restore_temp = TempProject::new();
        let mut restore_app = app_with_pending_layer_opacity_recovery(&restore_temp.0, 0.31);
        let restore_ctx = egui::Context::default();
        run_journal_dialog_frame(&mut restore_app, &restore_ctx, Vec::new());
        let restore_rect = restore_ctx
            .data(|data| {
                data.get_temp::<egui::Rect>(egui::Id::new("journal-recovery-restore-rect"))
            })
            .expect("Restore button geometry should be captured");
        let restore_pos = restore_rect.center();
        run_journal_dialog_frame(
            &mut restore_app,
            &restore_ctx,
            vec![egui::Event::PointerMoved(restore_pos)],
        );
        let restore_rect = restore_ctx
            .data(|data| {
                data.get_temp::<egui::Rect>(egui::Id::new("journal-recovery-restore-rect"))
            })
            .expect("Restore button geometry should remain available");
        let restore_pos = restore_rect.center();
        run_journal_dialog_frame(
            &mut restore_app,
            &restore_ctx,
            vec![
                egui::Event::PointerMoved(restore_pos),
                egui::Event::PointerButton {
                    pos: restore_pos,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
        run_journal_dialog_frame(
            &mut restore_app,
            &restore_ctx,
            vec![
                egui::Event::PointerMoved(restore_pos),
                egui::Event::PointerButton {
                    pos: restore_pos,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
        assert!(restore_app.pending_journal_recovery.is_none());
        assert_eq!(
            restore_app
                .doc
                .selected_layer()
                .expect("selected paint layer")
                .opacity,
            0.31
        );
        assert!(restore_app.unsaved);
        assert!(restore_app.recovery_journal.is_some());
        drop(restore_app);

        let discard_temp = TempProject::new();
        let mut discard_app = app_with_pending_layer_opacity_recovery(&discard_temp.0, 0.67);
        let discard_ctx = egui::Context::default();
        run_journal_dialog_frame(&mut discard_app, &discard_ctx, Vec::new());
        let discard_rect = discard_ctx
            .data(|data| {
                data.get_temp::<egui::Rect>(egui::Id::new("journal-recovery-discard-rect"))
            })
            .expect("Discard button geometry should be captured");
        let discard_pos = discard_rect.center();
        run_journal_dialog_frame(
            &mut discard_app,
            &discard_ctx,
            vec![egui::Event::PointerMoved(discard_pos)],
        );
        let discard_rect = discard_ctx
            .data(|data| {
                data.get_temp::<egui::Rect>(egui::Id::new("journal-recovery-discard-rect"))
            })
            .expect("Discard button geometry should remain available");
        let discard_pos = discard_rect.center();
        run_journal_dialog_frame(
            &mut discard_app,
            &discard_ctx,
            vec![
                egui::Event::PointerMoved(discard_pos),
                egui::Event::PointerButton {
                    pos: discard_pos,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
        run_journal_dialog_frame(
            &mut discard_app,
            &discard_ctx,
            vec![
                egui::Event::PointerMoved(discard_pos),
                egui::Event::PointerButton {
                    pos: discard_pos,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
        assert!(discard_app.pending_journal_recovery.is_none());
        assert_eq!(
            discard_app
                .doc
                .selected_layer()
                .expect("selected paint layer")
                .opacity,
            1.0
        );
        assert!(!discard_app.unsaved);
        assert!(discard_app.recovery_journal.is_some());
        let discarded_generation =
            RecoveryGenerationStore::from_journal_path(&recovery_journal_path(&discard_temp.0))
                .read_active(discard_app.doc.project_id, 1)
                .unwrap()
                .unwrap();
        assert_eq!(
            crate::recovery::scan(
                &discarded_generation.journal_path,
                discard_app.doc.project_id,
                discarded_generation.baseline_digest,
                1,
            )
            .unwrap()
            .len(),
            0,
            "Discard should replace the replayable journal with an empty baseline journal"
        );
    }

    #[test]
    fn complete_corrupt_journal_frame_is_rejected_without_recovery_prompt() {
        let temp = TempProject::new();
        let baseline = Document::new(64);
        project::save(&baseline, &temp.0).unwrap();

        let mut app = test_app();
        app.install_open_project(baseline, temp.0.clone(), false);
        app.append_recovery_batch(&[crate::recovery::DocumentEvent::LayerVisibility {
            set_id: app.doc.active_set_id,
            layer_id: app.doc.selected_layer().expect("selected paint layer").id,
            visible: false,
        }]);
        drop(app);

        let journal_path = recovery_journal_path(&temp.0);
        let mut journal_bytes = std::fs::read(&journal_path).unwrap();
        let last = journal_bytes
            .last_mut()
            .expect("journal has a header and record");
        *last ^= 0xff;
        std::fs::write(&journal_path, journal_bytes).unwrap();

        let mut reopened = test_app();
        reopened.install_open_project(project::load(&temp.0).unwrap(), temp.0.clone(), false);
        assert!(reopened.pending_journal_recovery.is_none());
        assert!(reopened.recovery_journal.is_none());
        assert!(reopened.status.contains("journal was rejected"));
        assert!(
            reopened
                .doc
                .selected_layer()
                .expect("selected paint layer")
                .visible
        );
    }

    #[test]
    fn selection_after_select_then_paint_is_restored_by_stable_id() {
        let temp = TempProject::new();
        let mut baseline = Document::new(64);
        baseline.add_layer().unwrap();
        project::save(&baseline, &temp.0).unwrap();

        let mut app = test_app();
        app.install_open_project(baseline, temp.0.clone(), false);
        app.doc.layer_tree.selected_node_id = app.doc.layer_tree.roots[1].id();
        let mut capture = History::begin_stroke(&app.doc, Channel::BaseColor, BrushMode::Paint);
        assert!(History::capture_stroke_rect(
            &mut capture,
            &app.doc,
            3,
            4,
            3,
            4
        ));
        app.doc
            .selected_layer_mut()
            .expect("selected paint layer")
            .pixels_mut(Channel::BaseColor, 64)
            .put_pixel(3, 4, image::Rgba([11, 22, 33, 255]));
        app.pending_stroke = Some(PendingStroke {
            capture,
            was_unsaved: false,
        });
        app.finish_stroke();
        let selected_id = app.doc.selected_layer().expect("selected paint layer").id;
        drop(app);

        let mut reopened = test_app();
        reopened.install_open_project(project::load(&temp.0).unwrap(), temp.0.clone(), false);
        reopened.resolve_journal_recovery(JournalRecoveryChoice::Restore);
        assert_eq!(
            reopened
                .doc
                .selected_layer()
                .expect("selected paint layer")
                .id,
            selected_id
        );
        assert_eq!(reopened.doc.layer_tree.selected_node_id, selected_id);
        assert_eq!(
            reopened
                .doc
                .selected_layer()
                .expect("selected paint layer")
                .pixels[&Channel::BaseColor]
                .get_pixel(3, 4),
            &image::Rgba([11, 22, 33, 255])
        );
    }

    #[test]
    fn recovery_journals_undo_and_redo_before_applying_history() {
        let temp = TempProject::new();
        let baseline = Document::new(64);
        project::save(&baseline, &temp.0).unwrap();

        let mut app = test_app();
        app.install_open_project(baseline, temp.0.clone(), false);
        let layer_id = app.doc.selected_layer().expect("selected paint layer").id;
        let mut capture = History::begin_stroke(&app.doc, Channel::BaseColor, BrushMode::Paint);
        assert!(History::capture_stroke_rect(
            &mut capture,
            &app.doc,
            3,
            4,
            3,
            4
        ));
        app.doc
            .selected_layer_mut()
            .expect("selected paint layer")
            .pixels_mut(Channel::BaseColor, 64)
            .put_pixel(3, 4, image::Rgba([11, 22, 33, 255]));
        app.pending_stroke = Some(PendingStroke {
            capture,
            was_unsaved: false,
        });
        app.finish_stroke();

        app.apply_history(HistoryDirection::Undo);
        assert!(!app
            .doc
            .selected_layer()
            .expect("selected paint layer")
            .pixels
            .contains_key(&Channel::BaseColor));
        app.apply_history(HistoryDirection::Redo);
        assert_eq!(
            app.doc
                .selected_layer()
                .expect("selected paint layer")
                .pixels[&Channel::BaseColor]
                .get_pixel(3, 4),
            &image::Rgba([11, 22, 33, 255])
        );
        let set_id = app.doc.active_set_id;
        let recovery_before = LayerRecoveryProperties::capture(
            app.doc.selected_layer().expect("selected paint layer"),
        );
        let history_before =
            LayerProperties::capture(app.doc.selected_layer().expect("selected paint layer"));
        let graph = crate::procedural::ProceduralGraph::value_noise(193, 7.25);
        app.doc
            .selected_layer_mut()
            .expect("selected paint layer")
            .procedural_graph = Some(graph.clone());
        app.history.record_properties(
            layer_id,
            history_before,
            LayerProperties::capture(app.doc.selected_layer().expect("selected paint layer")),
        );
        let graph_events = recovery_before.events_since(
            set_id,
            app.doc.selected_layer().expect("selected paint layer"),
        );
        assert_eq!(graph_events.len(), 1);
        app.change_with_recovery_events(&graph_events);
        app.apply_history(HistoryDirection::Undo);
        assert!(app
            .doc
            .selected_layer()
            .expect("selected paint layer")
            .procedural_graph
            .is_none());
        app.apply_history(HistoryDirection::Redo);
        assert_eq!(
            app.doc
                .selected_layer()
                .expect("selected paint layer")
                .procedural_graph,
            Some(graph.clone())
        );
        drop(app);

        let mut reopened = test_app();
        reopened.install_open_project(project::load(&temp.0).unwrap(), temp.0.clone(), false);
        assert!(reopened.pending_journal_recovery.is_some());
        reopened.resolve_journal_recovery(JournalRecoveryChoice::Restore);
        assert_eq!(
            reopened
                .doc
                .selected_layer()
                .expect("selected paint layer")
                .id,
            layer_id
        );
        assert_eq!(
            reopened
                .doc
                .selected_layer()
                .expect("selected paint layer")
                .pixels[&Channel::BaseColor]
                .get_pixel(3, 4),
            &image::Rgba([11, 22, 33, 255])
        );
        assert_eq!(
            reopened
                .doc
                .selected_layer()
                .expect("selected paint layer")
                .procedural_graph,
            Some(graph)
        );
    }

    #[test]
    fn undo_waits_for_durable_recovery_ack_before_mutating_history() {
        let temp = TempProject::new();
        let baseline = Document::new(64);
        let project_id = baseline.project_id;
        project::save(&baseline, &temp.0).unwrap();

        let mut app = test_app();
        app.install_open_project(baseline, temp.0.clone(), false);
        app.brush.color = [9, 19, 29, 255];
        app.perform(Action::Fill);
        let filled = app
            .doc
            .selected_layer()
            .expect("selected paint layer")
            .pixels[&Channel::BaseColor]
            .clone();
        assert!(app.history.can_undo());

        app.stop_recovery_writer().unwrap();
        let baseline_digest = project::file_digest(&temp.0).unwrap();
        let (journal, records) = Journal::open(
            recovery_journal_path(&temp.0),
            project_id,
            baseline_digest,
            JOURNAL_SCHEMA,
        )
        .unwrap();
        let committed_before_undo = records.len();
        let (writer, release_worker) = JournalWriter::new_paused(journal, 4, 8 * 1024 * 1024);
        app.recovery_journal = Some(writer);

        app.apply_history_impl(HistoryDirection::Undo, false);
        assert!(app.pending_history_change.is_some());
        assert!(app.history.can_undo());
        assert_eq!(
            app.doc
                .selected_layer()
                .expect("selected paint layer")
                .pixels[&Channel::BaseColor]
                .as_raw(),
            filled.as_raw()
        );
        let (_, records_while_pending) = Journal::open(
            recovery_journal_path(&temp.0),
            project_id,
            baseline_digest,
            JOURNAL_SCHEMA,
        )
        .unwrap();
        assert_eq!(records_while_pending.len(), committed_before_undo);

        release_worker.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while app.pending_history_change.is_some() {
            assert!(Instant::now() < deadline, "Undo acknowledgment timed out");
            app.advance_pending_history();
            std::thread::yield_now();
        }
        assert!(!app
            .doc
            .selected_layer()
            .expect("selected paint layer")
            .pixels
            .contains_key(&Channel::BaseColor));
        assert!(!app.history.can_undo());
        drop(app);
    }

    #[test]
    fn recovery_journals_layer_add_undo_and_redo_with_stable_ids() {
        let temp = TempProject::new();
        let baseline = Document::new(64);
        project::save(&baseline, &temp.0).unwrap();

        let mut app = test_app();
        app.install_open_project(baseline, temp.0.clone(), false);
        app.layer_command(LayerCommand::Add);
        let added_id = app.doc.selected_layer().expect("selected paint layer").id;
        let next_layer_id = app.doc.layer_tree.next_node_id;
        app.apply_history(HistoryDirection::Undo);
        assert_eq!(app.doc.layer_tree.roots.len(), 1);
        app.apply_history(HistoryDirection::Redo);
        assert_eq!(app.doc.layer_tree.roots.len(), 2);
        assert_eq!(
            app.doc.selected_layer().expect("selected paint layer").id,
            added_id
        );
        assert_eq!(app.doc.layer_tree.next_node_id, next_layer_id);
        drop(app);

        let mut reopened = test_app();
        reopened.install_open_project(project::load(&temp.0).unwrap(), temp.0.clone(), false);
        assert!(reopened.pending_journal_recovery.is_some());
        reopened.resolve_journal_recovery(JournalRecoveryChoice::Restore);
        assert_eq!(reopened.doc.layer_tree.roots.len(), 2);
        assert_eq!(
            reopened
                .doc
                .selected_layer()
                .expect("selected paint layer")
                .id,
            added_id
        );
        assert_eq!(reopened.doc.layer_tree.next_node_id, next_layer_id);
    }

    #[test]
    fn imported_layer_undo_redo_snapshot_survives_reopen() {
        let temp = TempProject::new();
        let texture_path = temp.0.with_extension("png");
        image::RgbaImage::from_pixel(8, 8, image::Rgba([35, 80, 145, 255]))
            .save(&texture_path)
            .unwrap();
        let baseline = Document::new(64);
        project::save(&baseline, &temp.0).unwrap();

        let mut app = test_app();
        app.install_open_project(baseline, temp.0.clone(), false);
        app.import_texture_path(&texture_path).unwrap();
        let layer_id = app.doc.selected_layer().expect("selected paint layer").id;
        let next_layer_id = app.doc.layer_tree.next_node_id;
        {
            let layer = app.doc.selected_layer_mut().expect("selected paint layer");
            layer.name = "Imported and edited".into();
            layer.visible = false;
            layer.opacity = 0.375;
            layer.blend = BlendMode::Multiply;
            layer.disabled_channels.insert(Channel::Metallic);
            layer.mask.put_pixel(5, 7, image::Luma([63]));
            layer.pixels_mut(Channel::Roughness, 64).put_pixel(
                9,
                11,
                image::Rgba([17, 34, 51, 68]),
            );
            layer.extensions.insert(
                "future_layer_field".into(),
                serde_json::json!({"preserve": [1, 2, 3]}),
            );
        }
        let expected = app
            .doc
            .selected_layer()
            .expect("selected paint layer")
            .clone();
        app.apply_history(HistoryDirection::Undo);
        assert_eq!(app.doc.layer_tree.roots.len(), 1);
        app.apply_history(HistoryDirection::Redo);
        assert_eq!(
            app.doc.selected_layer().expect("selected paint layer").id,
            layer_id
        );
        assert_eq!(
            app.doc
                .selected_layer()
                .expect("selected paint layer")
                .mask
                .as_raw(),
            expected.mask.as_raw()
        );
        assert_eq!(
            app.doc.selected_layer().expect("selected paint layer").name,
            expected.name
        );
        drop(app);

        let mut reopened = test_app();
        reopened.install_open_project(project::load(&temp.0).unwrap(), temp.0.clone(), false);
        assert!(reopened.pending_journal_recovery.is_some());
        reopened.resolve_journal_recovery(JournalRecoveryChoice::Restore);
        let restored = reopened.doc.selected_layer().expect("selected paint layer");
        assert_eq!(restored.id, layer_id);
        assert_eq!(reopened.doc.layer_tree.next_node_id, next_layer_id);
        assert_eq!(restored.name, "Imported and edited");
        assert!(!restored.visible);
        assert_eq!(restored.opacity, 0.375);
        assert_eq!(restored.blend, BlendMode::Multiply);
        assert!(restored.disabled_channels.contains(&Channel::Metallic));
        assert_eq!(restored.mask.as_raw(), expected.mask.as_raw());
        assert_eq!(
            restored.pixels[&Channel::BaseColor].as_raw(),
            expected.pixels[&Channel::BaseColor].as_raw()
        );
        assert_eq!(
            restored.pixels[&Channel::Roughness].as_raw(),
            expected.pixels[&Channel::Roughness].as_raw()
        );
        assert_eq!(restored.extensions, expected.extensions);
        let _ = fs::remove_file(texture_path);
    }

    #[test]
    fn imported_mesh_recovery_restores_mesh_bindings_and_texture_sets() {
        let temp = TempProject::new();
        let mut baseline = Document::new(64);
        let second_set = baseline.add_texture_set("Second Set").unwrap();
        baseline.texture_sets[0].layer_tree.roots[0]
            .pixels_mut(Channel::Roughness, 64)
            .put_pixel(7, 9, image::Rgba([23, 41, 59, 255]));
        let active_set = baseline.active_set_id;
        let retained_set = baseline.texture_sets[0].id;
        assert_eq!(active_set, second_set);
        let baseline_mesh = serde_json::to_vec(&baseline.mesh).unwrap();
        let baseline_bindings = baseline.material_bindings.clone();
        project::save(&baseline, &temp.0).unwrap();
        let project_id = baseline.project_id;

        let mut imported = Mesh::demo_cube();
        imported.materials = vec!["Stone".into(), "Trim".into()];
        imported.triangle_materials = (0..imported.indices.len() / 3)
            .map(|triangle| (triangle % 2) as u32)
            .collect();
        imported.extensions.insert(
            "future_mesh_metadata".into(),
            serde_json::Value::String("m".repeat(5 * 1024 * 1024)),
        );
        let expected_mesh = serde_json::to_vec(&imported).unwrap();

        let mut app = test_app();
        app.install_open_project(baseline, temp.0.clone(), false);
        assert!(app.install_imported_mesh(imported).unwrap());
        assert_eq!(app.doc.material_bindings, vec![Some(active_set); 2]);
        assert_eq!(app.doc.texture_sets.len(), 1);
        assert_eq!(app.doc.texture_sets[0].id, retained_set);
        app.apply_history(HistoryDirection::Undo);
        assert_eq!(serde_json::to_vec(&app.doc.mesh).unwrap(), baseline_mesh);
        assert_eq!(app.doc.material_bindings, baseline_bindings);
        app.apply_history(HistoryDirection::Redo);
        assert_eq!(serde_json::to_vec(&app.doc.mesh).unwrap(), expected_mesh);
        assert_eq!(app.doc.material_bindings, vec![Some(active_set); 2]);
        drop(app);

        let digest = project::file_digest(&temp.0).unwrap();
        let (_, records) = Journal::open(
            recovery_journal_path(&temp.0),
            project_id,
            digest,
            JOURNAL_SCHEMA,
        )
        .unwrap();
        let chunk_count = records
            .iter()
            .flat_map(|record| recovery::decode_event_batch_bounded(&record.payload).unwrap())
            .filter(|event| matches!(event, DocumentEvent::MeshReplaceChunk { .. }))
            .count();
        assert!(chunk_count >= 2, "large mesh must span recovery chunks");

        let mut reopened = test_app();
        reopened.install_open_project(project::load(&temp.0).unwrap(), temp.0.clone(), false);
        assert!(reopened.pending_journal_recovery.is_some());
        reopened.resolve_journal_recovery(JournalRecoveryChoice::Restore);
        assert_eq!(
            serde_json::to_vec(&reopened.doc.mesh).unwrap(),
            expected_mesh
        );
        assert_eq!(
            reopened.doc.mesh.extensions["future_mesh_metadata"],
            serde_json::Value::String("m".repeat(5 * 1024 * 1024))
        );
        assert_eq!(reopened.doc.material_bindings, vec![Some(active_set); 2]);
        assert_eq!(reopened.doc.texture_sets.len(), 1);
        assert_eq!(reopened.doc.texture_sets[0].id, retained_set);
        assert_eq!(
            reopened.doc.texture_sets[0].layer_tree.roots[0].pixels[&Channel::Roughness]
                .get_pixel(7, 9),
            &image::Rgba([23, 41, 59, 255])
        );

        let saved = temp.0.with_extension("restored.kalium");
        project::save(&reopened.doc, &saved).unwrap();
        let roundtrip = project::load(&saved).unwrap();
        assert_eq!(serde_json::to_vec(&roundtrip.mesh).unwrap(), expected_mesh);
        assert_eq!(
            roundtrip.mesh.extensions["future_mesh_metadata"],
            serde_json::Value::String("m".repeat(5 * 1024 * 1024))
        );
        assert_eq!(roundtrip.material_bindings, vec![Some(active_set); 2]);
        let _ = fs::remove_file(saved);
    }

    #[test]
    fn rejected_mesh_without_uvs_leaves_the_open_document_unchanged() {
        let dir =
            std::env::temp_dir().join(format!("kalium-app-missing-uv-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("no-uv.obj");
        std::fs::write(&path, "v 0 0 0\nv 1 0 0\nv 0 1 0\nf 1 2 3\n").unwrap();

        let mut app = test_app();
        let mesh_before = serde_json::to_value(&app.doc.mesh).unwrap();
        let bindings_before = app.doc.material_bindings.clone();
        let layer_ids_before = app
            .doc
            .layer_tree
            .roots
            .iter()
            .map(|layer| layer.id)
            .collect::<Vec<_>>();
        let selected_before = app.doc.layer_tree.selected_node_id;
        let unsaved_before = app.unsaved;
        let error = app.import_mesh_path(&path).unwrap_err().to_string();

        assert!(error.contains("add UVs before importing"), "{error}");
        assert_eq!(serde_json::to_value(&app.doc.mesh).unwrap(), mesh_before);
        assert_eq!(app.doc.material_bindings, bindings_before);
        assert_eq!(
            app.doc
                .layer_tree
                .roots
                .iter()
                .map(|layer| layer.id)
                .collect::<Vec<_>>(),
            layer_ids_before
        );
        assert_eq!(app.doc.layer_tree.selected_node_id, selected_before);
        assert_eq!(app.unsaved, unsaved_before);
        assert!(!app.history.can_undo());

        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn imported_mesh_undo_is_restored_from_journal_after_reopen() {
        let temp = TempProject::new();
        let mut baseline = Document::new(64);
        let metal_set = baseline.add_texture_set("Metal").unwrap();
        let default_set = baseline.texture_sets[0].id;
        baseline.mesh.materials = vec!["Body".into(), "Trim".into()];
        baseline.mesh.triangle_materials = (0..baseline.mesh.indices.len() / 3)
            .map(|triangle| (triangle % 2) as u32)
            .collect();
        baseline.material_bindings = vec![Some(default_set), Some(metal_set)];
        let original_mesh = serde_json::to_vec(&baseline.mesh).unwrap();
        let original_bindings = baseline.material_bindings.clone();
        project::save(&baseline, &temp.0).unwrap();

        let mut app = test_app();
        app.install_open_project(baseline, temp.0.clone(), false);
        let mut imported = Mesh::demo_cube();
        imported.name = "Undo target".into();
        imported.materials = vec!["Imported".into()];
        imported.triangle_materials.fill(0);
        assert!(app.install_imported_mesh(imported).unwrap());
        app.apply_history(HistoryDirection::Undo);
        assert_eq!(serde_json::to_vec(&app.doc.mesh).unwrap(), original_mesh);
        assert_eq!(app.doc.material_bindings, original_bindings);
        drop(app);

        let mut reopened = test_app();
        reopened.install_open_project(project::load(&temp.0).unwrap(), temp.0.clone(), false);
        assert!(reopened.pending_journal_recovery.is_some());
        reopened.resolve_journal_recovery(JournalRecoveryChoice::Restore);
        assert_eq!(
            serde_json::to_vec(&reopened.doc.mesh).unwrap(),
            original_mesh
        );
        assert_eq!(reopened.doc.material_bindings, original_bindings);
        assert_eq!(reopened.doc.texture_sets.len(), 1);
        assert_eq!(reopened.doc.texture_sets[0].id, default_set);
    }

    #[test]
    fn oversized_mesh_import_disables_recovery_but_keeps_import_available() {
        let temp = TempProject::new();
        let baseline = Document::new(64);
        let active_set = baseline.active_set_id;
        project::save(&baseline, &temp.0).unwrap();

        let mut app = test_app();
        app.install_open_project(project::load(&temp.0).unwrap(), temp.0.clone(), false);
        assert!(app.recovery_journal.is_some());

        let metadata = "m".repeat(super::RECOVERY_MESH_BYTES + 1024 * 1024);
        let mut imported = Mesh::demo_cube();
        imported.extensions.insert(
            "future_mesh_metadata".into(),
            serde_json::Value::String(metadata.clone()),
        );
        app.install_imported_mesh(imported).unwrap();

        assert_eq!(app.doc.mesh.extensions["future_mesh_metadata"], metadata);
        assert_eq!(app.doc.material_bindings, vec![Some(active_set)]);
        assert!(app.recovery_journal.is_none());
        assert!(app.status.contains("32 MiB recovery limit"));
        assert!(recovery_journal_disabled_path(&recovery_journal_path(&temp.0)).is_file());
        drop(app);

        let mut reopened = test_app();
        reopened.install_open_project(project::load(&temp.0).unwrap(), temp.0.clone(), false);
        assert!(reopened.pending_journal_recovery.is_none());
        assert_eq!(reopened.doc.mesh.name, baseline.mesh.name);
    }

    #[test]
    fn mesh_recovery_fingerprint_accepts_exact_32_mib_and_rejects_one_byte_more() {
        let mut mesh = Mesh::demo_cube();
        mesh.extensions.insert(
            "future_mesh_metadata".into(),
            serde_json::Value::String(String::new()),
        );
        let empty_extension_len = serde_json::to_vec(&mesh).unwrap().len();
        let exact_extension_len = super::RECOVERY_MESH_BYTES - empty_extension_len;
        mesh.extensions.insert(
            "future_mesh_metadata".into(),
            serde_json::Value::String("m".repeat(exact_extension_len)),
        );

        let (encoded_len, digest) = super::mesh_recovery_fingerprint(&mesh).unwrap();
        assert_eq!(encoded_len, super::RECOVERY_MESH_BYTES);
        assert_ne!(digest, [0; 32]);

        if let Some(serde_json::Value::String(extension)) =
            mesh.extensions.get_mut("future_mesh_metadata")
        {
            extension.push('m');
        } else {
            unreachable!();
        }
        let error = super::mesh_recovery_fingerprint(&mesh).unwrap_err();
        assert!(format!("{error:#}").contains("32 MiB recovery limit"));
    }

    #[test]
    fn large_layer_snapshot_is_batched_and_restores_all_channels() {
        let temp = TempProject::new();
        let mut baseline = Document::new(1024);
        let selected_before = baseline.selected_layer().expect("selected paint layer").id;
        baseline.add_layer().unwrap();
        let layer_id = baseline.selected_layer().expect("selected paint layer").id;
        {
            let layer = baseline.selected_layer_mut().expect("selected paint layer");
            layer.pixels.insert(
                Channel::BaseColor,
                image::RgbaImage::from_pixel(1024, 1024, image::Rgba([11, 23, 37, 255])),
            );
            layer.pixels.insert(
                Channel::Roughness,
                image::RgbaImage::from_pixel(1024, 1024, image::Rgba([31, 47, 63, 79])),
            );
            layer.pixels.insert(
                Channel::Normal,
                image::RgbaImage::from_pixel(1024, 1024, image::Rgba([97, 113, 129, 255])),
            );
        }
        let project_id = baseline.project_id;
        project::save(&baseline, &temp.0).unwrap();

        let mut app = test_app();
        app.install_open_project(baseline, temp.0.clone(), false);
        assert!(app
            .history
            .record_added_layer_with_payload(&app.doc, layer_id, selected_before));
        let expected = app
            .doc
            .selected_layer()
            .expect("selected paint layer")
            .clone();

        app.apply_history(HistoryDirection::Undo);
        app.apply_history(HistoryDirection::Redo);
        assert!(app.recovery_journal.is_some(), "{}", app.status);
        assert_eq!(
            app.doc
                .selected_layer()
                .expect("selected paint layer")
                .pixels[&Channel::Normal]
                .as_raw(),
            expected.pixels[&Channel::Normal].as_raw()
        );
        drop(app);

        let digest = project::file_digest(&temp.0).unwrap();
        let (_, records) = Journal::open(
            recovery_journal_path(&temp.0),
            project_id,
            digest,
            JOURNAL_SCHEMA,
        )
        .unwrap();
        assert!(
            records.len() >= 3,
            "snapshot should span durable journal records"
        );

        let mut reopened = test_app();
        reopened.install_open_project(project::load(&temp.0).unwrap(), temp.0.clone(), false);
        assert!(reopened.pending_journal_recovery.is_some());
        reopened.resolve_journal_recovery(JournalRecoveryChoice::Restore);
        let restored = reopened.doc.selected_layer().expect("selected paint layer");
        assert_eq!(restored.id, layer_id);
        assert_eq!(restored.mask.as_raw(), expected.mask.as_raw());
        assert_eq!(restored.pixels.len(), expected.pixels.len());
        for (channel, image) in &expected.pixels {
            assert_eq!(restored.pixels[channel].as_raw(), image.as_raw());
        }
    }

    #[test]
    fn interrupted_layer_snapshot_is_truncated_before_restore() {
        let temp = TempProject::new();
        let texture_path = temp.0.with_extension("png");
        image::RgbaImage::from_pixel(8, 8, image::Rgba([21, 44, 87, 255]))
            .save(&texture_path)
            .unwrap();
        project::save(&Document::new(64), &temp.0).unwrap();

        let mut app = test_app();
        app.install_open_project(project::load(&temp.0).unwrap(), temp.0.clone(), false);
        app.import_texture_path(&texture_path).unwrap();
        let imported_id = app.doc.selected_layer().expect("selected paint layer").id;
        app.apply_history(HistoryDirection::Undo);
        assert_eq!(app.doc.layer_tree.roots.len(), 1);
        let redo_events = app
            .history
            .prepare_recovery_events(&app.doc, app.doc.active_set_id, HistoryDirection::Redo)
            .expect("imported layer redo should produce snapshot events");
        assert!(matches!(
            redo_events.first(),
            Some(DocumentEvent::LayerSnapshotBegin { layer_id, .. }) if *layer_id == imported_id
        ));
        assert!(matches!(
            redo_events.last(),
            Some(DocumentEvent::LayerSelection { layer_id, .. }) if *layer_id == imported_id
        ));
        app.append_recovery_batch_inner(&redo_events[..redo_events.len() - 2], false);
        drop(app);

        let mut reopened = test_app();
        reopened.install_open_project(project::load(&temp.0).unwrap(), temp.0.clone(), false);
        assert!(reopened.pending_journal_recovery.is_some());
        reopened.resolve_journal_recovery(JournalRecoveryChoice::Restore);
        assert_eq!(reopened.doc.layer_tree.roots.len(), 1);
        assert!(!reopened
            .doc
            .layer_tree
            .roots
            .iter()
            .any(|layer| layer.id == imported_id));
        assert!(reopened.recovery_journal.is_some());
        let _ = fs::remove_file(texture_path);
    }

    #[test]
    fn empty_undo_or_redo_keeps_recovery_journal_enabled() {
        let temp = TempProject::new();
        project::save(&Document::new(64), &temp.0).unwrap();
        let mut app = test_app();
        app.install_open_project(project::load(&temp.0).unwrap(), temp.0.clone(), false);
        let journal_path = app.recovery_journal_path.clone().unwrap();

        app.apply_history(HistoryDirection::Undo);
        app.apply_history(HistoryDirection::Redo);

        assert!(app.recovery_journal.is_some());
        assert!(!recovery_journal_disabled_path(&journal_path).exists());
        assert!(!app.unsaved);
    }

    #[test]
    fn ao_bake_layer_feeds_roughness_graph_and_recovers_after_reopen() {
        let temp = TempProject::new();
        project::save(&Document::new(64), &temp.0).unwrap();
        let mut app = test_app();
        app.install_open_project(project::load(&temp.0).unwrap(), temp.0.clone(), false);
        let settings = crate::bake::AmbientOcclusionBakeSettings {
            sample_count: 5,
            max_ray_distance: 2.75,
            seed: 42,
        };
        app.update_ao_settings(settings);
        let expected = crate::bake::bake_ambient_occlusion(
            &app.doc.mesh,
            crate::bake::AmbientOcclusionBakeOptions {
                size: app.doc.size,
                bindings: &app.doc.material_bindings,
                active_set_id: app.doc.active_set_id,
                sample_count: settings.sample_count,
                max_ray_distance: settings.max_ray_distance,
                seed: settings.seed,
                working_budget_bytes: crate::document::MAX_DECODED_BYTES,
            },
            &std::sync::atomic::AtomicBool::new(false),
        )
        .unwrap()
        .occlusion_image()
        .unwrap();
        let ctx = egui::Context::default();
        app.start_ao_bake().unwrap();

        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        while app.ao_bake_job.is_some() && Instant::now() < deadline {
            app.advance_ao_bake_job(&ctx);
            std::thread::yield_now();
        }
        assert!(
            app.ao_bake_job.is_none(),
            "AO worker should finish promptly"
        );
        assert_eq!(app.doc.layer_tree.roots.len(), 2);
        assert_eq!(
            app.doc.selected_layer().expect("selected paint layer").name,
            "Ambient Occlusion"
        );
        let image = app
            .doc
            .selected_layer()
            .expect("selected paint layer")
            .pixels
            .get(&Channel::Occlusion)
            .unwrap();
        assert_eq!(image.dimensions(), (64, 64));
        assert_eq!(image, &expected);
        let expected = image.as_raw().clone();
        let ao_layer_id = app.doc.selected_layer().expect("selected paint layer").id;
        let consumer_layer_id = app.doc.layer_tree.roots[0].id;
        let before = crate::history::LayerProperties::capture(&app.doc.layer_tree.roots[0]);
        app.doc.layer_tree.roots[0].procedural_graph = Some(
            crate::procedural::ProceduralGraph::value_noise_times_occlusion(5, 6.0, ao_layer_id),
        );
        let after = crate::history::LayerProperties::capture(&app.doc.layer_tree.roots[0]);
        app.history
            .record_properties(consumer_layer_id, before, after);
        let graph_event = DocumentEvent::LayerProceduralGraphSet {
            set_id: app.doc.active_set_id,
            layer_id: consumer_layer_id,
            graph: app.doc.layer_tree.roots[0].procedural_graph.clone(),
        };
        app.change_with_recovery_events(&[graph_event]);
        let expected_roughness = app.doc.composite(Channel::Roughness);
        drop(app);

        let mut reopened = test_app();
        reopened.install_open_project(project::load(&temp.0).unwrap(), temp.0.clone(), false);
        assert!(reopened.pending_journal_recovery.is_some());
        reopened.resolve_journal_recovery(JournalRecoveryChoice::Restore);
        assert_eq!(reopened.ao_settings, settings);
        assert_eq!(reopened.doc.layer_tree.roots.len(), 2);
        assert_eq!(
            reopened
                .doc
                .selected_layer()
                .expect("selected paint layer")
                .name,
            "Ambient Occlusion"
        );
        assert_eq!(
            reopened
                .doc
                .selected_layer()
                .expect("selected paint layer")
                .pixels[&Channel::Occlusion]
                .as_raw(),
            &expected
        );
        assert_eq!(
            reopened.doc.layer_tree.roots[0].procedural_graph,
            Some(
                crate::procedural::ProceduralGraph::value_noise_times_occlusion(
                    5,
                    6.0,
                    ao_layer_id,
                )
            )
        );
        assert_eq!(
            reopened.doc.composite(Channel::Roughness),
            expected_roughness
        );
    }

    #[test]
    fn ao_layer_undo_redo_survives_reopen() {
        let temp = TempProject::new();
        project::save(&Document::new(64), &temp.0).unwrap();
        let mut app = test_app();
        app.install_open_project(project::load(&temp.0).unwrap(), temp.0.clone(), false);
        let ctx = egui::Context::default();
        app.start_ao_bake().unwrap();
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        while app.ao_bake_job.is_some() && Instant::now() < deadline {
            app.advance_ao_bake_job(&ctx);
            std::thread::yield_now();
        }
        assert!(app.ao_bake_job.is_none());
        assert_eq!(app.doc.layer_tree.roots.len(), 2);
        let ao_layer_id = app.doc.selected_layer().expect("selected paint layer").id;
        let expected_pixels = app
            .doc
            .selected_layer()
            .expect("selected paint layer")
            .pixels[&Channel::Occlusion]
            .as_raw()
            .clone();

        app.apply_history(HistoryDirection::Undo);
        assert_eq!(app.doc.layer_tree.roots.len(), 1);
        app.apply_history(HistoryDirection::Redo);
        assert_eq!(app.doc.layer_tree.roots.len(), 2);
        assert_eq!(
            app.doc.selected_layer().expect("selected paint layer").id,
            ao_layer_id
        );
        drop(app);

        let mut reopened = test_app();
        reopened.install_open_project(project::load(&temp.0).unwrap(), temp.0.clone(), false);
        assert!(reopened.pending_journal_recovery.is_some());
        reopened.resolve_journal_recovery(JournalRecoveryChoice::Restore);
        assert_eq!(reopened.doc.layer_tree.roots.len(), 2);
        assert_eq!(
            reopened
                .doc
                .selected_layer()
                .expect("selected paint layer")
                .id,
            ao_layer_id
        );
        assert_eq!(
            reopened
                .doc
                .selected_layer()
                .expect("selected paint layer")
                .name,
            "Ambient Occlusion"
        );
        assert_eq!(
            reopened
                .doc
                .selected_layer()
                .expect("selected paint layer")
                .pixels[&Channel::Occlusion]
                .as_raw(),
            &expected_pixels
        );
    }

    #[test]
    fn selection_after_select_then_reorder_is_restored_by_stable_id() {
        let temp = TempProject::new();
        let mut baseline = Document::new(64);
        baseline.add_layer().unwrap();
        baseline.add_layer().unwrap();
        project::save(&baseline, &temp.0).unwrap();

        let mut app = test_app();
        app.install_open_project(baseline, temp.0.clone(), false);
        app.doc.layer_tree.selected_node_id = app.doc.layer_tree.roots[2].id();
        let selected_id = app.doc.selected_layer().expect("selected paint layer").id;
        let source_id = app.doc.layer_tree.roots[2].id();
        let target_id = app.doc.layer_tree.roots[0].id();
        assert!(app.move_layer_to(source_id, target_id));
        assert_eq!(
            app.doc.selected_layer().expect("selected paint layer").id,
            selected_id
        );
        drop(app);

        let mut reopened = test_app();
        reopened.install_open_project(project::load(&temp.0).unwrap(), temp.0.clone(), false);
        reopened.resolve_journal_recovery(JournalRecoveryChoice::Restore);
        assert_eq!(
            reopened
                .doc
                .selected_layer()
                .expect("selected paint layer")
                .id,
            selected_id
        );
        assert_eq!(reopened.doc.layer_tree.selected_node_id, selected_id);
    }

    #[test]
    fn layer_properties_and_order_replay_from_journal() {
        let temp = TempProject::new();
        let mut baseline = Document::new(64);
        baseline.add_layer().unwrap();
        let ids_before = baseline
            .layer_tree
            .roots
            .iter()
            .map(|layer| layer.id)
            .collect::<Vec<_>>();
        project::save(&baseline, &temp.0).unwrap();

        let mut app = test_app();
        app.install_open_project(baseline, temp.0.clone(), false);
        let set_id = app.doc.active_set_id;
        let layer_id = app.doc.layer_tree.roots[0].id;
        let before = LayerRecoveryProperties::capture(&app.doc.layer_tree.roots[0]);
        {
            let layer = &mut app.doc.layer_tree.roots[0];
            layer.name = "Journaled layer".into();
            layer.visible = false;
            layer.opacity = 0.375;
            layer.blend = BlendMode::Multiply;
            layer.disabled_channels.insert(Channel::Metallic);
            layer.procedural_graph = Some(crate::procedural::ProceduralGraph::value_noise(57, 4.5));
        }
        let property_events = before.events_since(set_id, &app.doc.layer_tree.roots[0]);
        assert_eq!(property_events.len(), 6);
        app.change_with_recovery_events(&property_events);
        assert!(app.move_layer_to(ids_before[0], ids_before[1]));
        drop(app);

        let mut reopened = test_app();
        reopened.install_open_project(project::load(&temp.0).unwrap(), temp.0.clone(), false);
        assert!(reopened.pending_journal_recovery.is_some());
        reopened.resolve_journal_recovery(JournalRecoveryChoice::Restore);

        assert_eq!(reopened.doc.layer_tree.roots[0].id, ids_before[1]);
        assert_eq!(reopened.doc.layer_tree.roots[1].id, layer_id);
        let restored = &reopened.doc.layer_tree.roots[1];
        assert_eq!(restored.name, "Journaled layer");
        assert!(!restored.visible);
        assert_eq!(restored.opacity, 0.375);
        assert_eq!(restored.blend, BlendMode::Multiply);
        assert!(restored.disabled_channels.contains(&Channel::Metallic));
        assert_eq!(
            restored.procedural_graph,
            Some(crate::procedural::ProceduralGraph::value_noise(57, 4.5))
        );
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
            layer_id: app.doc.layer_tree.roots[0].id,
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
        let clean_generation =
            RecoveryGenerationStore::from_journal_path(&recovery_journal_path(&temp.0))
                .read_active(project_id, 1)
                .unwrap()
                .unwrap();
        assert_eq!(clean_generation.baseline_digest, baseline_digest);
        assert!(crate::recovery::scan(
            &clean_generation.journal_path,
            project_id,
            clean_generation.baseline_digest,
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
        let original: Vec<_> = app
            .doc
            .layer_tree
            .roots
            .iter()
            .map(|layer| layer.id)
            .collect();
        let selected_id = app.doc.selected_layer().expect("selected paint layer").id;
        app.unsaved = false;
        app.paint_dirty = false;
        app.model_dirty = false;

        assert!(app.move_layer_to(original[0], original[2]));
        assert_eq!(
            app.doc
                .layer_tree
                .roots
                .iter()
                .map(|layer| layer.id)
                .collect::<Vec<_>>(),
            vec![original[1], original[2], original[0]]
        );
        assert_eq!(
            app.doc.selected_layer().expect("selected paint layer").id,
            selected_id
        );
        assert!(app.unsaved && app.paint_dirty && app.model_dirty);

        assert!(app.history.undo(&mut app.doc));
        assert_eq!(
            app.doc
                .layer_tree
                .roots
                .iter()
                .map(|layer| layer.id)
                .collect::<Vec<_>>(),
            original
        );
        assert_eq!(
            app.doc.selected_layer().expect("selected paint layer").id,
            selected_id
        );
        assert!(app.history.redo(&mut app.doc));
        assert_eq!(app.doc.layer_tree.roots.last().unwrap().id, original[0]);
        assert_eq!(
            app.doc.selected_layer().expect("selected paint layer").id,
            selected_id
        );
    }

    #[test]
    fn layer_drop_self_and_out_of_range_targets_are_no_ops() {
        let mut app = test_app();
        let original: Vec<_> = app
            .doc
            .layer_tree
            .roots
            .iter()
            .map(|layer| layer.id)
            .collect();

        assert!(!app.move_layer_to(original[0], original[0]));
        assert!(!app.move_layer_to(original[0], u32::MAX));
        assert!(!app.move_layer_to(u32::MAX, original[0]));
        assert_eq!(
            app.doc
                .layer_tree
                .roots
                .iter()
                .map(|layer| layer.id)
                .collect::<Vec<_>>(),
            original
        );
        assert!(!app.unsaved);
        assert!(!app.history.undo(&mut app.doc));
    }

    #[test]
    fn egui_layers_create_ao_roughness_graph_with_stable_source_id() {
        use image::Rgba;

        let mut app = test_app();
        let source_layer_id = app.doc.selected_layer().expect("selected paint layer").id;
        let size = app.doc.size;
        app.doc
            .selected_layer_mut()
            .expect("selected paint layer")
            .pixels_mut(Channel::Occlusion, size)
            .put_pixel(3, 4, Rgba([128, 128, 128, 255]));

        let ctx = egui::Context::default();
        let screen_rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(640.0, 900.0));
        fn run_frame(
            app: &mut KaliumApp,
            ctx: &egui::Context,
            screen_rect: egui::Rect,
            events: Vec<egui::Event>,
        ) -> egui::FullOutput {
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
        }
        fn click(
            app: &mut KaliumApp,
            ctx: &egui::Context,
            screen_rect: egui::Rect,
            position: egui::Pos2,
        ) {
            run_frame(
                app,
                ctx,
                screen_rect,
                vec![
                    egui::Event::PointerMoved(position),
                    egui::Event::PointerButton {
                        pos: position,
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
            );
            run_frame(
                app,
                ctx,
                screen_rect,
                vec![
                    egui::Event::PointerMoved(position),
                    egui::Event::PointerButton {
                        pos: position,
                        button: egui::PointerButton::Primary,
                        pressed: false,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
            );
        }
        run_frame(&mut app, &ctx, screen_rect, Vec::new());
        let button_rect = ctx
            .data(|data| {
                data.get_temp::<egui::Rect>(egui::Id::new("test-layer-create-ao-roughness"))
            })
            .expect("AO-driven graph button should be shown for an Occlusion layer");
        click(&mut app, &ctx, screen_rect, button_rect.center());

        let graph = app
            .doc
            .selected_layer()
            .expect("selected paint layer")
            .procedural_graph
            .as_ref()
            .expect("AO-driven roughness graph should be created");
        assert_eq!(graph.occlusion_source_layer_id(), Some(source_layer_id));
        assert!(app.history.can_undo());
        assert!(app.unsaved);

        app.apply_history(super::HistoryDirection::Undo);
        assert!(app
            .doc
            .selected_layer()
            .expect("selected paint layer")
            .procedural_graph
            .is_none());
        app.apply_history(super::HistoryDirection::Redo);
        assert_eq!(
            app.doc
                .selected_layer()
                .expect("selected paint layer")
                .procedural_graph
                .as_ref()
                .and_then(|graph| graph.occlusion_source_layer_id()),
            Some(source_layer_id)
        );

        app.doc.add_layer().unwrap();
        let replacement_source_id = app.doc.selected_layer().expect("selected paint layer").id;
        let size = app.doc.size;
        app.doc
            .selected_layer_mut()
            .expect("selected paint layer")
            .pixels_mut(Channel::Occlusion, size)
            .put_pixel(7, 8, Rgba([200, 200, 200, 255]));
        app.doc.layer_tree.selected_node_id = app.doc.layer_tree.roots[0].id();
        run_frame(&mut app, &ctx, screen_rect, Vec::new());
        let combo_rect = ctx
            .data(|data| data.get_temp::<egui::Rect>(egui::Id::new("test-layer-ao-source-combo")))
            .expect("current AO source selector should be shown");
        click(&mut app, &ctx, screen_rect, combo_rect.center());
        run_frame(&mut app, &ctx, screen_rect, Vec::new());
        let replacement_rect = ctx
            .data(|data| {
                data.get_temp::<egui::Rect>(egui::Id::new(format!(
                    "test-layer-ao-source-option-{replacement_source_id}"
                )))
            })
            .expect("replacement Occlusion layer should be selectable");
        click(&mut app, &ctx, screen_rect, replacement_rect.center());
        assert_eq!(
            app.doc
                .selected_layer()
                .expect("selected paint layer")
                .procedural_graph
                .as_ref()
                .and_then(|graph| graph.occlusion_source_layer_id()),
            Some(replacement_source_id)
        );

        run_frame(&mut app, &ctx, screen_rect, Vec::new());
        let combo_rect = ctx
            .data(|data| data.get_temp::<egui::Rect>(egui::Id::new("test-layer-ao-source-combo")))
            .expect("updated AO source selector should be shown");
        click(&mut app, &ctx, screen_rect, combo_rect.center());
        run_frame(&mut app, &ctx, screen_rect, Vec::new());
        let remove_rect = ctx
            .data(|data| data.get_temp::<egui::Rect>(egui::Id::new("test-layer-ao-source-remove")))
            .expect("AO source removal option should be shown");
        click(&mut app, &ctx, screen_rect, remove_rect.center());
        assert_eq!(
            app.doc
                .selected_layer()
                .expect("selected paint layer")
                .procedural_graph
                .as_ref()
                .and_then(|graph| graph.occlusion_source_layer_id()),
            None
        );
    }

    #[test]
    fn egui_layer_groups_collapse_without_mutating_project_or_losing_selection() {
        use crate::document::Layer;
        use crate::layer_tree::LayerNode;

        let mut app = test_app();
        let size = app.doc.size;
        let outer = app.doc.layer_tree.allocate_group("Outer", size).unwrap();
        let outer_id = outer.properties.id;
        app.doc
            .layer_tree
            .insert(None, 1, LayerNode::Group(outer))
            .unwrap();
        let inner = app.doc.layer_tree.allocate_group("Inner", size).unwrap();
        let inner_id = inner.properties.id;
        app.doc
            .layer_tree
            .insert(Some(outer_id), 0, LayerNode::Group(inner))
            .unwrap();
        let nested_id = app.doc.layer_tree.next_node_id;
        app.doc
            .layer_tree
            .insert(
                Some(inner_id),
                0,
                LayerNode::Paint(Layer::new(nested_id, "Nested paint", size)),
            )
            .unwrap();
        app.doc.layer_tree.selected_node_id = nested_id;
        app.doc.validate().unwrap();
        let node_ids_before = app.doc.layer_tree.all_node_ids();
        let composite_before = app.doc.composite(Channel::BaseColor);
        let selected_before = app.doc.layer_tree.selected_node_id;
        let unsaved_before = app.unsaved;

        let ctx = egui::Context::default();
        let screen_rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(720.0, 1200.0));
        fn frame(
            app: &mut KaliumApp,
            ctx: &egui::Context,
            screen_rect: egui::Rect,
            events: Vec<egui::Event>,
        ) {
            let _ = ctx.run(
                egui::RawInput {
                    screen_rect: Some(screen_rect),
                    events,
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| app.layers(ui));
                },
            );
        }
        fn control_rect(ctx: &egui::Context, key: impl Into<String>) -> egui::Rect {
            ctx.data(|data| data.get_temp::<egui::Rect>(egui::Id::new(key.into())))
                .expect("expected visible Layers control")
        }
        fn clear_control_rect(ctx: &egui::Context, key: impl Into<String>) {
            ctx.data_mut(|data| data.remove::<egui::Rect>(egui::Id::new(key.into())));
        }
        fn click(
            app: &mut KaliumApp,
            ctx: &egui::Context,
            screen_rect: egui::Rect,
            position: egui::Pos2,
        ) {
            for pressed in [true, false] {
                frame(
                    app,
                    ctx,
                    screen_rect,
                    vec![
                        egui::Event::PointerMoved(position),
                        egui::Event::PointerButton {
                            pos: position,
                            button: egui::PointerButton::Primary,
                            pressed,
                            modifiers: egui::Modifiers::NONE,
                        },
                    ],
                );
            }
        }

        frame(&mut app, &ctx, screen_rect, Vec::new());
        let outer_toggle = control_rect(&ctx, format!("test-layer-group-toggle-{outer_id}"));
        assert!(ctx
            .data(|data| data.get_temp::<egui::Rect>(egui::Id::new(format!(
                "test-layer-row-select-{nested_id}"
            ))))
            .is_some());
        click(&mut app, &ctx, screen_rect, outer_toggle.center());
        assert!(app.collapsed_layer_groups.contains(&outer_id));
        assert_eq!(app.doc.layer_tree.selected_node_id, selected_before);
        clear_control_rect(&ctx, format!("test-layer-row-select-{nested_id}"));
        clear_control_rect(&ctx, format!("test-layer-row-select-{inner_id}"));
        frame(&mut app, &ctx, screen_rect, Vec::new());
        assert!(ctx
            .data(|data| data.get_temp::<egui::Rect>(egui::Id::new(format!(
                "test-layer-row-select-{nested_id}"
            ))))
            .is_none());
        assert!(ctx
            .data(|data| data
                .get_temp::<egui::Rect>(egui::Id::new(format!("test-layer-row-select-{inner_id}"))))
            .is_none());
        assert_eq!(app.doc.layer_tree.all_node_ids(), node_ids_before);
        assert_eq!(app.doc.composite(Channel::BaseColor), composite_before);
        assert_eq!(app.unsaved, unsaved_before);

        let outer_toggle = control_rect(&ctx, format!("test-layer-group-toggle-{outer_id}"));
        click(&mut app, &ctx, screen_rect, outer_toggle.center());
        assert!(!app.collapsed_layer_groups.contains(&outer_id));
        frame(&mut app, &ctx, screen_rect, Vec::new());
        let inner_toggle = control_rect(&ctx, format!("test-layer-group-toggle-{inner_id}"));
        click(&mut app, &ctx, screen_rect, inner_toggle.center());
        assert!(app.collapsed_layer_groups.contains(&inner_id));
        assert_eq!(app.doc.layer_tree.selected_node_id, selected_before);
        clear_control_rect(&ctx, format!("test-layer-row-select-{nested_id}"));
        frame(&mut app, &ctx, screen_rect, Vec::new());
        assert!(ctx
            .data(|data| data
                .get_temp::<egui::Rect>(egui::Id::new(format!("test-layer-row-select-{inner_id}"))))
            .is_some());
        assert!(ctx
            .data(|data| data.get_temp::<egui::Rect>(egui::Id::new(format!(
                "test-layer-row-select-{nested_id}"
            ))))
            .is_none());
        assert_eq!(app.doc.layer_tree.all_node_ids(), node_ids_before);
        assert_eq!(app.doc.composite(Channel::BaseColor), composite_before);
        assert_eq!(app.unsaved, unsaved_before);
    }

    #[test]
    fn egui_group_fill_workflow_saves_and_recovers_nested_edits() {
        use crate::layer_tree::LayerNode;

        let project_path = TempProject::new();
        let mut baseline = Document::new(64);
        baseline.add_layer().unwrap();
        let original_paint_id = baseline.layer_tree.roots[0].id();
        let sibling_paint_id = baseline.layer_tree.roots[1].id();
        project::save(&baseline, &project_path.0).unwrap();

        let mut app = test_app();
        app.install_open_project(baseline, project_path.0.clone(), false);
        let ctx = egui::Context::default();
        let screen_rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(720.0, 1200.0));
        fn frame(
            app: &mut KaliumApp,
            ctx: &egui::Context,
            screen_rect: egui::Rect,
            events: Vec<egui::Event>,
        ) {
            let _ = ctx.run(
                egui::RawInput {
                    screen_rect: Some(screen_rect),
                    events,
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| app.layers(ui));
                },
            );
        }
        fn control_rect(ctx: &egui::Context, key: impl Into<String>) -> egui::Rect {
            ctx.data(|data| data.get_temp::<egui::Rect>(egui::Id::new(key.into())))
                .expect("expected Layers control to be visible")
        }
        fn click(
            app: &mut KaliumApp,
            ctx: &egui::Context,
            screen_rect: egui::Rect,
            position: egui::Pos2,
        ) {
            for pressed in [true, false] {
                frame(
                    app,
                    ctx,
                    screen_rect,
                    vec![
                        egui::Event::PointerMoved(position),
                        egui::Event::PointerButton {
                            pos: position,
                            button: egui::PointerButton::Primary,
                            pressed,
                            modifiers: egui::Modifiers::NONE,
                        },
                    ],
                );
            }
        }
        fn drag_rows(
            app: &mut KaliumApp,
            ctx: &egui::Context,
            screen_rect: egui::Rect,
            source_id: u32,
            target_id: u32,
        ) {
            frame(app, ctx, screen_rect, Vec::new());
            let source_pos = control_rect(ctx, format!("test-layer-row-drag-{source_id}")).center();
            let target_pos = control_rect(ctx, format!("test-layer-row-drag-{target_id}")).center();
            frame(
                app,
                ctx,
                screen_rect,
                vec![
                    egui::Event::PointerMoved(source_pos),
                    egui::Event::PointerButton {
                        pos: source_pos,
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
            );
            frame(
                app,
                ctx,
                screen_rect,
                vec![egui::Event::PointerMoved(target_pos)],
            );
            frame(
                app,
                ctx,
                screen_rect,
                vec![
                    egui::Event::PointerMoved(target_pos),
                    egui::Event::PointerButton {
                        pos: target_pos,
                        button: egui::PointerButton::Primary,
                        pressed: false,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
            );
        }

        frame(&mut app, &ctx, screen_rect, Vec::new());
        click(
            &mut app,
            &ctx,
            screen_rect,
            control_rect(&ctx, "test-layer-action-Group").center(),
        );
        let group_id = app.doc.layer_tree.selected_node_id;
        assert!(matches!(
            app.doc.layer_tree.find_node(group_id),
            Some(LayerNode::Group(_))
        ));
        assert!(app.doc.selected_paint_layer().is_none());
        app.fill_uv([0.5, 0.5]);
        assert!(app.status.contains("Select a Paint layer"));
        app.paint_uv([0.5, 0.5]);
        app.finish_stroke();
        assert!(app
            .doc
            .paint_layer(original_paint_id)
            .unwrap()
            .pixels
            .is_empty());

        frame(&mut app, &ctx, screen_rect, Vec::new());
        let opacity_rect = control_rect(&ctx, "test-layer-opacity-slider");
        click(
            &mut app,
            &ctx,
            screen_rect,
            egui::pos2(
                opacity_rect.left() + opacity_rect.width() * 0.25,
                opacity_rect.center().y,
            ),
        );
        let LayerNode::Group(group) = app.doc.layer_tree.find_node(group_id).unwrap() else {
            panic!("expected selected Group");
        };
        assert!(group.properties.opacity < 0.75);
        let first_group_opacity = group.properties.opacity;

        drag_rows(&mut app, &ctx, screen_rect, original_paint_id, group_id);
        assert_eq!(
            app.doc.layer_tree.parent_id(original_paint_id),
            Some(Some(group_id))
        );
        drag_rows(&mut app, &ctx, screen_rect, group_id, original_paint_id);
        assert!(app.status.contains("Cannot move this layer"));
        assert_eq!(app.doc.layer_tree.parent_id(group_id), Some(None));
        app.doc.validate().unwrap();
        drag_rows(
            &mut app,
            &ctx,
            screen_rect,
            original_paint_id,
            sibling_paint_id,
        );
        assert_eq!(app.doc.layer_tree.parent_id(original_paint_id), Some(None));
        drag_rows(&mut app, &ctx, screen_rect, original_paint_id, group_id);
        assert_eq!(
            app.doc.layer_tree.parent_id(original_paint_id),
            Some(Some(group_id))
        );

        frame(&mut app, &ctx, screen_rect, Vec::new());
        click(
            &mut app,
            &ctx,
            screen_rect,
            control_rect(&ctx, "test-layer-action-Fill").center(),
        );
        let fill_id = app.doc.layer_tree.selected_node_id;
        assert_eq!(app.doc.layer_tree.parent_id(fill_id), Some(Some(group_id)));
        assert!(matches!(
            app.doc.layer_tree.find_node(fill_id),
            Some(LayerNode::Fill(_))
        ));

        ctx.data_mut(|data| {
            data.remove::<egui::Rect>(egui::Id::new("test-layer-remove-fill-value"));
        });
        frame(&mut app, &ctx, screen_rect, Vec::new());
        assert!(control_rect(&ctx, "test-layer-fill-red-slider").is_positive());
        assert!(
            ctx.data(|data| {
                data.get_temp::<egui::Rect>(egui::Id::new("test-layer-remove-fill-value"))
            })
            .is_none(),
            "a one-channel Fill must not offer removal of its final value"
        );
        let before_clear = match app.doc.layer_tree.find_node(fill_id).unwrap() {
            LayerNode::Fill(fill) => fill.values[&Channel::BaseColor],
            _ => unreachable!(),
        };
        app.perform(Action::ClearChannel);
        assert!(app.status.contains("must keep at least one channel value"));
        assert_eq!(
            match app.doc.layer_tree.find_node(fill_id).unwrap() {
                LayerNode::Fill(fill) => fill.values[&Channel::BaseColor],
                _ => unreachable!(),
            },
            before_clear
        );
        let red_slider = control_rect(&ctx, "test-layer-fill-red-slider");
        click(
            &mut app,
            &ctx,
            screen_rect,
            egui::pos2(
                red_slider.left() + red_slider.width() * 0.25,
                red_slider.center().y,
            ),
        );
        let initial_fill_value = match app.doc.layer_tree.find_node(fill_id).unwrap() {
            LayerNode::Fill(fill) => fill.values[&Channel::BaseColor],
            _ => unreachable!(),
        };
        assert_ne!(initial_fill_value, before_clear);
        assert!(app.recovery_journal.is_some(), "status: {}", app.status);
        app.save_project_to_path(project_path.0.clone()).unwrap();

        let saved = project::load(&project_path.0).unwrap();
        assert_eq!(saved.layer_tree.selected_node_id, fill_id);
        assert_eq!(saved.layer_tree.parent_id(fill_id), Some(Some(group_id)));
        assert_eq!(
            saved.layer_tree.composite(saved.size, Channel::BaseColor),
            app.doc
                .layer_tree
                .composite(app.doc.size, Channel::BaseColor)
        );
        let saved_group_opacity = match saved.layer_tree.find_node(group_id).unwrap() {
            LayerNode::Group(group) => group.properties.opacity,
            _ => unreachable!(),
        };
        assert_eq!(saved_group_opacity, first_group_opacity);

        let selected_before_move = app.doc.layer_tree.selected_node_id;
        let nested_composite = app
            .doc
            .layer_tree
            .composite(app.doc.size, Channel::BaseColor);
        drag_rows(
            &mut app,
            &ctx,
            screen_rect,
            original_paint_id,
            sibling_paint_id,
        );
        assert_eq!(app.doc.layer_tree.parent_id(original_paint_id), Some(None));
        assert_eq!(
            app.doc.layer_tree.selected_node_id, selected_before_move,
            "dropping a node should preserve its selection"
        );
        assert_eq!(
            app.doc
                .layer_tree
                .path_to(original_paint_id)
                .unwrap()
                .last(),
            Some(&0)
        );
        app.apply_history(HistoryDirection::Undo);
        assert_eq!(
            app.doc.layer_tree.parent_id(original_paint_id),
            Some(Some(group_id))
        );
        assert_eq!(
            app.doc
                .layer_tree
                .path_to(original_paint_id)
                .unwrap()
                .last(),
            Some(&0)
        );
        assert_eq!(app.doc.layer_tree.selected_node_id, selected_before_move);
        assert_eq!(
            app.doc
                .layer_tree
                .composite(app.doc.size, Channel::BaseColor),
            nested_composite
        );
        app.apply_history(HistoryDirection::Redo);
        assert_eq!(app.doc.layer_tree.parent_id(original_paint_id), Some(None));
        assert_eq!(
            app.doc
                .layer_tree
                .path_to(original_paint_id)
                .unwrap()
                .last(),
            Some(&0)
        );
        assert_eq!(app.doc.layer_tree.selected_node_id, selected_before_move);
        assert_eq!(
            app.doc
                .layer_tree
                .composite(app.doc.size, Channel::BaseColor),
            nested_composite
        );
        drag_rows(&mut app, &ctx, screen_rect, original_paint_id, group_id);
        assert_eq!(
            app.doc.layer_tree.parent_id(original_paint_id),
            Some(Some(group_id))
        );

        frame(&mut app, &ctx, screen_rect, Vec::new());
        click(
            &mut app,
            &ctx,
            screen_rect,
            control_rect(&ctx, format!("test-layer-row-select-{group_id}")).center(),
        );
        frame(&mut app, &ctx, screen_rect, Vec::new());
        let opacity_rect = control_rect(&ctx, "test-layer-opacity-slider");
        click(
            &mut app,
            &ctx,
            screen_rect,
            egui::pos2(
                opacity_rect.left() + opacity_rect.width() * 0.4,
                opacity_rect.center().y,
            ),
        );
        let recovered_group_opacity = match app.doc.layer_tree.find_node(group_id).unwrap() {
            LayerNode::Group(group) => group.properties.opacity,
            _ => unreachable!(),
        };
        assert_ne!(recovered_group_opacity, saved_group_opacity);
        frame(&mut app, &ctx, screen_rect, Vec::new());
        click(
            &mut app,
            &ctx,
            screen_rect,
            control_rect(&ctx, format!("test-layer-row-select-{fill_id}")).center(),
        );
        frame(&mut app, &ctx, screen_rect, Vec::new());
        let red_slider = control_rect(&ctx, "test-layer-fill-red-slider");
        click(
            &mut app,
            &ctx,
            screen_rect,
            egui::pos2(
                red_slider.left() + red_slider.width() * 0.4,
                red_slider.center().y,
            ),
        );
        let recovered_fill_value = match app.doc.layer_tree.find_node(fill_id).unwrap() {
            LayerNode::Fill(fill) => fill.values[&Channel::BaseColor],
            _ => unreachable!(),
        };
        assert_ne!(recovered_fill_value, initial_fill_value);
        let recovered_composite = app
            .doc
            .layer_tree
            .composite(app.doc.size, Channel::BaseColor);
        drop(app);

        let mut reopened = test_app();
        reopened.install_open_project(
            project::load(&project_path.0).unwrap(),
            project_path.0.clone(),
            false,
        );
        assert!(reopened.pending_journal_recovery.is_some());
        reopened.resolve_journal_recovery(JournalRecoveryChoice::Restore);
        assert_eq!(reopened.doc.layer_tree.selected_node_id, fill_id);
        assert_eq!(
            reopened.doc.layer_tree.parent_id(fill_id),
            Some(Some(group_id))
        );
        assert_eq!(
            reopened.doc.layer_tree.parent_id(original_paint_id),
            Some(Some(group_id))
        );
        let LayerNode::Group(restored_group) = reopened
            .doc
            .layer_tree
            .find_node(group_id)
            .expect("restored Group")
        else {
            unreachable!()
        };
        assert_eq!(
            restored_group
                .children
                .iter()
                .map(LayerNode::id)
                .collect::<Vec<_>>(),
            vec![fill_id, original_paint_id]
        );
        assert_eq!(
            match reopened.doc.layer_tree.find_node(fill_id).unwrap() {
                LayerNode::Fill(fill) => fill.values[&Channel::BaseColor],
                _ => unreachable!(),
            },
            recovered_fill_value
        );
        assert_eq!(
            match reopened.doc.layer_tree.find_node(group_id).unwrap() {
                LayerNode::Group(group) => group.properties.opacity,
                _ => unreachable!(),
            },
            recovered_group_opacity
        );
        assert_eq!(
            reopened
                .doc
                .layer_tree
                .composite(reopened.doc.size, Channel::BaseColor),
            recovered_composite
        );
        reopened.doc.validate().unwrap();
    }

    #[test]
    fn spatial_mask_brush_on_group_invalidates_composites_and_is_undoable() {
        use crate::layer_tree::{LayerGroup, LayerNode, LayerNodeProperties};

        let mut app = test_app();
        let id = app.doc.layer_tree.next_node_id;
        let properties = LayerNodeProperties::new(id, "Mask target", app.doc.size).unwrap();
        app.doc.layer_tree.roots.push(LayerNode::Group(LayerGroup {
            properties,
            children: Vec::new(),
        }));
        app.doc.layer_tree.next_node_id = id + 1;
        app.doc.select_node(id).unwrap();
        app.brush.mode = BrushMode::MaskBlack;
        app.brush.radius = 2;
        app.brush.hardness = 1.0;

        app.paint_uv([0.5, 0.5]);

        assert!(app.unsaved);
        assert!(app.paint_dirty);
        assert!(
            app.doc
                .layer_tree
                .find_node(id)
                .unwrap()
                .mask()
                .get_pixel(32, 32)
                .0[0]
                < 255
        );
        app.finish_stroke();
        assert!(app.history.undo(&mut app.doc));
        assert_eq!(
            app.doc
                .layer_tree
                .find_node(id)
                .unwrap()
                .mask()
                .get_pixel(32, 32)
                .0,
            [255]
        );
    }

    #[test]
    fn egui_fill_and_group_channel_toggles_persist_and_change_composite() {
        use crate::layer_tree::LayerNode;

        let temp = TempProject::new();
        let mut app = test_app();
        let ctx = egui::Context::default();
        let screen_rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(720.0, 1200.0));
        fn frame(
            app: &mut KaliumApp,
            ctx: &egui::Context,
            screen_rect: egui::Rect,
            events: Vec<egui::Event>,
        ) {
            let _ = ctx.run(
                egui::RawInput {
                    screen_rect: Some(screen_rect),
                    events,
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| app.layers(ui));
                },
            );
        }
        fn control_rect(ctx: &egui::Context, key: impl Into<String>) -> egui::Rect {
            ctx.data(|data| data.get_temp::<egui::Rect>(egui::Id::new(key.into())))
                .expect("expected Layers control to be visible")
        }
        fn click(
            app: &mut KaliumApp,
            ctx: &egui::Context,
            screen_rect: egui::Rect,
            position: egui::Pos2,
        ) {
            for pressed in [true, false] {
                frame(
                    app,
                    ctx,
                    screen_rect,
                    vec![
                        egui::Event::PointerMoved(position),
                        egui::Event::PointerButton {
                            pos: position,
                            button: egui::PointerButton::Primary,
                            pressed,
                            modifiers: egui::Modifiers::NONE,
                        },
                    ],
                );
            }
        }
        fn click_channel(
            app: &mut KaliumApp,
            ctx: &egui::Context,
            screen_rect: egui::Rect,
            node_id: u32,
        ) {
            frame(app, ctx, screen_rect, Vec::new());
            let rect = control_rect(
                ctx,
                format!(
                    "test-layer-channel-{node_id}-{}",
                    Channel::BaseColor.label()
                ),
            );
            click(app, ctx, screen_rect, rect.center());
        }

        frame(&mut app, &ctx, screen_rect, Vec::new());
        click(
            &mut app,
            &ctx,
            screen_rect,
            control_rect(&ctx, "test-layer-action-Group").center(),
        );
        let group_id = app.doc.layer_tree.selected_node_id;
        assert!(matches!(
            app.doc.layer_tree.find_node(group_id),
            Some(LayerNode::Group(_))
        ));
        frame(&mut app, &ctx, screen_rect, Vec::new());
        click(
            &mut app,
            &ctx,
            screen_rect,
            control_rect(&ctx, "test-layer-action-Fill").center(),
        );
        let fill_id = app.doc.layer_tree.selected_node_id;
        assert_eq!(app.doc.layer_tree.parent_id(fill_id), Some(Some(group_id)));

        let enabled_composite = app
            .doc
            .layer_tree
            .composite(app.doc.size, Channel::BaseColor);
        click_channel(&mut app, &ctx, screen_rect, fill_id);
        let LayerNode::Fill(fill) = app.doc.layer_tree.find_node(fill_id).unwrap() else {
            panic!("expected selected Fill");
        };
        assert!(fill
            .properties
            .disabled_channels
            .contains(&Channel::BaseColor));
        let fill_disabled_composite = app
            .doc
            .layer_tree
            .composite(app.doc.size, Channel::BaseColor);
        assert_ne!(fill_disabled_composite, enabled_composite);
        project::save(&app.doc, &temp.0).unwrap();
        let saved = project::load(&temp.0).unwrap();
        let LayerNode::Fill(saved_fill) = saved.layer_tree.find_node(fill_id).unwrap() else {
            panic!("expected persisted Fill");
        };
        assert!(saved_fill
            .properties
            .disabled_channels
            .contains(&Channel::BaseColor));

        click_channel(&mut app, &ctx, screen_rect, fill_id);
        assert_eq!(
            app.doc
                .layer_tree
                .composite(app.doc.size, Channel::BaseColor),
            enabled_composite
        );
        frame(&mut app, &ctx, screen_rect, Vec::new());
        click(
            &mut app,
            &ctx,
            screen_rect,
            control_rect(&ctx, format!("test-layer-row-select-{group_id}")).center(),
        );
        click_channel(&mut app, &ctx, screen_rect, group_id);
        let LayerNode::Group(group) = app.doc.layer_tree.find_node(group_id).unwrap() else {
            panic!("expected selected Group");
        };
        assert!(group
            .properties
            .disabled_channels
            .contains(&Channel::BaseColor));
        let group_disabled_composite = app
            .doc
            .layer_tree
            .composite(app.doc.size, Channel::BaseColor);
        assert_ne!(group_disabled_composite, enabled_composite);
        project::save(&app.doc, &temp.0).unwrap();
        let saved = project::load(&temp.0).unwrap();
        let LayerNode::Group(saved_group) = saved.layer_tree.find_node(group_id).unwrap() else {
            panic!("expected persisted Group");
        };
        assert!(saved_group
            .properties
            .disabled_channels
            .contains(&Channel::BaseColor));
    }

    #[test]
    fn egui_selected_layer_solo_is_preview_only_and_restores_composite() {
        use crate::layer_tree::{FillLayer, LayerNode, LayerNodeProperties};
        use image::Luma;
        use std::collections::HashMap;

        let mut app = test_app();
        let mut group = app.doc.layer_tree.allocate_group("Solo group", 64).unwrap();
        group.properties.opacity = 0.5;
        group.properties.mask.put_pixel(8, 9, Luma([128]));
        let group_id = group.properties.id;
        let mut target_values = HashMap::new();
        target_values.insert(Channel::BaseColor, [240, 20, 30, 255]);
        let mut target_properties =
            LayerNodeProperties::new(app.doc.layer_tree.next_node_id, "Solo target", 64).unwrap();
        target_properties.opacity = 0.75;
        let target_id = target_properties.id;
        let target = FillLayer {
            properties: target_properties,
            values: target_values,
        };
        let mut sibling_values = HashMap::new();
        sibling_values.insert(Channel::BaseColor, [10, 230, 40, 255]);
        let sibling = FillLayer {
            properties: LayerNodeProperties::new(target_id + 1, "Sibling", 64).unwrap(),
            values: sibling_values,
        };
        group
            .children
            .extend([LayerNode::Fill(target), LayerNode::Fill(sibling)]);
        app.doc
            .layer_tree
            .insert(None, 1, LayerNode::Group(group))
            .unwrap();
        app.doc.layer_tree.select(target_id).unwrap();
        let full = app.doc.layer_tree.composite(64, Channel::BaseColor);
        let target_solo = app
            .doc
            .layer_tree
            .composite_isolated(target_id, 64, Channel::BaseColor)
            .unwrap();
        let group_solo = app
            .doc
            .layer_tree
            .composite_isolated(group_id, 64, Channel::BaseColor)
            .unwrap();
        let original_visibility = app
            .doc
            .layer_tree
            .all_node_ids()
            .into_iter()
            .map(|id| {
                let visible = match app.doc.layer_tree.find_node(id).unwrap() {
                    LayerNode::Paint(layer) => layer.visible,
                    LayerNode::Fill(fill) => fill.properties.visible,
                    LayerNode::Group(group) => group.properties.visible,
                };
                (id, visible)
            })
            .collect::<Vec<_>>();
        app.unsaved = false;
        app.paint_dirty = true;
        app.model_dirty = true;

        let ctx = egui::Context::default();
        let screen_rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(720.0, 1200.0));
        fn frame(
            app: &mut KaliumApp,
            ctx: &egui::Context,
            screen_rect: egui::Rect,
            events: Vec<egui::Event>,
        ) {
            let _ = ctx.run(
                egui::RawInput {
                    screen_rect: Some(screen_rect),
                    events,
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| app.layers(ui));
                },
            );
        }
        fn control_rect(ctx: &egui::Context, key: impl Into<String>) -> egui::Rect {
            ctx.data(|data| data.get_temp::<egui::Rect>(egui::Id::new(key.into())))
                .expect("expected Layers control to be visible")
        }
        fn click(
            app: &mut KaliumApp,
            ctx: &egui::Context,
            screen_rect: egui::Rect,
            position: egui::Pos2,
        ) {
            for pressed in [true, false] {
                frame(
                    app,
                    ctx,
                    screen_rect,
                    vec![
                        egui::Event::PointerMoved(position),
                        egui::Event::PointerButton {
                            pos: position,
                            button: egui::PointerButton::Primary,
                            pressed,
                            modifiers: egui::Modifiers::NONE,
                        },
                    ],
                );
            }
        }

        frame(&mut app, &ctx, screen_rect, Vec::new());
        click(
            &mut app,
            &ctx,
            screen_rect,
            control_rect(&ctx, "test-layer-solo").center(),
        );
        assert!(app.solo_layer);
        assert!(!app.unsaved, "solo must not dirty project content");
        app.refresh_textures(&ctx);
        assert_eq!(
            app.material
                .as_ref()
                .unwrap()
                .active()
                .image(Channel::BaseColor),
            &target_solo
        );
        assert_eq!(app.doc.layer_tree.composite(64, Channel::BaseColor), full);

        click(
            &mut app,
            &ctx,
            screen_rect,
            control_rect(&ctx, "test-layer-solo").center(),
        );
        assert!(!app.solo_layer);
        assert!(!app.unsaved);
        app.refresh_textures(&ctx);
        assert_eq!(
            app.material
                .as_ref()
                .unwrap()
                .active()
                .image(Channel::BaseColor),
            &full
        );

        click(
            &mut app,
            &ctx,
            screen_rect,
            control_rect(&ctx, format!("test-layer-row-select-{group_id}")).center(),
        );
        assert_eq!(app.doc.layer_tree.selected_node_id, group_id);
        app.unsaved = false;
        frame(&mut app, &ctx, screen_rect, Vec::new());
        click(
            &mut app,
            &ctx,
            screen_rect,
            control_rect(&ctx, "test-layer-solo").center(),
        );
        assert!(!app.unsaved);
        app.refresh_textures(&ctx);
        assert_eq!(
            app.material
                .as_ref()
                .unwrap()
                .active()
                .image(Channel::BaseColor),
            &group_solo
        );
        assert_eq!(
            original_visibility,
            app.doc
                .layer_tree
                .all_node_ids()
                .into_iter()
                .map(|id| {
                    let visible = match app.doc.layer_tree.find_node(id).unwrap() {
                        LayerNode::Paint(layer) => layer.visible,
                        LayerNode::Fill(fill) => fill.properties.visible,
                        LayerNode::Group(group) => group.properties.visible,
                    };
                    (id, visible)
                })
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn egui_layer_row_drag_drops_on_another_row() {
        let mut app = test_app();
        app.doc.add_layer().unwrap();
        app.doc.add_layer().unwrap();
        let original: Vec<_> = app
            .doc
            .layer_tree
            .roots
            .iter()
            .map(|layer| layer.id)
            .collect();
        let source_id = original[0];
        let target_id = original[2];
        let selected_id = app.doc.selected_layer().expect("selected paint layer").id;
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
            .data(|data| {
                data.get_temp::<egui::Rect>(egui::Id::new(format!(
                    "test-layer-row-drag-{source_id}"
                )))
            })
            .unwrap();
        let target_rect = ctx
            .data(|data| {
                data.get_temp::<egui::Rect>(egui::Id::new(format!(
                    "test-layer-row-drag-{target_id}"
                )))
            })
            .unwrap();
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
                .layer_tree
                .roots
                .iter()
                .map(|layer| layer.id)
                .collect::<Vec<_>>(),
            vec![original[1], original[2], original[0]]
        );
        assert_eq!(
            app.doc.selected_layer().expect("selected paint layer").id,
            selected_id
        );
        assert!(app.unsaved);
        assert!(app.history.undo(&mut app.doc));
        assert_eq!(
            app.doc
                .layer_tree
                .roots
                .iter()
                .map(|layer| layer.id)
                .collect::<Vec<_>>(),
            original
        );
        assert!(app.history.redo(&mut app.doc));
        assert_eq!(app.doc.layer_tree.roots.last().unwrap().id, source_id);
    }

    #[test]
    fn egui_layer_selection_marks_project_dirty_and_survives_save_and_recovery() {
        let temp = TempProject::new();
        let mut app = test_app();
        app.doc.add_layer().unwrap();
        let layer_ids: Vec<_> = app
            .doc
            .layer_tree
            .roots
            .iter()
            .map(|layer| layer.id)
            .collect();
        assert_eq!(
            app.doc.selected_layer().expect("selected paint layer").id,
            layer_ids[1]
        );
        app.save_project_to_path(temp.0.clone()).unwrap();
        assert!(!app.unsaved);

        let ctx = egui::Context::default();
        let screen_rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(480.0, 640.0));
        {
            let mut run_frame = |events| {
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
            run_frame(Vec::new());
            let target_row = ctx
                .data(|data| {
                    data.get_temp::<egui::Rect>(egui::Id::new(format!(
                        "test-layer-row-select-{}",
                        layer_ids[0]
                    )))
                })
                .unwrap();
            let position = target_row.center();
            for pressed in [true, false] {
                run_frame(vec![
                    egui::Event::PointerMoved(position),
                    egui::Event::PointerButton {
                        pos: position,
                        button: egui::PointerButton::Primary,
                        pressed,
                        modifiers: egui::Modifiers::NONE,
                    },
                ]);
            }
        }
        assert_eq!(
            app.doc.selected_layer().expect("selected paint layer").id,
            layer_ids[0]
        );
        assert!(app.unsaved, "selection is serialized project state");
        assert!(app.recovery_journal.is_some());

        app.save_project_to_path(temp.0.clone()).unwrap();
        assert!(!app.unsaved);
        let saved = project::load(&temp.0).unwrap();
        assert_eq!(
            saved.selected_layer().expect("selected paint layer").id,
            layer_ids[0]
        );

        app.select_layer_by_id(layer_ids[1]);
        drop(app);
        let mut recovered = test_app();
        recovered.install_open_project(project::load(&temp.0).unwrap(), temp.0.clone(), false);
        assert!(recovered.pending_journal_recovery.is_some());
        recovered.resolve_journal_recovery(JournalRecoveryChoice::Restore);
        assert_eq!(
            recovered
                .doc
                .selected_layer()
                .expect("selected paint layer")
                .id,
            layer_ids[1]
        );
        assert!(recovered.unsaved);
    }

    #[test]
    fn egui_texture_set_list_add_button_creates_set_and_preserves_existing_content() {
        use image::{Luma, Rgba};

        let mut app = test_app();
        let original_set_id = app.doc.active_set_id;
        let original_layer_id = app.doc.selected_layer().expect("selected paint layer").id;
        let original_set_name = app.doc.active_set_name.clone();
        let pixel = Rgba([12, 34, 56, 78]);
        app.doc
            .selected_layer_mut()
            .expect("selected paint layer")
            .pixels_mut(Channel::BaseColor, 64)
            .put_pixel(4, 5, pixel);
        app.doc
            .selected_layer_mut()
            .expect("selected paint layer")
            .mask
            .put_pixel(6, 7, Luma([123]));
        let expected_new_set_id = app.doc.next_set_id;
        let expected_new_set_name = format!("Texture Set {expected_new_set_id}");
        app.unsaved = false;
        app.paint_dirty = false;
        app.model_dirty = false;

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
                    egui::CentralPanel::default().show(ctx, |ui| app.texture_sets(ui));
                },
            )
        };

        run(Vec::new());
        let add_set_rect = ctx
            .data(|data| {
                data.get_temp::<egui::Rect>(egui::Id::new("texture-set-list-add-button-rect"))
            })
            .expect("Texture Set List add button geometry should be available");
        let add_set_pos = add_set_rect.center();

        run(vec![
            egui::Event::PointerMoved(add_set_pos),
            egui::Event::PointerButton {
                pos: add_set_pos,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            },
        ]);
        run(vec![
            egui::Event::PointerMoved(add_set_pos),
            egui::Event::PointerButton {
                pos: add_set_pos,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            },
        ]);

        assert_eq!(app.doc.texture_set_count(), 2);
        assert_eq!(app.doc.active_set_id, expected_new_set_id);
        assert_eq!(app.doc.active_set_name, expected_new_set_name);
        assert_eq!(app.texture_set_name_input, expected_new_set_name);
        assert!(app.unsaved);
        assert!(app.paint_dirty && app.model_dirty);

        let original_set = app
            .doc
            .texture_sets
            .iter()
            .find(|set| set.id == original_set_id)
            .expect("the original texture set should remain present");
        assert_eq!(original_set.name, original_set_name);
        assert_eq!(original_set.layer_tree.roots.len(), 1);
        assert_eq!(original_set.layer_tree.roots[0].id, original_layer_id);
        assert_eq!(
            original_set.layer_tree.roots[0].pixels[&Channel::BaseColor].get_pixel(4, 5),
            &pixel
        );
        assert_eq!(
            original_set.layer_tree.roots[0].mask.get_pixel(6, 7).0,
            [123]
        );
    }

    #[test]
    fn egui_texture_set_list_selects_inactive_set_and_preserves_independent_content() {
        use image::{Luma, Rgba};

        let mut app = test_app();
        let original_set_id = app.doc.active_set_id;
        let original_set_name = app.doc.active_set_name.clone();
        let original_layer_id = app.doc.selected_layer().expect("selected paint layer").id;
        let original_size = app.doc.size;
        let original_pixel = Rgba([21, 42, 63, 84]);
        app.doc
            .selected_layer_mut()
            .expect("selected paint layer")
            .pixels_mut(Channel::BaseColor, original_size)
            .put_pixel(4, 5, original_pixel);
        app.doc
            .selected_layer_mut()
            .expect("selected paint layer")
            .mask
            .put_pixel(6, 7, Luma([111]));

        let second_set_id = app.doc.add_texture_set("High Resolution").unwrap();
        app.doc.try_resize(128).unwrap();
        let second_size = app.doc.size;
        let second_layer_id = app.doc.selected_layer().expect("selected paint layer").id;
        let second_pixel = Rgba([210, 120, 30, 240]);
        app.doc
            .selected_layer_mut()
            .expect("selected paint layer")
            .pixels_mut(Channel::Metallic, second_size)
            .put_pixel(10, 11, second_pixel);
        app.doc
            .selected_layer_mut()
            .expect("selected paint layer")
            .mask
            .put_pixel(12, 13, Luma([222]));
        let second_set_name = app.doc.active_set_name.clone();
        app.texture_set_name_input = second_set_name.clone();
        assert_eq!(app.doc.size, 128);
        app.unsaved = false;
        app.paint_dirty = false;
        app.model_dirty = false;

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
                    egui::CentralPanel::default().show(ctx, |ui| app.texture_sets(ui));
                },
            )
        };

        run(Vec::new());
        let inactive_row_rect = ctx
            .data(|data| {
                data.get_temp::<egui::Rect>(egui::Id::new((
                    "texture-set-list-row-rect",
                    original_set_id,
                )))
            })
            .expect("inactive Texture Set row geometry should be available");
        let inactive_row_pos = inactive_row_rect.center();

        run(vec![
            egui::Event::PointerMoved(inactive_row_pos),
            egui::Event::PointerButton {
                pos: inactive_row_pos,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            },
        ]);
        run(vec![
            egui::Event::PointerMoved(inactive_row_pos),
            egui::Event::PointerButton {
                pos: inactive_row_pos,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            },
        ]);

        assert_eq!(app.doc.texture_set_count(), 2);
        assert_eq!(app.doc.active_set_id, original_set_id);
        assert_eq!(app.doc.active_set_name, original_set_name);
        assert_eq!(app.texture_set_name_input, original_set_name);
        assert_eq!(app.doc.size, 64);
        assert_eq!(app.doc.layer_tree.roots.len(), 1);
        assert_eq!(app.doc.layer_tree.roots[0].id, original_layer_id);
        assert_eq!(
            app.doc.layer_tree.roots[0].pixels[&Channel::BaseColor].get_pixel(4, 5),
            &original_pixel
        );
        assert_eq!(app.doc.layer_tree.roots[0].mask.get_pixel(6, 7).0, [111]);

        let inactive_second_set = app
            .doc
            .texture_sets
            .iter()
            .find(|set| set.id == second_set_id)
            .expect("the other Texture Set should remain present");
        assert_eq!(inactive_second_set.name, second_set_name);
        assert_eq!(inactive_second_set.size, 128);
        assert_eq!(inactive_second_set.layer_tree.roots.len(), 1);
        assert_eq!(inactive_second_set.layer_tree.roots[0].id, second_layer_id);
        assert_eq!(
            inactive_second_set.layer_tree.roots[0].pixels[&Channel::Metallic].get_pixel(10, 11),
            &second_pixel
        );
        assert_eq!(
            inactive_second_set.layer_tree.roots[0]
                .mask
                .get_pixel(12, 13)
                .0,
            [222]
        );
        assert!(app.unsaved);
        assert!(app.paint_dirty && app.model_dirty);
    }

    #[test]
    fn egui_texture_set_list_rename_updates_name_without_losing_layer_content() {
        use image::{Luma, Rgba};

        let mut app = test_app();
        let active_set_id = app.doc.active_set_id;
        let original_name = app.doc.active_set_name.clone();
        let layer_id = app.doc.selected_layer().expect("selected paint layer").id;
        let pixel = Rgba([15, 45, 75, 105]);
        app.doc
            .selected_layer_mut()
            .expect("selected paint layer")
            .pixels_mut(Channel::BaseColor, 64)
            .put_pixel(8, 9, pixel);
        app.doc
            .selected_layer_mut()
            .expect("selected paint layer")
            .mask
            .put_pixel(10, 11, Luma([135]));
        app.texture_set_name_input.clear();
        app.unsaved = false;
        app.paint_dirty = false;
        app.model_dirty = false;

        let ctx = egui::Context::default();
        let screen_rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(480.0, 640.0));
        fn run_frame(
            app: &mut KaliumApp,
            ctx: &egui::Context,
            screen_rect: egui::Rect,
            events: Vec<egui::Event>,
        ) -> egui::FullOutput {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(screen_rect),
                    events,
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| app.texture_sets(ui));
                },
            )
        }
        fn click(
            app: &mut KaliumApp,
            ctx: &egui::Context,
            screen_rect: egui::Rect,
            pos: egui::Pos2,
        ) {
            run_frame(
                app,
                ctx,
                screen_rect,
                vec![
                    egui::Event::PointerMoved(pos),
                    egui::Event::PointerButton {
                        pos,
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
            );
            run_frame(
                app,
                ctx,
                screen_rect,
                vec![
                    egui::Event::PointerMoved(pos),
                    egui::Event::PointerButton {
                        pos,
                        button: egui::PointerButton::Primary,
                        pressed: false,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
            );
        }

        run_frame(&mut app, &ctx, screen_rect, Vec::new());
        let name_input_id = egui::Id::new(("texture-set-list-name-input-rect", active_set_id));
        let rename_button_id =
            egui::Id::new(("texture-set-list-rename-button-rect", active_set_id));
        let name_input_rect = ctx
            .data(|data| data.get_temp::<egui::Rect>(name_input_id))
            .expect("active Texture Set name input geometry should be available");
        let rename_button_rect = ctx
            .data(|data| data.get_temp::<egui::Rect>(rename_button_id))
            .expect("active Texture Set Rename button geometry should be available");

        click(&mut app, &ctx, screen_rect, name_input_rect.center());
        run_frame(
            &mut app,
            &ctx,
            screen_rect,
            vec![egui::Event::Text("Renamed Texture Set".into())],
        );
        assert_eq!(app.texture_set_name_input, "Renamed Texture Set");
        click(&mut app, &ctx, screen_rect, rename_button_rect.center());

        assert_eq!(app.doc.active_set_id, active_set_id);
        assert_eq!(app.doc.active_set_name, "Renamed Texture Set");
        assert_eq!(app.texture_set_name_input, "Renamed Texture Set");
        assert_ne!(app.doc.active_set_name, original_name);
        assert!(app.unsaved);
        assert!(app.paint_dirty && app.model_dirty);
        assert_eq!(app.doc.layer_tree.roots.len(), 1);
        assert_eq!(app.doc.layer_tree.roots[0].id, layer_id);
        assert_eq!(
            app.doc.layer_tree.roots[0].pixels[&Channel::BaseColor].get_pixel(8, 9),
            &pixel
        );
        assert_eq!(app.doc.layer_tree.roots[0].mask.get_pixel(10, 11).0, [135]);

        app.unsaved = false;
        app.paint_dirty = false;
        app.model_dirty = false;
        app.texture_set_name_input.clear();
        click(&mut app, &ctx, screen_rect, name_input_rect.center());
        assert!(app.texture_set_name_input.is_empty());
        click(&mut app, &ctx, screen_rect, rename_button_rect.center());

        assert_eq!(app.doc.active_set_id, active_set_id);
        assert_eq!(app.doc.active_set_name, "Renamed Texture Set");
        assert!(app.texture_set_name_input.is_empty());
        assert!(!app.unsaved);
        assert!(!app.paint_dirty && !app.model_dirty);
        assert_eq!(app.doc.layer_tree.roots[0].id, layer_id);
        assert_eq!(
            app.doc.layer_tree.roots[0].pixels[&Channel::BaseColor].get_pixel(8, 9),
            &pixel
        );
        assert_eq!(app.doc.layer_tree.roots[0].mask.get_pixel(10, 11).0, [135]);
    }

    #[test]
    fn egui_material_slot_combo_binds_texture_set_without_changing_set_content() {
        use image::{Luma, Rgba};

        let mut app = test_app();
        let first_set_id = app.doc.active_set_id;
        let first_set_name = app.doc.active_set_name.clone();
        let first_layer_id = app.doc.selected_layer().expect("selected paint layer").id;
        let first_pixel = Rgba([18, 36, 54, 72]);
        app.doc
            .selected_layer_mut()
            .expect("selected paint layer")
            .pixels_mut(Channel::BaseColor, 64)
            .put_pixel(3, 4, first_pixel);
        app.doc
            .selected_layer_mut()
            .expect("selected paint layer")
            .mask
            .put_pixel(5, 6, Luma([108]));

        let mut mesh = crate::mesh::Mesh::demo_cube();
        mesh.materials = vec!["Body".into(), "Trim".into()];
        mesh.triangle_materials = (0..12).map(|triangle| triangle % 2).collect();
        app.doc.import_mesh(mesh).unwrap();
        assert_eq!(app.doc.mesh.material_count(), 2);
        assert_eq!(app.doc.mesh.material_name(0), "Body");
        assert_eq!(app.doc.mesh.material_name(1), "Trim");

        let second_set_id = app.doc.add_texture_set("Second Set").unwrap();
        let second_set_name = app.doc.active_set_name.clone();
        let second_layer_id = app.doc.selected_layer().expect("selected paint layer").id;
        let second_pixel = Rgba([90, 120, 150, 180]);
        app.doc
            .selected_layer_mut()
            .expect("selected paint layer")
            .pixels_mut(Channel::Metallic, 64)
            .put_pixel(7, 8, second_pixel);
        app.doc
            .selected_layer_mut()
            .expect("selected paint layer")
            .mask
            .put_pixel(9, 10, Luma([210]));
        app.texture_set_name_input = second_set_name.clone();
        assert_eq!(app.doc.material_bindings, vec![Some(first_set_id); 2]);
        app.unsaved = false;
        app.paint_dirty = false;
        app.model_dirty = false;

        let ctx = egui::Context::default();
        let screen_rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(480.0, 640.0));
        fn run_frame(
            app: &mut KaliumApp,
            ctx: &egui::Context,
            screen_rect: egui::Rect,
            events: Vec<egui::Event>,
        ) -> egui::FullOutput {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(screen_rect),
                    events,
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| app.texture_sets(ui));
                },
            )
        }
        fn click(
            app: &mut KaliumApp,
            ctx: &egui::Context,
            screen_rect: egui::Rect,
            pos: egui::Pos2,
        ) {
            run_frame(
                app,
                ctx,
                screen_rect,
                vec![
                    egui::Event::PointerMoved(pos),
                    egui::Event::PointerButton {
                        pos,
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
            );
            run_frame(
                app,
                ctx,
                screen_rect,
                vec![
                    egui::Event::PointerMoved(pos),
                    egui::Event::PointerButton {
                        pos,
                        button: egui::PointerButton::Primary,
                        pressed: false,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
            );
        }

        run_frame(&mut app, &ctx, screen_rect, Vec::new());
        let combo_rect = ctx
            .data(|data| {
                data.get_temp::<egui::Rect>(egui::Id::new((
                    "mesh-slot-binding-combo-rect",
                    0_usize,
                )))
            })
            .expect("first material slot ComboBox geometry should be available");
        click(&mut app, &ctx, screen_rect, combo_rect.center());
        run_frame(&mut app, &ctx, screen_rect, Vec::new());

        let second_set_option_rect = ctx
            .data(|data| {
                data.get_temp::<egui::Rect>(egui::Id::new((
                    "mesh-slot-set-option-rect",
                    0_usize,
                    second_set_id,
                )))
            })
            .expect("second Texture Set option geometry should be available");
        let option_pos = second_set_option_rect.center();
        run_frame(
            &mut app,
            &ctx,
            screen_rect,
            vec![egui::Event::PointerMoved(option_pos)],
        );
        run_frame(&mut app, &ctx, screen_rect, Vec::new());
        click(&mut app, &ctx, screen_rect, second_set_option_rect.center());
        assert_eq!(
            app.doc.material_bindings,
            vec![Some(second_set_id), Some(first_set_id)]
        );
        assert_eq!(app.doc.active_set_id, second_set_id);
        assert_eq!(app.doc.texture_set_count(), 2);
        assert_eq!(app.doc.layer_tree.roots.len(), 1);
        assert_eq!(app.doc.layer_tree.roots[0].id, second_layer_id);
        assert_eq!(
            app.doc.layer_tree.roots[0].pixels[&Channel::Metallic].get_pixel(7, 8),
            &second_pixel
        );
        assert_eq!(app.doc.layer_tree.roots[0].mask.get_pixel(9, 10).0, [210]);

        let first_set = app
            .doc
            .texture_sets
            .iter()
            .find(|set| set.id == first_set_id)
            .expect("first Texture Set should remain present");
        assert_eq!(first_set.name, first_set_name);
        assert_eq!(first_set.size, 64);
        assert_eq!(first_set.layer_tree.roots.len(), 1);
        assert_eq!(first_set.layer_tree.roots[0].id, first_layer_id);
        assert_eq!(
            first_set.layer_tree.roots[0].pixels[&Channel::BaseColor].get_pixel(3, 4),
            &first_pixel
        );
        assert_eq!(first_set.layer_tree.roots[0].mask.get_pixel(5, 6).0, [108]);
        assert_eq!(app.doc.active_set_name, second_set_name);
        assert!(app.unsaved);
        assert!(app.paint_dirty && app.model_dirty);
    }

    #[test]
    fn painted_tile_updates_cached_material_without_full_invalidation() {
        let mut app = test_app();
        let ctx = egui::Context::default();
        app.refresh_textures(&ctx);
        let set_id = app.doc.active_set_id;
        let size = app.doc.size;
        let hit = (0..app.view.height)
            .flat_map(|y| (0..app.view.width).map(move |x| (x, y)))
            .find_map(|(x, y)| app.view.pick_surface(x, y))
            .expect("reference mesh covers viewport pixels");
        let uv = glam::Vec2::from_array(hit.uv).clamp(glam::Vec2::ZERO, glam::Vec2::ONE);
        let x = (uv.x * (size - 1) as f32).round() as u32;
        let y = (uv.y * (size - 1) as f32).round() as u32;
        app.doc
            .selected_layer_mut()
            .expect("selected paint layer")
            .pixels_mut(Channel::BaseColor, size)
            .put_pixel(x, y, image::Rgba([210, 30, 70, 255]));
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
        assert!(!app.model_dirty);
        assert!(!app.model_partial_dirty.is_empty());

        let mut full_render = crate::renderer::Viewport::new(app.view.width, app.view.height);
        full_render.render_bound(
            &app.doc.mesh,
            app.material.as_ref().unwrap(),
            &app.doc.material_bindings,
            None,
        );
        assert_eq!(app.view.rgba, full_render.rgba);

        app.refresh_textures(&ctx);
        assert!(app.model_partial_dirty.is_empty());
        assert!(app.unsaved);
    }

    #[test]
    fn opacity_paint_invalidates_and_rebuilds_the_full_reference_view() {
        let mut app = test_app();
        let ctx = egui::Context::default();
        app.refresh_textures(&ctx);
        let hit_position = (0..app.view.height)
            .flat_map(|y| (0..app.view.width).map(move |x| (x, y)))
            .find_map(|(x, y)| app.view.pick_surface(x, y).map(|hit| ((x, y), hit)))
            .expect("reference mesh covers viewport pixels");
        let (screen_position, hit) = hit_position;
        let original_frame = app.view.rgba.clone();
        let uv = glam::Vec2::from_array(hit.uv).clamp(glam::Vec2::ZERO, glam::Vec2::ONE);
        let size = app.doc.size;
        let texel_x = (uv.x * (size - 1) as f32).round() as u32;
        let texel_y = (uv.y * (size - 1) as f32).round() as u32;
        app.doc
            .selected_layer_mut()
            .expect("selected paint layer")
            .pixels_mut(Channel::Opacity, size)
            .put_pixel(texel_x, texel_y, image::Rgba([0, 0, 0, 255]));
        let mut touched = HashSet::new();
        touched.insert(0);

        app.update_painted_tiles(Channel::Opacity, &touched);

        assert!(app.paint_dirty);
        assert!(app.model_dirty);
        app.refresh_textures(&ctx);
        assert_ne!(app.view.rgba, original_frame);
        assert_eq!(
            app.view.pick_surface(screen_position.0, screen_position.1),
            Some(hit)
        );
        let mut full = crate::renderer::Viewport::new(app.view.width, app.view.height);
        full.render_bound(
            &app.doc.mesh,
            app.material.as_ref().unwrap(),
            &app.doc.material_bindings,
            None,
        );
        assert_eq!(app.view.rgba, full.rgba);
    }

    #[test]
    fn uniform_channel_edits_patch_cached_material_without_rebuilding_all_maps() {
        let mut app = test_app();
        let ctx = egui::Context::default();
        app.refresh_textures(&ctx);
        app.channel = Channel::BaseColor;
        app.brush.color = [42, 137, 211, 255];
        let set_id = app.doc.active_set_id;
        let roughness_before = app
            .material
            .as_ref()
            .unwrap()
            .active()
            .image(Channel::Roughness)
            .clone();

        app.perform(Action::Fill);

        assert!(!app.paint_dirty);
        assert!(app.model_dirty);
        assert_eq!(
            app.material
                .as_ref()
                .unwrap()
                .active()
                .image(Channel::BaseColor),
            app.doc
                .composite_for_set(set_id, Channel::BaseColor)
                .as_ref()
                .unwrap()
        );
        assert_eq!(
            app.material
                .as_ref()
                .unwrap()
                .active()
                .image(Channel::Roughness),
            &roughness_before
        );

        app.refresh_textures(&ctx);
        let mut full_render = crate::renderer::Viewport::new(app.view.width, app.view.height);
        full_render.render_bound(
            &app.doc.mesh,
            app.material.as_ref().unwrap(),
            &app.doc.material_bindings,
            None,
        );
        assert_eq!(app.view.rgba, full_render.rgba);

        app.perform(Action::ClearChannel);
        assert!(!app.paint_dirty);
        assert_eq!(
            app.material
                .as_ref()
                .unwrap()
                .active()
                .image(Channel::BaseColor),
            app.doc
                .composite_for_set(set_id, Channel::BaseColor)
                .as_ref()
                .unwrap()
        );
        app.refresh_textures(&ctx);
        full_render.render_bound(
            &app.doc.mesh,
            app.material.as_ref().unwrap(),
            &app.doc.material_bindings,
            None,
        );
        assert_eq!(app.view.rgba, full_render.rgba);
    }

    #[test]
    fn ao_source_tile_refreshes_dependent_roughness_cache_region() {
        let mut app = test_app();
        app.doc.add_layer().unwrap();
        let source_layer_id = app.doc.selected_layer().expect("selected paint layer").id;
        let size = app.doc.size;
        app.doc
            .selected_layer_mut()
            .expect("selected paint layer")
            .pixels
            .insert(
                Channel::Occlusion,
                image::RgbaImage::from_pixel(size, size, image::Rgba([255, 255, 255, 255])),
            );
        app.doc.layer_tree.roots[0].procedural_graph = Some(
            crate::procedural::ProceduralGraph::value_noise_times_occlusion(
                67,
                5.0,
                source_layer_id,
            ),
        );
        app.material = Some(crate::renderer::MaterialLibrary::from_document(&app.doc));
        app.paint_dirty = false;
        app.model_dirty = true;
        app.doc
            .selected_layer_mut()
            .expect("selected paint layer")
            .pixels
            .get_mut(&Channel::Occlusion)
            .unwrap()
            .put_pixel(9, 11, image::Rgba([0, 0, 0, 255]));

        let mut touched = HashSet::new();
        touched.insert(0);
        app.update_painted_tiles(Channel::Occlusion, &touched);

        let cached = app
            .material
            .as_ref()
            .unwrap()
            .active()
            .image(Channel::Roughness);
        let full = app.doc.composite(Channel::Roughness);
        assert_eq!(cached, &full);
        assert_eq!(cached.get_pixel(9, 11).0[0], 0);
        assert!(!app.paint_dirty);
    }

    #[test]
    fn generation_compaction_restores_snapshot_with_empty_journal() {
        let temp = TempProject::new();
        let baseline = Document::new(64);
        let project_id = baseline.project_id;
        let layer_id = baseline.selected_layer().unwrap().id;
        project::save(&baseline, &temp.0).unwrap();
        let primary_bytes = std::fs::read(&temp.0).unwrap();

        let mut app = test_app();
        app.install_open_project(project::load(&temp.0).unwrap(), temp.0.clone(), false);
        app.doc.selected_layer_mut().unwrap().name = "Checkpointed edit".into();
        app.append_recovery_batch_inner(
            &[DocumentEvent::LayerRename {
                set_id: app.doc.active_set_id,
                layer_id,
                name: "Checkpointed edit".into(),
            }],
            false,
        );

        assert_eq!(std::fs::read(&temp.0).unwrap(), primary_bytes);
        let generation_store =
            RecoveryGenerationStore::from_journal_path(&recovery_journal_path(&temp.0));
        let journal_path = app.recovery_journal_path.as_ref().unwrap();
        let journal_len = std::fs::metadata(journal_path).unwrap().len();
        app.compact_recovery_journal_if_needed_at(journal_len + 1);
        assert!(generation_store
            .read_active(project_id, 1)
            .unwrap()
            .is_none());
        assert!(app.recovery_journal.is_some());
        app.compact_recovery_journal_if_needed_at(journal_len);
        assert!(app.recovery_compaction_job.is_some());
        app.finish_recovery_compaction();
        let generation = generation_store
            .read_active(project_id, 1)
            .unwrap()
            .unwrap();
        assert_eq!(generation.sequence_high_water, 1);
        assert!(crate::recovery::scan(
            &generation.journal_path,
            project_id,
            generation.baseline_digest,
            1
        )
        .unwrap()
        .is_empty());
        app.stop_recovery_writer().unwrap();
        drop(app);

        let mut reopened = test_app();
        reopened.install_open_project(project::load(&temp.0).unwrap(), temp.0.clone(), false);
        assert!(
            reopened.pending_journal_recovery.is_some(),
            "{}; recovery unavailable: {:?}",
            reopened.status,
            reopened.recovery_unavailable
        );
        reopened.resolve_journal_recovery(JournalRecoveryChoice::Restore);
        assert_eq!(
            reopened.doc.selected_layer().unwrap().name,
            "Checkpointed edit"
        );
        assert!(reopened.recovery_journal.is_some());
    }

    #[test]
    fn recovery_compaction_worker_keeps_ui_pumping_without_cloning_document() {
        let temp = TempProject::new();
        let baseline = Document::new(64);
        let project_id = baseline.project_id;
        let layer_id = baseline.selected_layer().unwrap().id;
        project::save(&baseline, &temp.0).unwrap();

        let mut app = test_app();
        app.install_open_project(project::load(&temp.0).unwrap(), temp.0.clone(), false);
        app.doc.selected_layer_mut().unwrap().name = "Worker checkpoint edit".into();
        app.append_recovery_batch_inner(
            &[DocumentEvent::LayerRename {
                set_id: app.doc.active_set_id,
                layer_id,
                name: "Worker checkpoint edit".into(),
            }],
            false,
        );

        let (start_gate, wait_for_release) = std::sync::mpsc::channel();
        app.start_recovery_compaction_paused(wait_for_release)
            .unwrap();
        assert!(app.recovery_compaction_job.is_some());
        assert!(app.recovery_journal.is_none());
        assert_ne!(app.doc.project_id, project_id);

        let context = egui::Context::default();
        let output = context.run(egui::RawInput::default(), |ctx| {
            app.show_recovery_compaction_progress(ctx);
        });
        let mut rendered = String::new();
        for clipped in &output.shapes {
            collect_shape_text(&clipped.shape, &mut rendered);
        }
        assert!(rendered.contains("Writing recovery checkpoint"));
        assert!(app
            .recovery_compaction_job
            .as_ref()
            .is_some_and(|job| !job.is_finished()));

        let close_output = context.run(
            egui::RawInput {
                viewports: std::iter::once((
                    egui::ViewportId::ROOT,
                    egui::ViewportInfo {
                        events: vec![egui::ViewportEvent::Close],
                        ..Default::default()
                    },
                ))
                .collect(),
                ..Default::default()
            },
            |ctx| app.show_recovery_compaction_progress(ctx),
        );
        assert!(close_output
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .is_some_and(|viewport| viewport
                .commands
                .contains(&egui::ViewportCommand::CancelClose)));
        assert!(app
            .recovery_compaction_job
            .as_ref()
            .is_some_and(|job| !job.is_finished()));

        start_gate.send(()).unwrap();
        app.finish_recovery_compaction();
        assert_eq!(app.doc.project_id, project_id);
        assert_eq!(
            app.doc.selected_layer().unwrap().name,
            "Worker checkpoint edit"
        );
        assert!(app.recovery_journal.is_some());
    }

    #[test]
    fn failed_recovery_compaction_reopens_old_writer_and_keeps_edits_recoverable() {
        let temp = TempProject::new();
        let baseline = Document::new(64);
        let project_id = baseline.project_id;
        let layer_id = baseline.selected_layer().unwrap().id;
        project::save(&baseline, &temp.0).unwrap();

        let mut app = test_app();
        app.install_open_project(project::load(&temp.0).unwrap(), temp.0.clone(), false);
        app.doc.selected_layer_mut().unwrap().name = "Before compaction failure".into();
        app.append_recovery_batch_inner(
            &[DocumentEvent::LayerRename {
                set_id: app.doc.active_set_id,
                layer_id,
                name: "Before compaction failure".into(),
            }],
            false,
        );

        app.start_recovery_compaction_failing_before_publish()
            .unwrap();
        app.finish_recovery_compaction();
        assert_eq!(app.doc.project_id, project_id);
        assert_eq!(
            app.doc.selected_layer().unwrap().name,
            "Before compaction failure"
        );
        assert!(app.recovery_journal.is_some());
        assert!(app.recovery_unavailable.is_none());
        assert!(app.status.contains("could not be compacted"));

        app.doc.selected_layer_mut().unwrap().name = "After compaction failure".into();
        app.append_recovery_batch_inner(
            &[DocumentEvent::LayerRename {
                set_id: app.doc.active_set_id,
                layer_id,
                name: "After compaction failure".into(),
            }],
            false,
        );
        app.stop_recovery_writer().unwrap();
        drop(app);

        let mut reopened = test_app();
        reopened.install_open_project(project::load(&temp.0).unwrap(), temp.0.clone(), false);
        assert!(reopened.pending_journal_recovery.is_some());
        reopened.resolve_journal_recovery(JournalRecoveryChoice::Restore);
        assert_eq!(
            reopened.doc.selected_layer().unwrap().name,
            "After compaction failure"
        );
    }

    #[test]
    fn dropping_app_joins_recovery_compaction_before_releasing_project_state() {
        let temp = TempProject::new();
        let baseline = Document::new(64);
        let project_id = baseline.project_id;
        let layer_id = baseline.selected_layer().unwrap().id;
        project::save(&baseline, &temp.0).unwrap();

        let mut app = test_app();
        app.install_open_project(project::load(&temp.0).unwrap(), temp.0.clone(), false);
        app.doc.selected_layer_mut().unwrap().name = "Drop worker edit".into();
        app.append_recovery_batch_inner(
            &[DocumentEvent::LayerRename {
                set_id: app.doc.active_set_id,
                layer_id,
                name: "Drop worker edit".into(),
            }],
            false,
        );
        let (release_sender, wait_for_release) = std::sync::mpsc::channel();
        app.start_recovery_compaction_paused(wait_for_release)
            .unwrap();
        let project_path = temp.0.clone();
        let release_thread = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            assert!(super::open_project_recovery_lock(&project_path).is_err());
            release_sender.send(()).unwrap();
        });

        drop(app);
        release_thread.join().unwrap();
        let released_lock = super::open_project_recovery_lock(&temp.0).unwrap();
        drop(released_lock);
        let store = RecoveryGenerationStore::from_journal_path(&recovery_journal_path(&temp.0));
        let generation = store
            .read_active(project_id, 1)
            .unwrap()
            .expect("Drop waits until the worker publishes its checkpoint");
        let checkpoint = project::load_checkpoint(&generation.baseline_path).unwrap();
        assert_eq!(
            checkpoint.selected_layer().unwrap().name,
            "Drop worker edit"
        );
    }

    #[test]
    fn discarding_generation_recovery_publishes_primary_baseline_without_stale_fallback() {
        let temp = TempProject::new();
        let baseline = Document::new(64);
        let project_id = baseline.project_id;
        let layer_id = baseline.selected_layer().unwrap().id;
        project::save(&baseline, &temp.0).unwrap();

        let mut app = test_app();
        app.install_open_project(project::load(&temp.0).unwrap(), temp.0.clone(), false);
        app.doc.selected_layer_mut().unwrap().name = "Discard this edit".into();
        app.append_recovery_batch_inner(
            &[DocumentEvent::LayerRename {
                set_id: app.doc.active_set_id,
                layer_id,
                name: "Discard this edit".into(),
            }],
            false,
        );
        app.compact_recovery_journal().unwrap();
        app.stop_recovery_writer().unwrap();
        drop(app);

        let mut reopened = test_app();
        reopened.install_open_project(project::load(&temp.0).unwrap(), temp.0.clone(), false);
        assert!(reopened.pending_journal_recovery.is_some());
        reopened.resolve_journal_recovery(JournalRecoveryChoice::Discard);
        assert_eq!(reopened.doc.selected_layer().unwrap().name, "Paint Layer");
        let store = RecoveryGenerationStore::from_journal_path(&recovery_journal_path(&temp.0));
        let clean = store.read_active(project_id, 1).unwrap().unwrap();
        assert_eq!(
            clean.baseline_digest,
            project::file_digest(&temp.0).unwrap()
        );
        assert!(store
            .read_active(project_id, 1)
            .unwrap()
            .is_some_and(|generation| !generation.used_fallback));
        assert!(
            crate::recovery::scan(&clean.journal_path, project_id, clean.baseline_digest, 1)
                .unwrap()
                .is_empty()
        );
        reopened.stop_recovery_writer().unwrap();
        drop(reopened);

        let mut next_open = test_app();
        next_open.install_open_project(project::load(&temp.0).unwrap(), temp.0.clone(), false);
        assert!(next_open.pending_journal_recovery.is_none());
        assert!(next_open.recovery_journal.is_some());
    }
}
