use crate::{
    document::{BlendMode, Channel, Document},
    export,history::{History,Snapshot},mesh::Mesh,paint::{Brush,BrushMode},
    project,renderer::{Material,Viewport},spp,
};
use eframe::egui::{self,Color32,ColorImage,TextureHandle,TextureOptions,Vec2};
use std::path::PathBuf;

#[derive(Clone,Copy,PartialEq,Eq)]
enum Canvas {Model,UV}
#[derive(Clone,Copy)]
enum Action {New,Open,Save,SaveAs,ImportMesh,Export,InspectSpp}
struct PendingStroke {layer_id:u32,channel:Channel,mask:bool,before:Vec<u8>}

pub struct KaliumApp {
    doc:Document,
    brush:Brush,
    channel:Channel,
    canvas:Canvas,
    solo_channel:bool,
    show_uv_wire:bool,
    view:Viewport,
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
}

impl KaliumApp {
    pub fn new(cc:&eframe::CreationContext<'_>)->Self {
        let mut visuals=egui::Visuals::dark();
        visuals.selection.bg_fill=Color32::from_rgb(106,87,180);
        visuals.widgets.active.bg_fill=Color32::from_rgb(97,78,176);
        visuals.widgets.hovered.bg_fill=Color32::from_rgb(70,59,108);
        cc.egui_ctx.set_visuals(visuals);
        Self {
            doc:Document::new(512),brush:Brush::default(),channel:Channel::BaseColor,
            canvas:Canvas::Model,solo_channel:false,show_uv_wire:true,
            view:Viewport::new(600,600),material:None,model_texture:None,
            uv_texture:None,paint_dirty:true,model_dirty:true,
            history:History::new(),previous_uv:None,pending_stroke:None,
            file_path:None,unsaved:false,
            status:"Ready • Left drag to paint • Right drag to orbit • Scroll to zoom".into(),
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
                    self.unsaved=false;self.view=Viewport::new(600,600);self.change();
                    self.unsaved=false;
                    Ok(Some("New Kalium project created".into()))
                }
                Action::Open=>{
                    let Some(path)=rfd::FileDialog::new()
                        .add_filter("Kalium project", &["kalium"]).pick_file() else {return Ok(None)};
                    let doc=project::load(&path)?;
                    self.finish_stroke();
                    self.doc=doc;
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
                    self.history.clear();self.change();
                    Ok(Some(format!("Imported {}",path.display())))
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
                ui.heading(egui::RichText::new("◆ Kalium").color(Color32::from_rgb(183,155,255)));
                ui.separator();
                ui.menu_button("File",|ui|{
                    for (label,a) in [
                        ("New",Action::New),("Open .kalium…",Action::Open),
                        ("Save",Action::Save),("Save As…",Action::SaveAs),
                        ("Import OBJ / glTF / GLB…",Action::ImportMesh),
                        ("Export PBR textures…",Action::Export),
                        ("Inspect SPP signature (experimental)…",Action::InspectSpp),
                    ] {
                        if ui.button(label).clicked(){action=Some(a);ui.close_menu();}
                    }
                });
                ui.separator();
                if ui.button("Undo").clicked(){if self.history.undo(&mut self.doc){self.change();}}
                if ui.button("Redo").clicked(){if self.history.redo(&mut self.doc){self.change();}}
                ui.separator();
                ui.label(egui::RichText::new("AGPLv3 • No generative AI • Offline").small().weak());
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
    }
    fn tools(&mut self,ctx:&egui::Context) {
        egui::SidePanel::left("tool_sidebar").resizable(true).default_width(258.0).show(ctx,|ui|{
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
            ui.separator();
            ui.heading("Display");
            if ui.checkbox(&mut self.solo_channel,"Show selected channel in 3D").changed(){self.model_dirty=true;}
            ui.checkbox(&mut self.show_uv_wire,"UV wireframe overlay");
            ui.separator();
            ui.label(egui::RichText::new("TEXTURE RESOLUTION").small().weak());
            ui.label(format!("{} × {}",self.doc.size,self.doc.size));
            ui.horizontal_wrapped(|ui|{
                for size in [256,512,1024,2048,4096] {
                    if ui.selectable_label(self.doc.size==size,format!("{size}")).clicked() {
                        self.finish_stroke();
                        self.doc.resize(size);self.history.clear();self.change();
                    }
                }
            });
            ui.add_space(8.0);
            ui.label(egui::RichText::new("TIP").small().weak());
            ui.label("Left-drag paints on the 3D surface or UV image. Right-drag rotates the model.");
            ui.label("Mouse wheel zooms. Ctrl+S saves. Ctrl+Z / Ctrl+Y undo or redo strokes.");
        });
    }
    fn layers(&mut self,ctx:&egui::Context) {
        egui::SidePanel::right("layers_sidebar").resizable(true).default_width(265.0).show(ctx,|ui|{
            ui.heading("Layer Stack");
            let mut changed=false;
            ui.horizontal(|ui|{
                if ui.button("+ Add").clicked() {
                    match self.doc.add_layer() {Ok(())=>changed=true,Err(err)=>self.status=err.to_string()}
                }
                if ui.button("−").clicked() {self.doc.remove_layer();changed=true;}
                if ui.button("↑").clicked(){self.doc.move_selected(true);changed=true;}
                if ui.button("↓").clicked(){self.doc.move_selected(false);changed=true;}
            });
            ui.separator();
            egui::ScrollArea::vertical().max_height(360.0).show(ui,|ui|{
                for index in (0..self.doc.layers.len()).rev() {
                    let layer=&mut self.doc.layers[index];
                    ui.horizontal(|ui|{
                        if ui.checkbox(&mut layer.visible,"").changed(){changed=true;}
                        if ui.selectable_label(self.doc.selected==index,&layer.name).clicked(){self.doc.selected=index;}
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
            ui.label(egui::RichText::new("Channels painted on this layer:").small().weak());
            for c in Channel::ALL {
                if layer.pixels.contains_key(&c) {ui.label(format!("• {}",c.label()));}
            }
            ui.add_space(10.0);
            ui.label(egui::RichText::new("Layer masks are editable with Mask + and Mask − brush modes.").small().weak());
            if changed {self.history.clear();self.change();}
        });
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
    fn paint_uv(&mut self,uv:[f32;2]) {
        if uv.iter().any(|v|!v.is_finite() || *v < 0.0 || *v>1.0) {return;}
        self.begin_stroke();
        self.brush.stroke(&mut self.doc,self.channel,self.previous_uv,uv);
        self.previous_uv=Some(uv);
        self.change();
    }
    fn central(&mut self,ctx:&egui::Context) {
        egui::CentralPanel::default().show(ctx,|ui|{
            ui.horizontal(|ui|{
                if ui.selectable_label(self.canvas==Canvas::Model,"◈ 3D Viewport").clicked(){self.canvas=Canvas::Model;}
                if ui.selectable_label(self.canvas==Canvas::UV,"▦ 2D UV Editor").clicked(){self.canvas=Canvas::UV;}
                ui.separator();
                ui.label(egui::RichText::new(&self.doc.mesh.name).weak().small());
            });
            ui.separator();
            self.refresh_textures(ctx);
            let available=ui.available_size();
            let side=available.x.min(available.y).max(128.0).min(850.0);
            let size=Vec2::splat(side);
            ui.vertical_centered(|ui|{
                let texture=match self.canvas {
                    Canvas::Model=>self.model_texture.as_ref(),Canvas::UV=>self.uv_texture.as_ref(),
                };
                let Some(texture)=texture else {return;};
                let response=ui.add(egui::Image::new((texture.id(),size))
                    .sense(egui::Sense::click_and_drag()));
                if self.canvas==Canvas::UV && self.show_uv_wire {
                    let painter=ui.painter_at(response.rect);
                    for triangle in self.doc.mesh.indices.chunks_exact(3).take(2500) {
                        let uv=[triangle[0],triangle[1],triangle[2]]
                            .map(|idx|self.doc.mesh.vertices[idx as usize].uv);
                        for i in 0..3 {
                            let a=uv[i];let b=uv[(i+1)%3];
                            painter.line_segment([
                                egui::pos2(response.rect.left()+a[0]*side,response.rect.top()+a[1]*side),
                                egui::pos2(response.rect.left()+b[0]*side,response.rect.top()+b[1]*side),
                            ],egui::Stroke::new(0.65,Color32::from_rgba_unmultiplied(245,245,250,115)));
                        }
                    }
                }
                if self.canvas==Canvas::Model && response.hovered() {
                    let (secondary,delta,scroll)=ctx.input(|i|{
                        (i.pointer.secondary_down(),i.pointer.delta(),i.raw_scroll_delta.y)
                    });
                    if secondary {
                        self.view.orbit(delta.x,delta.y);
                        self.model_dirty=true;
                    }
                    if scroll!=0.0 {self.view.zoom(scroll);self.model_dirty=true;}
                }
                let (down,secondary,pointer)=ctx.input(|i|{
                    (i.pointer.primary_down(),i.pointer.secondary_down(),i.pointer.interact_pos())
                });
                if response.hovered() && down && !secondary {
                    if let Some(pos)=pointer {
                        let local=pos-response.rect.min;
                        let x=(local.x/response.rect.width()).clamp(0.0,0.999999);
                        let y=(local.y/response.rect.height()).clamp(0.0,0.999999);
                        let uv=match self.canvas {
                            Canvas::UV=>Some([x,y]),
                            Canvas::Model=>self.view.pick(
                                (x*self.view.width as f32) as usize,
                                (y*self.view.height as f32) as usize),
                        };
                        if let Some(uv)=uv {self.paint_uv(uv);}
                        else {self.previous_uv=None;}
                    }
                }
            });
        });
    }
}

impl eframe::App for KaliumApp {
    fn update(&mut self,ctx:&egui::Context,_frame:&mut eframe::Frame) {
        let down=ctx.input(|i|i.pointer.primary_down());
        if !down {self.finish_stroke();}
        self.menu(ctx);
        egui::TopBottomPanel::bottom("status").show(ctx,|ui|{
            ui.horizontal_wrapped(|ui|{
                ui.label(egui::RichText::new("KALIUM 0.1 α").color(Color32::from_rgb(178,149,247)));
                ui.separator();
                ui.label(&self.status);
                if self.unsaved {ui.label(egui::RichText::new("● Unsaved").color(Color32::YELLOW));}
            });
        });
        self.tools(ctx);
        self.layers(ctx);
        self.central(ctx);
        if self.paint_dirty || self.model_dirty {ctx.request_repaint();}
    }
}
