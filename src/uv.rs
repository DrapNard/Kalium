//! Deterministic, bounded CPU UV coverage rasterizer.
//!
//! The result is a one-byte-per-texel mask. It is independent of the UI and
//! used to prevent brush stamps and flood fills from modifying unoccupied UV
//! atlas space. It is NOT an island ID map and does not make UV seams seamless.
use crate::mesh::Mesh;
use anyhow::{bail, Result};
use glam::Vec2;
use image::{GrayImage, Luma};

const MAX_RASTER_SAMPLES: u64 = 90_000_000;

#[derive(Clone)]
pub struct UvCoverage {
    pub size: u32,
    cells: Vec<u8>,
    covered: usize,
}

impl UvCoverage {
    pub fn generate(mesh: &Mesh, size: u32) -> Result<Self> {
        if !(64..=4096).contains(&size) { bail!("UV coverage dimension must be between 64 and 4096"); }
        mesh.validate()?;
        let mut cells = vec![0u8; (size as usize)*(size as usize)];
        let edge = |p: Vec2, q: Vec2, r: Vec2| -> f32 {
            (r.x-p.x)*(q.y-p.y) - (r.y-p.y)*(q.x-p.x)
        };
        let mut raster_samples = 0u64;
        for triangle in mesh.indices.chunks_exact(3) {
            let pixel_uv = |index: u32| Vec2::from_array(mesh.vertices[index as usize].uv)
                * ((size-1) as f32);
            let a=pixel_uv(triangle[0]);
            let b=pixel_uv(triangle[1]);
            let c=pixel_uv(triangle[2]);
            let area = edge(a,b,c);
            if !area.is_finite() || area.abs() < 1e-7 { continue; }
            let min_x = a.x.min(b.x).min(c.x).floor().max(0.0) as u32;
            let min_y = a.y.min(b.y).min(c.y).floor().max(0.0) as u32;
            let max_x = a.x.max(b.x).max(c.x).ceil().min((size-1) as f32) as u32;
            let max_y = a.y.max(b.y).max(c.y).ceil().min((size-1) as f32) as u32;
            if min_x>max_x || min_y>max_y { continue; }
            let count = (u64::from(max_x-min_x)+1)*(u64::from(max_y-min_y)+1);
            raster_samples = raster_samples.saturating_add(count);
            if raster_samples > MAX_RASTER_SAMPLES {
                bail!("UV coverage exceeds the CPU rasterization safety budget");
            }
            // Sample at actual texel centers used by Brush::stamp: integer
            // texel coordinates with UV = texel / (size-1).
            for y in min_y..=max_y {
                for x in min_x..=max_x {
                    let p = Vec2::new(x as f32,y as f32);
                    let u=edge(b,c,p)/area;
                    let v=edge(c,a,p)/area;
                    let w=1.0-u-v;
                    if u>=-1e-5 && v>=-1e-5 && w>=-1e-5 {
                        cells[(y as usize)*(size as usize)+x as usize]=255;
                    }
                }
            }
        }
        let covered=cells.iter().filter(|p|**p!=0).count();
        Ok(Self {size,cells,covered})
    }
    pub fn contains(&self,x:u32,y:u32)->bool {
        x<self.size && y<self.size && self.cells[(y as usize)*(self.size as usize)+x as usize]!=0
    }
    pub fn contains_uv(&self,uv:[f32;2])->bool {
        if uv.iter().any(|v|!v.is_finite() || !(0.0..=1.0).contains(v)) {return false;}
        let x=(uv[0]*((self.size-1) as f32)).round() as u32;
        let y=(uv[1]*((self.size-1) as f32)).round() as u32;
        self.contains(x,y)
    }
    pub fn to_image(&self)->GrayImage {
        GrayImage::from_raw(self.size,self.size,self.cells.clone())
            .unwrap_or_else(||GrayImage::from_pixel(self.size,self.size,Luma([0])))
    }
    pub fn covered_texels(&self)->usize {self.covered}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mesh::{Mesh,Vertex};
    fn small_triangle()->Mesh {
        Mesh {name:"Triangle".into(),vertices:vec![
            Vertex {position:[0.,0.,0.],normal:[0.,0.,1.],uv:[0.25,0.25]},
            Vertex {position:[1.,0.,0.],normal:[0.,0.,1.],uv:[0.75,0.25]},
            Vertex {position:[0.,1.,0.],normal:[0.,0.,1.],uv:[0.25,0.75]},
        ],indices:vec![0,1,2]}
    }
    #[test]
    fn coverage_excludes_empty_uv_space() {
        let mask=UvCoverage::generate(&small_triangle(),64).unwrap();
        assert!(mask.contains_uv([0.35,0.35]));
        assert!(!mask.contains_uv([0.85,0.85]));
        assert!(!mask.contains_uv([-0.1,0.1]));
        assert!(mask.covered_texels()>0);
    }
    #[test]
    fn reversed_winding_still_rasterizes() {
        let mut mesh=small_triangle();mesh.indices.swap(1,2);
        let mask=UvCoverage::generate(&mesh,64).unwrap();
        assert!(mask.contains_uv([0.35,0.35]));
    }
    #[test]
    fn invalid_size_is_rejected() {
        assert!(UvCoverage::generate(&small_triangle(),5000).is_err());
    }
}
