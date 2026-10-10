use crate::{
    document::{BlendMode, Channel, Document},
    export,history::{History,Snapshot},mesh::Mesh,paint::{Brush,BrushMode},
    project,renderer::{Material,Viewport},spp,ops,brush_preset,uv::UvCoverage,
    layout::{self,Pane},
};
use eframe::egui::{self,Color32,ColorImage,TextureHandle,TextureOptions,Vec2};
use egui_dock::{DockArea,DockState,Style,TabViewer};
use std::time::{Duration,Instant};
use std::path::PathBuf;

#[derive(Clone,Copy,PartialEq,Eq)]
enum Canvas {Model,UV}
#[derive(Clone,Copy)]
enum Action {New,Open,Save,SaveAs,ImportMesh,ImportTexture,Export,InspectSpp,LoadBrush,SaveBrush,Fill,ClearChannel,InvertMask,ResetMask}
#[derive(Clone,Copy)]
enum LayerCommand {Add,Duplicate,Remove,Up,Down}
struct PendingStroke {layer_id:u32,channel:Channel,mask:bool,before:Vec<u8>}

pub struct KaliumApp {
    doc:Document,
    brush:Brush,
    channel:Channel,
    solo_channel:bool,
    show_uv_wire:bool,
    view:Viewport,
    uv_coverage:Option<UvCoverage>,
    uv_clipping:bool,
    material:Option<Material>,
    model_texture:Option<TextureHandle>,
    uv_texture:Option<TextureHandle>,
    paint_dirty:bool,
    model_dirty:bool,
    history:History,
    previous_uv:Option<[f32;2]>,
    pending_stroke:Option<PendingStroke>,
    file_path:Option<PathBuf>,
    unsaved:bool,
    status:String,
    dock:DockState<Pane>,
    last_layout_save:Instant,
    saved_layout:Vec<u8>,
    asset_search:String,
    show_tool_shelf:bool,
}

impl KaliumApp {
    pub fn new(cc:&eframe::CreationContext<'_>)->Self {
        let mut visuals=egui::Visuals::dark();
        visuals.selection.bg_fill=Color32::from_rgb(44,105,156);
        visuals.widgets.active.bg_fill=Color32::from_rgb(50,98,144);
        visuals.widgets.hovered.bg_fill=Color32::from_rgb(55,63,75);
        visuals.panel_fill=Color32::from_rgb(33,34,38);
        visuals.window_fill=Color32::from_rgb(34,35,40);
        cc.egui_ctx.set_visuals(visuals);
        let doc=Document::new(512);
        let uv_coverage=UvCoverage::generate(&doc.mesh,doc.size).ok();
        Self {
            doc,brush:Brush::default(),channel:Channel::BaseColor,
            solo_channel:false,show_uv_wire:true,
            view:Viewport::new(600,600),uv_coverage,uv_clipping:true,material:None,model_texture:None,
            uv_texture:None,paint_dirty:true,model_dirty:true,
            history:History::new(),previous_uv:None,pending_stroke:None,
            file_path:None,unsaved:false,
            status:"Ready • Left drag to paint • Right drag to orbit • Scroll to zoom".into(),
            dock:layout::load_workspace(),last_layout_save:Instant::now(),
            saved_layout:Vec::new(),asset_search:String::new(),show_tool_shelf:true,
        }
    }

