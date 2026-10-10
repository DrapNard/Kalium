//! Bounded, sparse undo/redo history. Dense snapshots only live during a stroke;
//! history retains copies of the changed 4-KiB chunks, not whole 4K textures.
use crate::document::{Channel,Document};
use crate::paint::BrushMode;

pub const MAX_HISTORY:usize=64;
pub const MAX_HISTORY_BYTES:usize=96*1024*1024;
const CHUNK_BYTES:usize=4096;

pub struct Snapshot {
    pub layer_id:u32,
    pub channel:Channel,
    pub mask:bool,
    pub before:Vec<u8>,
    pub after:Vec<u8>,
}

struct Patch { offset:usize, before:Vec<u8>, after:Vec<u8> }
struct Delta {
    layer_id:u32,
    channel:Channel,
    mask:bool,
    patches:Vec<Patch>,
    size:usize,
}

pub struct History {
    undo:Vec<Delta>,
    redo:Vec<Delta>,
    used_bytes:usize,
}

impl History {
    pub fn new()->Self {Self {undo:Vec::new(),redo:Vec::new(),used_bytes:0}}
    pub fn clear(&mut self) {self.undo.clear();self.redo.clear();self.used_bytes=0;}
    pub fn capture(doc:&mut Document,channel:Channel,mode:BrushMode)->Vec<u8> {
        let size=doc.size;
        let layer=doc.selected_layer_mut();
        if mode.is_mask() {layer.mask.as_raw().clone()}
        else {layer.pixels_mut(channel,size).as_raw().clone()}
    }
    pub fn commit(&mut self,entry:Snapshot) {
        if entry.before.len()!=entry.after.len() || entry.before==entry.after {return;}
        let mut patches=Vec::new();
        let mut stored=0usize;
        for (index,(before,after)) in entry.before.chunks(CHUNK_BYTES)
            .zip(entry.after.chunks(CHUNK_BYTES)).enumerate() {
            if before==after {continue;}
            stored=stored.saturating_add(before.len().saturating_mul(2));
            patches.push(Patch {offset:index*CHUNK_BYTES,before:before.to_vec(),after:after.to_vec()});
        }
        let delta=Delta {layer_id:entry.layer_id,channel:entry.channel,mask:entry.mask,
            patches,size:stored};
        self.used_bytes=self.used_bytes.saturating_sub(self.redo.iter().map(|d|d.size).sum::<usize>());
        self.redo.clear();
        // A massive full-channel replacement may exceed the history budget.
        // Apply the edit but do not claim it is undoable.
        if stored>MAX_HISTORY_BYTES {self.clear();return;}
        self.used_bytes=self.used_bytes.saturating_add(stored);
        self.undo.push(delta);
        while self.undo.len()>MAX_HISTORY || self.used_bytes>MAX_HISTORY_BYTES {
            let oldest=self.undo.remove(0);
            self.used_bytes=self.used_bytes.saturating_sub(oldest.size);
        }
    }
    fn apply(doc:&mut Document,delta:&Delta,undo:bool)->bool {
        let Some(layer)=doc.layers.iter_mut().find(|l|l.id==delta.layer_id) else {return false};
        let size=doc.size;
        let target: &mut [u8]=if delta.mask {
            layer.mask.as_mut()
        } else {
            layer.pixels_mut(delta.channel,size).as_mut()
        };
        if delta.patches.iter().any(|p|p.offset.checked_add(p.before.len())
            .map_or(true,|end|end>target.len())) {return false;}
        for patch in &delta.patches {
            let source=if undo {&patch.before} else {&patch.after};
            target[patch.offset..patch.offset+source.len()].copy_from_slice(source);
        }
        true
    }
    pub fn undo(&mut self,doc:&mut Document)->bool {
        let Some(entry)=self.undo.pop() else {return false};
        if Self::apply(doc,&entry,true) {self.redo.push(entry);true}
        else {self.clear();false}
    }
    pub fn redo(&mut self,doc:&mut Document)->bool {
        let Some(entry)=self.redo.pop() else {return false};
        if Self::apply(doc,&entry,false) {self.undo.push(entry);true}
        else {self.clear();false}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgba;
    #[test]
    fn sparse_undo_redo_reproduces_pixels() {
        let mut doc=Document::new(64);
        let mut history=History::new();
        let before=History::capture(&mut doc,Channel::BaseColor,BrushMode::Paint);
        doc.selected_layer_mut().pixels_mut(Channel::BaseColor,64)
            .put_pixel(1,1,Rgba([20,40,60,255]));
        let after=History::capture(&mut doc,Channel::BaseColor,BrushMode::Paint);
        history.commit(Snapshot {layer_id:1,channel:Channel::BaseColor,mask:false,before,after});
        assert!(history.undo(&mut doc));
        assert_eq!(doc.selected_layer().pixels[&Channel::BaseColor].get_pixel(1,1).0,[0,0,0,0]);
        assert!(history.redo(&mut doc));
        assert_eq!(doc.selected_layer().pixels[&Channel::BaseColor].get_pixel(1,1).0,[20,40,60,255]);
        assert!(history.used_bytes<=8192);
    }
    #[test]
    fn no_op_does_not_create_history_entry() {
        let mut doc=Document::new(64);
        let mut history=History::new();
        let before=History::capture(&mut doc,Channel::BaseColor,BrushMode::Paint);
        let after=before.clone();
        history.commit(Snapshot {layer_id:1,channel:Channel::BaseColor,mask:false,before,after});
        assert!(!history.undo(&mut doc));
    }
}
