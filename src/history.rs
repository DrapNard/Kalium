use crate::document::{Channel,Document};
use crate::paint::BrushMode;

pub const MAX_HISTORY:usize=14;

pub struct Snapshot {
    pub layer_id:u32,
    pub channel:Channel,
    pub mask:bool,
    pub before:Vec<u8>,
    pub after:Vec<u8>,
}

pub struct History {
    undo:Vec<Snapshot>,
    redo:Vec<Snapshot>,
}

impl History {
    pub fn new()->Self {Self {undo:Vec::new(),redo:Vec::new()}}
    pub fn clear(&mut self) {self.undo.clear();self.redo.clear();}
    pub fn capture(doc:&mut Document,channel:Channel,mode:BrushMode)->Vec<u8> {
        let size=doc.size;
        let layer=doc.selected_layer_mut();
        if mode.is_mask() {layer.mask.as_raw().clone()}
        else {layer.pixels_mut(channel,size).as_raw().clone()}
    }
    pub fn commit(&mut self,entry:Snapshot) {
        if entry.before==entry.after {return;}
        self.undo.push(entry);
        if self.undo.len()>MAX_HISTORY {self.undo.remove(0);}
        self.redo.clear();
    }
    fn apply(doc:&mut Document,snapshot:&Snapshot,undo:bool)->bool {
        let Some(layer)=doc.layers.iter_mut().find(|l|l.id==snapshot.layer_id) else {return false};
        let bytes=if undo {&snapshot.before} else {&snapshot.after};
        if snapshot.mask {
            if bytes.len()!=(doc.size as usize).pow(2) {return false;}
            layer.mask.as_mut().copy_from_slice(bytes);
        } else {
            let size=doc.size;
            if bytes.len()!=(size as usize).pow(2)*4 {return false;}
            layer.pixels_mut(snapshot.channel,size).as_mut().copy_from_slice(bytes);
        }
        true
    }
    pub fn undo(&mut self,doc:&mut Document)->bool {
        let Some(entry)=self.undo.pop() else {return false};
        if Self::apply(doc,&entry,true) {self.redo.push(entry);true} else {false}
    }
    pub fn redo(&mut self,doc:&mut Document)->bool {
        let Some(entry)=self.redo.pop() else {return false};
        if Self::apply(doc,&entry,false) {self.undo.push(entry);true} else {false}
    }
}