    fn rebuild_coverage(&mut self) {
        match UvCoverage::generate(&self.doc.mesh,self.doc.size) {
            Ok(map)=>self.uv_coverage=Some(map),
            Err(err)=>{
                self.uv_coverage=None;
                self.uv_clipping=false;
                self.status=format!("UV clipping disabled: {err}");
            }
        }
    }
    fn change(&mut self) {
        self.paint_dirty=true;
        self.model_dirty=true;
        self.unsaved=true;
    }
    fn refresh_textures(&mut self,ctx:&egui::Context) {
        if self.paint_dirty || self.material.is_none() {
            self.material=Some(Material::from_document(&self.doc));
            self.paint_dirty=false;
            self.model_dirty=true;
            self.uv_texture=None;
        }
        if self.uv_texture.is_none() {
            let image=self.material.as_ref().unwrap().image(self.channel);
            let rgba=ColorImage::from_rgba_unmultiplied(
                [image.width() as usize,image.height() as usize],image.as_raw());
            self.uv_texture=Some(ctx.load_texture("kalium-uv",rgba,TextureOptions::LINEAR));
        }
        if self.model_dirty {
            self.view.render(&self.doc.mesh,self.material.as_ref().unwrap(),
                if self.solo_channel {Some(self.channel)} else {None});
            let rgba=ColorImage::from_rgba_unmultiplied(
                [self.view.width,self.view.height],&self.view.rgba);
            if let Some(tex)=&mut self.model_texture {tex.set(rgba,TextureOptions::LINEAR);}
            else {self.model_texture=Some(ctx.load_texture("kalium-model",rgba,TextureOptions::LINEAR));}
            self.model_dirty=false;
        }
    }
    fn perform(&mut self,action:Action) {
        let result: anyhow::Result<Option<String>>=(||{
            match action {
                Action::New=>{
                    self.finish_stroke();
                    self.doc=Document::new(512);
                    self.history.clear();
                    self.file_path=None;
                    self.unsaved=false;self.view=Viewport::new(600,600);self.rebuild_coverage();self.change();
                    self.unsaved=false;
                    Ok(Some("New Kalium project created".into()))
                }
                Action::Open=>{
                    let Some(path)=rfd::FileDialog::new()
                        .add_filter("Kalium project", &["kalium"]).pick_file() else {return Ok(None)};
                    let doc=project::load(&path)?;
                    self.finish_stroke();
                    self.doc=doc;
                    self.rebuild_coverage();
                    self.history.clear();self.file_path=Some(path.clone());
                    self.change();self.unsaved=false;
                    Ok(Some(format!("Opened {}",path.display())))
                }
                Action::Save|Action::SaveAs=>{
                    self.finish_stroke();
                    let path=if matches!(action,Action::Save) {self.file_path.clone()} else {None};
                    let path=match path {
                        Some(p)=>p,
                        None=>{
                            let Some(p)=rfd::FileDialog::new().add_filter("Kalium project", &["kalium"])
                                .set_file_name("Untitled.kalium").save_file() else {return Ok(None)};
                            p
                        }
                    };
                    project::save(&self.doc,&path)?;
                    self.file_path=Some(path.clone());self.unsaved=false;
                    Ok(Some(format!("Saved {}",path.display())))
                }
                Action::ImportMesh=>{
                    let Some(path)=rfd::FileDialog::new()
                        .add_filter("3D mesh", &["obj","gltf","glb"]).pick_file() else {return Ok(None)};
                    let mesh=Mesh::from_file(&path)?;
                    self.finish_stroke();
                    self.doc.mesh=mesh;
                    self.rebuild_coverage();
                    self.history.clear();self.change();
                    Ok(Some(format!("Imported {}",path.display())))
                }
                Action::ImportTexture=>{
                    let Some(path)=rfd::FileDialog::new()
                        .add_filter("Image texture", &["png","jpg","jpeg"]).pick_file() else {return Ok(None)};
                    self.finish_stroke();
                    ops::import_texture(&mut self.doc,self.channel,&path)?;
                    self.history.clear();
                    self.change();
                    Ok(Some(format!("Imported texture {} as new {} layer",path.display(),self.channel.label())))
                }
                Action::LoadBrush=>{
                    let Some(path)=rfd::FileDialog::new()
                        .add_filter("Kalium brush preset", &["kbrush"]).pick_file() else {return Ok(None)};
                    self.finish_stroke();
                    let (name,brush)=brush_preset::load(&path)?;
                    self.brush=brush;
                    Ok(Some(format!("Loaded brush preset {name}")))
                }
                Action::SaveBrush=>{
                    let Some(path)=rfd::FileDialog::new()
                        .add_filter("Kalium brush preset", &["kbrush"])
                        .set_file_name("MyBrush.kbrush").save_file() else {return Ok(None)};
                    let name=path.file_stem().and_then(|s|s.to_str()).unwrap_or("MyBrush");
                    brush_preset::save(&self.brush,name,&path)?;
                    Ok(Some(format!("Saved brush preset {}",path.display())))
                }
                Action::Fill=>{
                    self.finish_stroke();
                    let mode=self.brush.mode;
                    let mask=mode.is_mask();
                    let before=History::capture(&mut self.doc,self.channel,mode);
                    let layer_id=self.doc.selected_layer().id;
                    let changed=ops::fill_selected(&mut self.doc,self.channel,&self.brush);
                    if changed {
                        let after=History::capture(&mut self.doc,self.channel,mode);
                        self.history.commit(Snapshot {layer_id,channel:self.channel,mask,before,after});
                        self.change();
                    }
                    Ok(Some(if changed {"Filled active layer channel or mask".into()}
                        else {"Nothing to fill (already identical)".into()}))
                }
                Action::ClearChannel=>{
                    self.finish_stroke();
                    let mode=BrushMode::Paint;
                    let before=History::capture(&mut self.doc,self.channel,mode);
                    let layer_id=self.doc.selected_layer().id;
                    let mut brush=self.brush.clone();
                    brush.mode=BrushMode::Erase;
                    if ops::fill_selected(&mut self.doc,self.channel,&brush) {
                        let after=History::capture(&mut self.doc,self.channel,mode);
                        self.history.commit(Snapshot {layer_id,channel:self.channel,mask:false,before,after});
                        self.change();
                    }
                    Ok(Some(format!("Cleared {} on selected layer",self.channel.label())))
                }
                Action::InvertMask|Action::ResetMask=>{
                    self.finish_stroke();
                    let mode=BrushMode::MaskWhite;
                    let before=History::capture(&mut self.doc,self.channel,mode);
                    let layer_id=self.doc.selected_layer().id;
                    let changed=if matches!(action,Action::InvertMask) {
                        self.doc.invert_selected_mask()
                    } else {
                        let mut brush=self.brush.clone();
                        brush.mode=BrushMode::MaskWhite;
                        ops::fill_selected(&mut self.doc,self.channel,&brush)
                    };
                    if changed {
                        let after=History::capture(&mut self.doc,self.channel,mode);
                        self.history.commit(Snapshot {layer_id,channel:self.channel,mask:true,before,after});
                        self.change();
                    }
                    Ok(Some(if matches!(action,Action::InvertMask) {"Inverted selected layer mask".into()} else {"Reset layer mask to white".into()}))
                }
                Action::Export=>{
                    let Some(dir)=rfd::FileDialog::new().pick_folder() else {return Ok(None)};
                    let n=export::export_textures(&self.doc,&dir)?;
                    Ok(Some(format!("Exported {n} PNG texture maps into {}",dir.display())))
                }
                Action::InspectSpp=>{
                    let Some(path)=rfd::FileDialog::new()
                        .add_filter("Substance Painter project", &["spp"]).pick_file() else {return Ok(None)};
                    let report=spp::inspect(&path)?;
                    Ok(Some(format!("SPP inspection: {report}")))
                }
            }
        })();
        match result {
            Ok(Some(s))=>self.status=s,
            Ok(None)=>{},
            Err(e)=>self.status=format!("Error: {e:#}"),
        }
    }
    fn menu(&mut self,ctx:&egui::Context) {
        let mut action=None;
        egui::TopBottomPanel::top("menu").show(ctx,|ui|{
            egui::menu::bar(ui,|ui|{
                ui.heading(egui::RichText::new("◆ Kalium").color(Color32::from_rgb(156,201,248)));
                ui.separator();
                ui.menu_button("File",|ui|{
                    for (label,a) in [
                        ("New",Action::New),("Open .kalium…",Action::Open),
                        ("Save",Action::Save),("Save As…",Action::SaveAs),
                        ("Import OBJ / glTF / GLB…",Action::ImportMesh),
                        ("Import PNG / JPEG as layer…",Action::ImportTexture),
                        ("Load brush preset (.kbrush)…",Action::LoadBrush),
                        ("Save brush preset (.kbrush)…",Action::SaveBrush),
                        ("Export PBR textures…",Action::Export),
                        ("Clear active channel",Action::ClearChannel),
                        ("Inspect SPP signature (experimental)…",Action::InspectSpp),
                    ] {
                        if ui.button(label).clicked(){action=Some(a);ui.close_menu();}
                    }
                });
                ui.menu_button("View",|ui| {
                    if ui.button("Restore default workspace").clicked() {
                        self.dock=layout::default_workspace();
                        self.status="Default workspace restored".into();
                        ui.close_menu();
                    }
                    if ui.button("Save workspace now").clicked() {
                        match layout::persist_workspace(&self.dock) {
                            Ok(_)=>self.status="Workspace saved".into(),
                            Err(err)=>self.status=format!("Cannot save workspace: {err}"),
                        }
                        ui.close_menu();
                    }
                    ui.separator();
                    ui.checkbox(&mut self.show_tool_shelf,"Show tool shelf");
                    ui.label("Show / hide panels");
                    for pane in Pane::ALL {
                        let mut visible=self.dock.find_tab(&pane).is_some();
                        if ui.checkbox(&mut visible,pane.label()).changed() {
                            if visible {self.dock.push_to_first_leaf(pane);}
                            else {self.dock.retain_tabs(|tab|*tab!=pane);}
                        }
                    }
                });
                ui.separator();
                if ui.button("Undo").clicked(){if self.history.undo(&mut self.doc){self.change();}}
                if ui.button("Redo").clicked(){if self.history.redo(&mut self.doc){self.change();}}
                ui.menu_button("Layer",|ui|{
                    if ui.button("Add paint layer").clicked() {self.layer_command(LayerCommand::Add);ui.close_menu();}
                    if ui.button("Duplicate selected layer").clicked() {self.layer_command(LayerCommand::Duplicate);ui.close_menu();}
                    if ui.button("Remove selected layer").clicked() {self.layer_command(LayerCommand::Remove);ui.close_menu();}
                    ui.separator();
                    if ui.button("Invert mask").clicked() {action=Some(Action::InvertMask);ui.close_menu();}
                    if ui.button("Reset mask").clicked() {action=Some(Action::ResetMask);ui.close_menu();}
                    if ui.button("Clear active channel").clicked() {action=Some(Action::ClearChannel);ui.close_menu();}
                });
                ui.separator();
                ui.label(egui::RichText::new("AGPLv3 • Offline").small().weak());
            });
            ui.separator();
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("TOOLS").small().weak());
                for (label,mode) in [("Brush",BrushMode::Paint),("Eraser",BrushMode::Erase),
                    ("Sample",BrushMode::Sample),("Fill",BrushMode::Bucket),
                    ("Mask +",BrushMode::MaskWhite),("Mask −",BrushMode::MaskBlack)] {
                    if ui.selectable_label(self.brush.mode==mode,label).clicked() {self.finish_stroke();self.brush.mode=mode;}
                }
                ui.separator();
                ui.label(format!("{}  |  {} px",self.channel.label(),self.brush.radius));
            });
        });
        if let Some(a)=action {self.perform(a);}
        let keys=ctx.input(|i|{
            let cmd=i.modifiers.command;
            if cmd && i.key_pressed(egui::Key::S) {
                Some(if i.modifiers.shift {Action::SaveAs} else {Action::Save})
            } else if cmd && i.key_pressed(egui::Key::O) {Some(Action::Open)}
            else {None}
        });
        if let Some(action)=keys {self.perform(action);}
        let (undo,redo)=ctx.input(|i|{
            (i.modifiers.command&&i.key_pressed(egui::Key::Z)&&!i.modifiers.shift,
             i.modifiers.command&&(i.key_pressed(egui::Key::Y)||
                (i.modifiers.shift&&i.key_pressed(egui::Key::Z))))
        });
        if undo {self.finish_stroke();if self.history.undo(&mut self.doc) {self.change();}}
        if redo {self.finish_stroke();if self.history.redo(&mut self.doc) {self.change();}}
        // Painting shortcuts do not steal keystrokes while editing text in panel controls.
        if !ctx.wants_keyboard_input() {
            let shortcut=ctx.input(|i|{
                if i.key_pressed(egui::Key::B) {Some(BrushMode::Paint)}
                else if i.key_pressed(egui::Key::E) {Some(BrushMode::Erase)}
                else if i.key_pressed(egui::Key::I) {Some(BrushMode::Sample)}
                else if i.key_pressed(egui::Key::G) {Some(BrushMode::Bucket)}
                else {None}
            });
            if let Some(mode)=shortcut {self.finish_stroke();self.brush.mode=mode;}
        }
    }
    fn tool_shelf(&mut self,ctx:&egui::Context) {
        if !self.show_tool_shelf {return;}
        egui::SidePanel::left("kalium-tool-shelf")
            .exact_width(48.0)
            .frame(egui::Frame::NONE.inner_margin(egui::Margin::symmetric(3,5)))
            .show(ctx,|ui| {
                ui.vertical_centered(|ui| {
                    for (icon,mode,help) in [
                        ("✎",BrushMode::Paint,"Paint (B)"),
                        ("⌫",BrushMode::Erase,"Eraser (E)"),
                        ("◎",BrushMode::Sample,"Sample (I)"),
                        ("▧",BrushMode::Bucket,"Connected fill (G)"),
                        ("◐",BrushMode::MaskWhite,"Paint mask white"),
                        ("◑",BrushMode::MaskBlack,"Paint mask black"),
                    ] {
                        if ui.add_sized([37.0,34.0],egui::Button::new(icon)
                            .selected(self.brush.mode==mode)).on_hover_text(help).clicked() {
                            self.finish_stroke();
                            self.brush.mode=mode;
                        }
                    }
                });
            });
    }
    fn tools(&mut self,ui:&mut egui::Ui) {
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
                        self.finish_stroke();
                        self.doc.resize(size);self.rebuild_coverage();self.history.clear();self.change();
                    }
                }
            });
            ui.add_space(8.0);
            ui.label(egui::RichText::new("TIP").small().weak());
            ui.label("Left-drag paints on the 3D surface or UV image. Flood Fill uses one click. Right-drag rotates the model.");
            ui.label("Mouse wheel zooms. Ctrl+S saves. Ctrl+Z / Ctrl+Y undo or redo strokes.");
        });
    }
    fn layer_command(&mut self,command:LayerCommand) {
        self.finish_stroke();
        let changed=match command {
            LayerCommand::Add=>match self.doc.add_layer() {
                Ok(())=>true,Err(err)=>{self.status=err.to_string();false}
            },
            LayerCommand::Duplicate=>match self.doc.duplicate_selected() {
                Ok(())=>true,Err(err)=>{self.status=err.to_string();false}
            },
            LayerCommand::Remove=>{
                if self.doc.layers.len()>1 {self.doc.remove_layer();true} else {false}
            },
            LayerCommand::Up=>{
                let old=self.doc.selected;
                self.doc.move_selected(true);
                self.doc.selected!=old
            },
            LayerCommand::Down=>{
                let old=self.doc.selected;
                self.doc.move_selected(false);
                self.doc.selected!=old
            },
        };
        if changed {self.history.clear();self.change();}
    }
    fn layers(&mut self,ui:&mut egui::Ui) {
        egui::ScrollArea::vertical().show(ui,|ui|{
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("LAYERS").strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center),|ui| {
                    ui.label(egui::RichText::new(format!("{} layers",self.doc.layers.len())).small().weak());
                });
            });
            let mut changed=false;
            ui.horizontal_wrapped(|ui|{
                for (label,help,command) in [
                    ("+","Add paint layer",LayerCommand::Add),
                    ("⧉","Duplicate selected layer",LayerCommand::Duplicate),
                    ("−","Delete layer",LayerCommand::Remove),
                    ("↑","Move up",LayerCommand::Up),
                    ("↓","Move down",LayerCommand::Down),
                ] {
                    if ui.small_button(label).on_hover_text(help).clicked() {self.layer_command(command);}
                }
                if ui.small_button("◐").on_hover_text("Invert selected mask").clicked() {
                    self.perform(Action::InvertMask);
                }
            });
            ui.separator();
            ui.horizontal(|ui| {
                egui::ComboBox::from_id_salt("layers-channel")
                    .selected_text(self.channel.label()).show_ui(ui,|ui| {
                        for channel in Channel::ALL {
                            if ui.selectable_value(&mut self.channel,channel,channel.label()).changed() {
                                self.uv_texture=None;
                                if self.solo_channel {self.model_dirty=true;}
                            }
                        }
                    });
            });
            egui::ScrollArea::vertical().max_height(360.0).show(ui,|ui|{
                for index in (0..self.doc.layers.len()).rev() {
                    let layer=&mut self.doc.layers[index];
                    let is_selected=self.doc.selected==index;
                    let title=if layer.pixels.contains_key(&self.channel) {
                        format!("▣ {}",layer.name)
                    } else {format!("□ {}",layer.name)};
                    ui.horizontal(|ui| {
                        if ui.checkbox(&mut layer.visible,"").on_hover_text("Toggle visibility").changed() {changed=true;}
                        if ui.selectable_label(is_selected,title).clicked() {self.doc.selected=index;}
                        if !layer.mask.pixels().all(|p|p.0[0]==255) {
                            ui.label(egui::RichText::new("◐").small()).on_hover_text("Masked layer");
                        }
                    });
                }
            });
            ui.separator();
            ui.label("Selected layer");
            if ui.text_edit_singleline(&mut self.doc.selected_layer_mut().name).changed(){self.unsaved=true;}
            let layer=self.doc.selected_layer_mut();
            if ui.add(egui::Slider::new(&mut layer.opacity,0.0..=1.0).text("Opacity")).changed(){changed=true;}
            egui::ComboBox::from_id_salt("blend_mode")
                .selected_text(format!("{:?}",layer.blend))
                .show_ui(ui,|ui|{
                    for blend in BlendMode::ALL {
                        if ui.selectable_value(&mut layer.blend,blend,format!("{:?}",blend)).changed(){changed=true;}
                    }
                });
            ui.separator();
            ui.label(egui::RichText::new("Layer channels (Painter-style toggles):").small().weak());
            ui.label(egui::RichText::new("Disabled channels keep their pixels for later reactivation.").small().weak());
            ui.horizontal_wrapped(|ui| {
                for c in Channel::ALL {
                    let mut enabled=!layer.disabled_channels.contains(&c);
                    let has_pixels=layer.pixels.contains_key(&c);
                    let label=format!("{}{}",if has_pixels {"● "}else{"○ "},c.label());
                    if ui.checkbox(&mut enabled,label).changed() {
                        if enabled {layer.disabled_channels.remove(&c);}
                        else {layer.disabled_channels.insert(c);}
                        changed=true;
                    }
                }
            });
            ui.horizontal_wrapped(|ui| {
                if ui.small_button("Invert mask").clicked(){self.perform(Action::InvertMask);}
                if ui.small_button("Reset mask").clicked(){self.perform(Action::ResetMask);}
                if ui.small_button("Clear channel").clicked(){self.perform(Action::ClearChannel);}
            });
            ui.add_space(10.0);
            ui.label(egui::RichText::new("Layer masks are editable with Mask + and Mask − brush modes.").small().weak());
            if changed {self.history.clear();self.change();}
        });
    }

    fn assets(&mut self,ui:&mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("Library");
            if ui.small_button("+ Import").clicked() {self.perform(Action::ImportTexture);}
        });
        ui.add(egui::TextEdit::singleline(&mut self.asset_search).hint_text("Search brushes and materials…"));
        ui.separator();
        egui::ScrollArea::vertical().show(ui,|ui| {
            ui.label(egui::RichText::new("BRUSH PRESETS").small().weak());
            let presets=[
                ("Soft Painter",42,0.50,0.15,Color32::from_rgb(150,168,178)),
                ("Hard Detail",8,1.0,0.95,Color32::from_rgb(202,178,156)),
                ("Surface Color",32,0.85,0.65,Color32::from_rgb(171,108,88)),
            ];
            ui.columns(2,|cols| {
                for (index,(label,radius,opacity,hardness,swatch)) in presets.into_iter().enumerate() {
                    if !label.to_ascii_lowercase().contains(&self.asset_search.to_ascii_lowercase()) {continue;}
                    cols[index%2].vertical_centered(|ui| {
                        let (rect,clicked)=ui.allocate_exact_size(egui::vec2(76.0,72.0),egui::Sense::click());
                        ui.painter().circle_filled(rect.center(),26.0,swatch);
                        ui.painter().circle_stroke(rect.center(),26.0,egui::Stroke::new(1.2_f32,Color32::from_gray(204)));
                        if clicked.clicked() {
                            self.brush.radius=radius;
                            self.brush.opacity=opacity;
                            self.brush.hardness=hardness;
                            self.brush.mode=BrushMode::Paint;
                            self.status=format!("Selected brush: {label}");
                        }
                        ui.label(egui::RichText::new(label).small());
                    });
                }
            });
            ui.separator();
            ui.label(egui::RichText::new("LOCAL ASSETS").small().weak());
            if ui.button("Load .kbrush…").clicked(){self.perform(Action::LoadBrush);}
            if ui.button("Import texture as layer…").clicked(){self.perform(Action::ImportTexture);}
            ui.label(egui::RichText::new("Substance .sppr / .spsm assets are not yet decoded. No AI-generated content.").weak().small());
        });
    }

    fn texture_sets(&mut self,ui:&mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("Texture Set List");
            if ui.small_button("Import mesh…").clicked(){self.perform(Action::ImportMesh);}
        });
        ui.separator();
        // V0.2: one texture set per document. Do not fabricate materials from a reference picture.
        let _=ui.selectable_label(true,format!("◉ Default  •  {}×{}",self.doc.size,self.doc.size))
            .on_hover_text("Current texture set; multi-material support is not yet available");
        ui.label(egui::RichText::new(format!("Mesh: {}",self.doc.mesh.name)).weak().small());
        ui.add_space(8.0);
        ui.label(egui::RichText::new("Multiple material texture sets and per-set shaders: planned.").weak().small());
    }

    fn texture_settings(&mut self,ui:&mut egui::Ui) {
        ui.heading("Texture Set Settings");
        ui.label("Texture resolution");
        ui.horizontal_wrapped(|ui| {
            for size in [256,512,1024,2048,4096] {
                if ui.selectable_label(size==self.doc.size,size.to_string()).clicked() && size!=self.doc.size {
                    self.finish_stroke();
                    self.doc.resize(size);
                    self.rebuild_coverage();
                    self.history.clear();
                    self.change();
                }
            }
        });
        ui.separator();
        if ui.checkbox(&mut self.show_uv_wire,"UV wireframe overlay").changed() {ui.ctx().request_repaint();}
        ui.add_enabled(self.uv_coverage.is_some(),egui::Checkbox::new(&mut self.uv_clipping,"Limit brush strokes to UV polygons"));
        if let Some(map)=&self.uv_coverage {
            ui.label(format!("Coverage: {:.1}%",100.0*map.covered_texels() as f32/(self.doc.size as f32).powi(2)));
        }
        ui.separator();
        ui.label(egui::RichText::new("Bake mesh maps, UDIM and per-material texture sets are not yet implemented.").weak().small());
    }

    fn properties(&mut self,ui:&mut egui::Ui) {
        ui.heading("Properties — Paint / Fill");
        ui.label("MATERIAL CHANNEL");
        egui::ComboBox::from_id_salt("properties_channel")
            .selected_text(self.channel.label()).show_ui(ui,|ui| {
                for channel in Channel::ALL {
                    if ui.selectable_value(&mut self.channel,channel,channel.label()).changed() {
                        self.uv_texture=None;
                        if self.solo_channel {self.model_dirty=true;}
                    }
                }
            });
        ui.horizontal_wrapped(|ui| {
            for channel in Channel::ALL {
                if ui.small_button(channel.label()).clicked() {
                    self.channel=channel;
                    self.uv_texture=None;
                    if self.solo_channel {self.model_dirty=true;}
                }
            }
        });
        ui.separator();
        ui.label("Active brush / fill value");
        if self.channel.is_color() {
            let mut color=Color32::from_rgb(self.brush.color[0],self.brush.color[1],self.brush.color[2]);
            if ui.color_edit_button_srgba(&mut color).changed() {
                self.brush.color=[color.r(),color.g(),color.b(),255];
            }
        } else {
            ui.add(egui::Slider::new(&mut self.brush.grayscale,0..=255).text(self.channel.label()));
        }
        ui.add(egui::Slider::new(&mut self.brush.opacity,0.01..=1.0).text("Opacity"));
        ui.add(egui::Slider::new(&mut self.brush.radius,1..=180).text("Size"));
        if ui.button("Fill selected layer channel / mask").clicked(){self.perform(Action::Fill);}
        ui.separator();
        ui.label(egui::RichText::new("Smart Materials and node generators: pending implementation.").weak().small());
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
        if self.pending_stroke.is_some() {return;}
        let mask=self.brush.mode.is_mask();
        let before=History::capture(&mut self.doc,self.channel,self.brush.mode);
        self.pending_stroke=Some(PendingStroke {
            layer_id:self.doc.selected_layer().id,channel:self.channel,mask,before,
        });
    }
    fn finish_stroke(&mut self) {
        self.previous_uv=None;
        let Some(pending)=self.pending_stroke.take() else {return;};
        let Some(layer)=self.doc.layers.iter().find(|l|l.id==pending.layer_id) else {return;};
        let after=if pending.mask {layer.mask.as_raw().clone()} else {
            layer.pixels.get(&pending.channel).map(|p|p.as_raw().clone()).unwrap_or_default()
        };
        self.history.commit(Snapshot {layer_id:pending.layer_id,channel:pending.channel,
            mask:pending.mask,before:pending.before,after});
    }
    fn fill_uv(&mut self,uv:[f32;2]) {
        self.finish_stroke();
        if self.uv_clipping && self.uv_coverage.as_ref().is_some_and(|c| !c.contains_uv(uv)) {
            self.status="Flood fill skipped: outside the UV polygons".into();
            return;
        }
        let before=History::capture(&mut self.doc,self.channel,self.brush.mode);
        let layer_id=self.doc.selected_layer().id;
        let coverage=if self.uv_clipping {self.uv_coverage.as_ref()} else {None};
        match ops::flood_fill(&mut self.doc,self.channel,uv,&self.brush,coverage) {
            Ok(true)=>{
                let after=History::capture(&mut self.doc,self.channel,self.brush.mode);
                self.history.commit(Snapshot {layer_id,channel:self.channel,mask:false,before,after});
                self.change();
                self.status=format!("Flood filled {} at UV ({:.3}, {:.3})",self.channel.label(),uv[0],uv[1]);
            }
            Ok(false)=>self.status="No matching pixels to fill at selected UV".into(),
            Err(err)=>self.status=format!("Flood fill failed: {err}")
        }
    }
    fn paint_uv(&mut self,uv:[f32;2]) {
        if uv.iter().any(|v|!v.is_finite() || *v < 0.0 || *v>1.0) {return;}
        if self.brush.mode==BrushMode::Sample {
            let Some(material)=self.material.as_ref() else {return;};
            let image=material.image(self.channel);
            let x=(uv[0]*(image.width()-1) as f32).round() as u32;
            let y=(uv[1]*(image.height()-1) as f32).round() as u32;
            let sampled=image.get_pixel(x,y).0;
            if self.channel.is_color() {self.brush.color=sampled;}
            else {self.brush.grayscale=sampled[0];}
            self.status=format!("Sampled {} at ({x},{y})",self.channel.label());
            return;
        }
        self.begin_stroke();
        let coverage=if self.uv_clipping {self.uv_coverage.as_ref()} else {None};
        self.brush.stroke_masked(&mut self.doc,self.channel,self.previous_uv,uv,coverage);
        self.previous_uv=Some(uv);
        self.change();
    }
    fn view_pane(&mut self,ui:&mut egui::Ui,canvas:Canvas) {
            let ctx=ui.ctx().clone();
            ui.horizontal(|ui|{
                ui.label(egui::RichText::new(&self.doc.mesh.name).weak().small());
                ui.separator();
                ui.label(if canvas==Canvas::Model {"Material · 3D"} else {"Material · UV"});
            });
            ui.separator();
            let available=ui.available_size();
            let size=if canvas==Canvas::UV {
                Vec2::splat(available.x.min(available.y).max(128.0).min(850.0))
            } else {
                egui::vec2(available.x.max(128.0).min(1100.0),
                    available.y.max(128.0).min(900.0))
            };
            if canvas==Canvas::Model {
                let desired=(size.x.round() as usize,size.y.round() as usize);
                if (self.view.width,self.view.height)!=desired {
                    self.view.resize(desired.0,desired.1);
                    self.model_dirty=true;
                    self.refresh_textures(&ctx);
                }
            }
            ui.vertical_centered(|ui|{
                let texture=match canvas {
                    Canvas::Model=>self.model_texture.as_ref(),Canvas::UV=>self.uv_texture.as_ref(),
                };
                let Some(texture)=texture else {return;};
                let response=ui.add(egui::Image::new((texture.id(),size))
                    .sense(egui::Sense::click_and_drag()));
                if canvas==Canvas::UV && self.show_uv_wire {
                    let painter=ui.painter_at(response.rect);
                    for triangle in self.doc.mesh.indices.chunks_exact(3).take(2500) {
                        let uv=[triangle[0],triangle[1],triangle[2]]
                            .map(|idx|self.doc.mesh.vertices[idx as usize].uv);
                        for i in 0..3 {
                            let a=uv[i];let b=uv[(i+1)%3];
                            painter.line_segment([
                                egui::pos2(response.rect.left()+a[0]*size.x,response.rect.top()+a[1]*size.y),
                                egui::pos2(response.rect.left()+b[0]*size.x,response.rect.top()+b[1]*size.y),
                            ],egui::Stroke::new(0.65_f32,Color32::from_rgba_unmultiplied(245,245,250,115)));
                        }
                    }
                }
                // The cursor overlay is a non-destructive UI guide, not part of the exported texture.
                if response.hovered() && !matches!(self.brush.mode,BrushMode::Sample|BrushMode::Bucket) {
                    if let Some(pointer)=ctx.input(|i|i.pointer.hover_pos()) {
                        let radius=self.brush.radius as f32 *
                            response.rect.width() / self.doc.size as f32;
                        ui.painter().circle_stroke(pointer,radius.max(1.0),
                            egui::Stroke::new(1.0_f32,Color32::from_gray(235)));
                    }
                }
                if canvas==Canvas::Model && response.hovered() {
                    let (secondary,delta,scroll)=ctx.input(|i|{
                        (i.pointer.secondary_down(),i.pointer.delta(),i.raw_scroll_delta.y)
                    });
                    if secondary {
                        self.view.orbit(delta.x,delta.y);
                        self.model_dirty=true;
                    }
                    if scroll!=0.0 {self.view.zoom(scroll);self.model_dirty=true;}
                }
                let (down,pressed,secondary,pointer)=ctx.input(|i|{
                    (i.pointer.primary_down(),i.pointer.primary_pressed(),i.pointer.secondary_down(),i.pointer.interact_pos())
                });
                let bucket=self.brush.mode==BrushMode::Bucket;
                if response.hovered() && (if bucket {pressed} else {down}) && !secondary {
                    if let Some(pos)=pointer {
                        let local=pos-response.rect.min;
                        let x=(local.x/response.rect.width()).clamp(0.0,0.999999);
                        let y=(local.y/response.rect.height()).clamp(0.0,0.999999);
                        let uv=match canvas {
                            Canvas::UV=>Some([x,y]),
                            Canvas::Model=>self.view.pick(
                                (x*self.view.width as f32) as usize,
                                (y*self.view.height as f32) as usize),
                        };
                        if let Some(uv)=uv {
                            if bucket {self.fill_uv(uv);} else {self.paint_uv(uv);}
                        }
                        else {self.previous_uv=None;}
                    }
                }
            });
    }
}

