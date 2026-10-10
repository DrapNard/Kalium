//! Deterministic reference viewport. The desktop UI runs on wgpu; this
//! software rasterizer provides predictable UV hit-testing for the first alpha.
//! A native wgpu mesh/compute path is a planned milestone, not implemented here.
use crate::document::{Channel,Document};
use crate::mesh::Mesh;
use glam::{Vec2,Vec3};
use image::RgbaImage;
use std::collections::HashMap;

pub struct Material {
    maps:HashMap<Channel,RgbaImage>,
}
impl Material {
    pub fn from_document(doc:&Document)->Self {
        Self {maps:Channel::ALL.into_iter().map(|c|(c,doc.composite(c))).collect()}
    }
    pub fn get(&self,ch:Channel,uv:Vec2)->[u8;4] {
        let img=&self.maps[&ch];
        let x=(uv.x.clamp(0.0,1.0)*(img.width()-1) as f32).round() as u32;
        let y=(uv.y.clamp(0.0,1.0)*(img.height()-1) as f32).round() as u32;
        img.get_pixel(x,y).0
    }
    pub fn image(&self,ch:Channel)->&RgbaImage {&self.maps[&ch]}
}

#[derive(Clone,Copy)]
struct ScreenVertex {xy:Vec2,z:f32,uv:Vec2,n:Vec3,p:Vec3}

