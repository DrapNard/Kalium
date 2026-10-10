use crate::document::{Channel,Document};
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone,Copy,PartialEq,Eq,Debug,Serialize,Deserialize)]
pub enum BrushMode { Paint, Erase, MaskWhite, MaskBlack, Sample }

impl BrushMode {
    pub const ALL:[Self;5]=[Self::Paint,Self::Erase,Self::MaskWhite,Self::MaskBlack,Self::Sample];
    pub fn label(self)->&'static str {match self {
        Self::Paint=>"Paint",Self::Erase=>"Erase",Self::MaskWhite=>"Mask +",Self::MaskBlack=>"Mask −",Self::Sample=>"Eyedropper",
    }}
    pub fn is_mask(self)->bool {matches!(self,Self::MaskWhite|Self::MaskBlack)}
}

#[derive(Clone,Debug,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Brush {
    pub radius:u32,
    pub opacity:f32,
    pub hardness:f32,
    pub spacing:f32,
    pub mode:BrushMode,
    pub color:[u8;4],
    pub grayscale:u8,
    pub mirror_u:bool,
}

impl Default for Brush {
    fn default()->Self { Self {radius:22,opacity:0.85,hardness:0.65,spacing:0.25,
        mode:BrushMode::Paint,color:[225,99,137,255],grayscale:175,mirror_u:false} }
}

impl Brush {
    pub fn validate(&self) -> Result<()> {
        if !(1..=4096).contains(&self.radius) { bail!("Brush radius must be 1..=4096"); }
        if !self.opacity.is_finite() || !(0.0..=1.0).contains(&self.opacity) { bail!("Invalid opacity"); }
        if !self.hardness.is_finite() || !(0.0..=1.0).contains(&self.hardness) { bail!("Invalid hardness"); }
        if !self.spacing.is_finite() || !(0.1..=4.0).contains(&self.spacing) { bail!("Invalid spacing"); }
        Ok(())
    }
    pub fn stamp(&self,doc:&mut Document,channel:Channel,uv:[f32;2]) {
        if self.mode==BrushMode::Sample {return;}
        if uv.iter().any(|v|!v.is_finite()) {return;}
        let size=doc.size;
        let radius=self.radius.max(1) as f32;
        let cx=uv[0]*(size-1) as f32;
        let cy=uv[1]*(size-1) as f32;
        let min_x=(cx-radius-1.0).floor().max(0.0) as u32;
        let min_y=(cy-radius-1.0).floor().max(0.0) as u32;
        let max_x=(cx+radius+1.0).ceil().min((size-1) as f32) as u32;
        let max_y=(cy+radius+1.0).ceil().min((size-1) as f32) as u32;
        let mode=self.mode;
        let hardness=self.hardness.clamp(0.0,1.0);
        let target = if channel.is_color() { self.color } else {[self.grayscale,self.grayscale,self.grayscale,255]};
        let layer=doc.selected_layer_mut();
        if mode.is_mask() {
            for y in min_y..=max_y {
                for x in min_x..=max_x {
                    let dx=x as f32-cx;let dy=y as f32-cy;
                    let distance=(dx*dx+dy*dy).sqrt()/radius;
                    if distance>1.0 {continue;}
                    let feather=if distance<=hardness {1.0} else {
                        let t=((distance-hardness)/(1.0-hardness).max(0.0001)).clamp(0.0,1.0);
                        1.0-t*t*(3.0-2.0*t)
                    };
                    let a=(self.opacity*feather).clamp(0.0,1.0);
                    if a<=0.0 {continue;}
                    let pixel=layer.mask.get_pixel_mut(x,y);
                    let target=if mode==BrushMode::MaskWhite {255.0} else {0.0};
                    pixel.0[0]=(pixel.0[0] as f32*(1.0-a)+target*a).round() as u8;
                }
            }
        }
        if !mode.is_mask() {
            let image=layer.pixels_mut(channel,size);
            for y in min_y..=max_y {
                for x in min_x..=max_x {
                    let dx=x as f32-cx;let dy=y as f32-cy;
                    let distance=(dx*dx+dy*dy).sqrt()/radius;
                    if distance>1.0 {continue;}
                    let feather=if distance<=hardness {1.0} else {
                        let t=((distance-hardness)/(1.0-hardness).max(0.0001)).clamp(0.0,1.0);
                        1.0-t*t*(3.0-2.0*t)
                    };
                    let a=(self.opacity*feather).clamp(0.0,1.0);
                    if a<=0.0 {continue;}
                    let pixel=image.get_pixel_mut(x,y);
                    if mode==BrushMode::Erase {
                        pixel.0[3]=(pixel.0[3] as f32*(1.0-a)).round() as u8;
                    } else {
                        let old_a=pixel.0[3] as f32/255.0;
                        let new_a=a+old_a*(1.0-a);
                        for c in 0..3 {
                            pixel.0[c]=((target[c] as f32*a+pixel.0[c] as f32*old_a*(1.0-a))/new_a.max(1e-6))
                                .round().clamp(0.0,255.0) as u8;
                        }
                        pixel.0[3]=(new_a*255.0).round() as u8;
                    }
                }
            }
        }
    }

    pub fn stroke(&self,doc:&mut Document,channel:Channel,from:Option<[f32;2]>,to:[f32;2]) {
        let stamp=|uv:[f32;2],doc:&mut Document| {
            self.stamp(doc,channel,uv);
            if self.mirror_u {self.stamp(doc,channel,[1.0-uv[0],uv[1]]);}
        };
        let Some(from)=from else {stamp(to,doc);return;};
        let du=from[0]-to[0];let dv=from[1]-to[1];
        let dist=(du*du+dv*dv).sqrt()*doc.size as f32;
        // UV islands must not be connected by a long cross-atlas stroke.
        if dist>doc.size as f32*0.24 {stamp(to,doc);return;}
        let spacing=(self.radius.max(1) as f32*self.spacing.max(0.1)).max(1.0);
        let steps=(dist/spacing).ceil().max(1.0).min(1024.0) as usize;
        for i in 1..=steps {
            let t=i as f32/steps as f32;
            stamp([from[0]+(to[0]-from[0])*t,from[1]+(to[1]-from[1])*t],doc);
        }
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn painting_changes_only_pixels_in_brush() {
        let mut doc=Document::new(64);
        let brush=Brush {radius:5,opacity:1.0,hardness:1.0,..Brush::default()};
        brush.stamp(&mut doc,Channel::BaseColor,[0.5,0.5]);
        let pixels=doc.selected_layer().pixels.get(&Channel::BaseColor).unwrap();
        assert_eq!(pixels.get_pixel(0,0).0,[0,0,0,0]);
        assert!(pixels.get_pixel(32,32).0[3]>0);
    }
    #[test]
    fn eraser_removes_alpha() {
        let mut doc=Document::new(64);
        let mut brush=Brush {radius:6,opacity:1.0,hardness:1.0,..Brush::default()};
        brush.stamp(&mut doc,Channel::BaseColor,[0.5,0.5]);
        brush.mode=BrushMode::Erase;
        brush.stamp(&mut doc,Channel::BaseColor,[0.5,0.5]);
        let pixels=doc.selected_layer().pixels.get(&Channel::BaseColor).unwrap();
        assert_eq!(pixels.get_pixel(32,32).0[3],0);
    }
}