impl eframe::App for KaliumApp {
    fn update(&mut self,ctx:&egui::Context,_frame:&mut eframe::Frame) {
        let down=ctx.input(|i|i.pointer.primary_down());
        if !down {self.finish_stroke();}
        self.menu(ctx);
        self.tool_shelf(ctx);
        egui::TopBottomPanel::bottom("status").show(ctx,|ui|{
            ui.horizontal_wrapped(|ui|{
                ui.label(egui::RichText::new(format!("KALIUM {} α · DrapNard",env!("CARGO_PKG_VERSION"))).color(Color32::from_rgb(156,201,248)));
                ui.separator();
                ui.label(&self.status);
                if self.unsaved {ui.label(egui::RichText::new("● Unsaved").color(Color32::YELLOW));}
            });
        });
        self.refresh_textures(ctx);
        egui::CentralPanel::default().frame(egui::Frame::NONE).show(ctx,|ui|{
            // Temporarily take the tree: TabViewer gets exclusive access to app state.
            let mut dock=std::mem::replace(&mut self.dock,DockState::new(vec![]));
            let style=Style::from_egui(ui.style().as_ref());
            DockArea::new(&mut dock).style(style).show_inside(ui,self);
            self.dock=dock;
        });
        self.autosave_workspace();
        ctx.request_repaint_after(Duration::from_secs(2));
        if self.paint_dirty || self.model_dirty {ctx.request_repaint();}
    }
}


impl TabViewer for KaliumApp {
    type Tab=Pane;
    fn title(&mut self,pane:&mut Pane)->egui::WidgetText {pane.label().into()}
    fn ui(&mut self,ui:&mut egui::Ui,pane:&mut Pane) {
        match pane {
            Pane::Assets=>self.assets(ui),
            Pane::Viewport3D=>self.view_pane(ui,Canvas::Model),
            Pane::Viewport2D=>self.view_pane(ui,Canvas::UV),
            Pane::TextureSets=>self.texture_sets(ui),
            Pane::Layers=>self.layers(ui),
            Pane::TextureSettings=>self.texture_settings(ui),
            Pane::Properties=>self.properties(ui),
            Pane::BrushStudio=>self.tools(ui),
        }
    }
}