pub struct Viewport {
    pub width:usize,
    pub height:usize,
    pub rgba:Vec<u8>,
    uv:Vec<Option<[f32;2]>>,
    pub yaw:f32,
    pub pitch:f32,
    pub distance:f32,
}
impl Viewport {
    pub fn new(width:usize,height:usize)->Self {
        Self {width,height,rgba:vec![0;width*height*4],uv:vec![None;width*height],
            yaw:0.42,pitch:0.25,distance:4.2}
    }
    /// Resize the software framebuffer without resetting the orbit camera.
    /// Keeps hit testing aligned with the displayed 3D editor after docking.
    pub fn resize(&mut self,width:usize,height:usize) {
        let width=width.clamp(128,1100);
        let height=height.clamp(128,900);
        if self.width==width && self.height==height {return;}
        self.width=width;
        self.height=height;
        self.rgba.resize(width*height*4,0);
        self.uv.resize(width*height,None);
        self.uv.fill(None);
    }
    pub fn orbit(&mut self,dx:f32,dy:f32) {
        self.yaw += dx*0.012;
        self.pitch=(self.pitch-dy*0.012).clamp(-1.45,1.45);
    }
    pub fn zoom(&mut self,delta:f32) {
        self.distance=(self.distance*(-delta*0.001).exp()).clamp(1.6,15.0);
    }
    pub fn pick(&self,x:usize,y:usize)->Option<[f32;2]> {
        if x>=self.width||y>=self.height {None}else{self.uv[y*self.width+x]}
    }
    pub fn render(&mut self,mesh:&Mesh,mat:&Material,solo:Option<Channel>) {
        let size=self.width*self.height;
        if self.rgba.len()!=size*4 {self.rgba.resize(size*4,0);}
        if self.uv.len()!=size {self.uv.resize(size,None);}
        self.uv.fill(None);
        let mut depths=vec![f32::INFINITY;size];
        for y in 0..self.height {for x in 0..self.width {
            let i=(y*self.width+x)*4;
            let shade=((y as f32/self.height as f32)*14.0).round() as u8;
            self.rgba[i..i+4].copy_from_slice(&[27+shade/3,27+shade/3,36+shade,255]);
        }}
        let eye=Vec3::new(self.distance*self.yaw.sin()*self.pitch.cos(),
            self.distance*self.pitch.sin(),self.distance*self.yaw.cos()*self.pitch.cos());
        let forward=(-eye).normalize();
        let right=forward.cross(Vec3::Y).normalize_or_zero();
        let up=right.cross(forward).normalize_or_zero();
        let f=self.height as f32*1.08;
        let project=|v:&crate::mesh::Vertex|->ScreenVertex {
            let p=Vec3::from_array(v.position);
            let rel=p-eye;
            let z=rel.dot(forward);
            ScreenVertex {xy:Vec2::new(self.width as f32*0.5+rel.dot(right)*f/z,
                self.height as f32*0.5-rel.dot(up)*f/z),z,uv:Vec2::from_array(v.uv),
                n:Vec3::from_array(v.normal),p}
        };
        let projected:Vec<_>=mesh.vertices.iter().map(project).collect();
        let light=Vec3::new(0.3,0.83,0.55).normalize();
        for face in mesh.indices.chunks_exact(3) {
            let a=projected[face[0] as usize];
            let b=projected[face[1] as usize];
            let c=projected[face[2] as usize];
            if a.z<0.05||b.z<0.05||c.z<0.05 {continue;}
            if !a.xy.is_finite()||!b.xy.is_finite()||!c.xy.is_finite() {continue;}
            let edge=|p:Vec2,q:Vec2,r:Vec2| (r.x-p.x)*(q.y-p.y)-(r.y-p.y)*(q.x-p.x);
            let area=edge(a.xy,b.xy,c.xy);
            if area.abs()<0.0001 {continue;}
            let x0=a.xy.x.min(b.xy.x).min(c.xy.x).floor().max(0.0) as usize;
            let x1=a.xy.x.max(b.xy.x).max(c.xy.x).ceil().min((self.width-1) as f32) as usize;
            let y0=a.xy.y.min(b.xy.y).min(c.xy.y).floor().max(0.0) as usize;
            let y1=a.xy.y.max(b.xy.y).max(c.xy.y).ceil().min((self.height-1) as f32) as usize;
            if x0>x1||y0>y1 {continue;}
            // Approximate per-triangle tangent basis for tangent-space normals.
            let dp1=b.p-a.p;let dp2=c.p-a.p;
            let dt1=b.uv-a.uv;let dt2=c.uv-a.uv;
            let det=dt1.x*dt2.y-dt1.y*dt2.x;
            let tangent=if det.abs()>0.000001 {
                ((dp1*dt2.y-dp2*dt1.y)/det).normalize_or_zero()
            } else {Vec3::X};
            for y in y0..=y1 {for x in x0..=x1 {
                let sample=Vec2::new(x as f32+0.5,y as f32+0.5);
                let u=edge(b.xy,c.xy,sample)/area;
                let v=edge(c.xy,a.xy,sample)/area;
                let w=1.0-u-v;
                if u < -0.00001||v < -0.00001||w < -0.00001 {continue;}
                let ia=u/a.z;let ib=v/b.z;let ic=w/c.z;
                let inv_z=ia+ib+ic;
                if inv_z<0.000001 {continue;}
                let z=1.0/inv_z;
                let idx=y*self.width+x;
                if z>=depths[idx] {continue;}
                depths[idx]=z;
                let uv=(a.uv*ia+b.uv*ib+c.uv*ic)*z;
                let n=(a.n*ia+b.n*ib+c.n*ic).normalize_or_zero();
                let normal=if solo.is_none() {
                    let tex=mat.get(Channel::Normal,uv);
                    let nt=Vec3::new((tex[0] as f32/255.0)*2.0-1.0,
                        (tex[1] as f32/255.0)*2.0-1.0,(tex[2] as f32/255.0)*2.0-1.0);
                    let t=(tangent-n*tangent.dot(n)).normalize_or_zero();
                    let bitangent=n.cross(t);
                    (t*nt.x+bitangent*nt.y+n*nt.z).normalize_or_zero()
                } else {n};
                let diffuse=mat.get(solo.unwrap_or(Channel::BaseColor),uv);
                let rgb=if solo.is_some() {
                    [diffuse[0],diffuse[1],diffuse[2]]
                } else {
                    let rough=mat.get(Channel::Roughness,uv)[0] as f32/255.0;
                    let metal=mat.get(Channel::Metallic,uv)[0] as f32/255.0;
                    let ao=mat.get(Channel::Occlusion,uv)[0] as f32/255.0;
                    let em=mat.get(Channel::Emissive,uv);
                    let ndotl=normal.dot(light).max(0.0);
                    let ambient=0.20+0.20*ao;
                    let spec=(1.0-rough).powf(3.0)*(ndotl.powf(24.0))*0.6;
                    let lighting=ambient+ndotl*0.8;
                    let mut out=[0u8;3];
                    for k in 0..3 {
                        let base=diffuse[k] as f32/255.0;
                        let col=base*lighting*(1.0-metal*0.35)
                            +spec*(0.10*(1.0-metal)+base*metal)
                            +em[k] as f32/255.0;
                        out[k]=(255.0*col.clamp(0.0,1.0)).round() as u8;
                    }
                    out
                };
                let off=idx*4;
                self.rgba[off..off+4].copy_from_slice(&[rgb[0],rgb[1],rgb[2],255]);
                self.uv[idx]=Some(uv.to_array());
            }}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn resizing_preserves_camera_and_pick_buffer_dimensions() {
        let mut view=Viewport::new(200,200);
        view.orbit(10.0,-5.0);
        let yaw=view.yaw;
        let pitch=view.pitch;
        view.resize(480,320);
        assert_eq!((view.width,view.height),(480,320));
        assert_eq!(view.yaw,yaw);
        assert_eq!(view.pitch,pitch);
        assert!(view.pick(481,200).is_none());
        assert_eq!(view.rgba.len(),480*320*4);
    }
    #[test]
    fn rendered_cube_has_hit_pixels() {
        let doc=Document::new(64);
        let mat=Material::from_document(&doc);
        let mut view=Viewport::new(200,200);
        view.render(&doc.mesh,&mat,None);
        assert!(view.uv.iter().any(|uv|uv.is_some()));
    }
}
