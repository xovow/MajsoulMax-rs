use egui::epaint::{
    ClippedPrimitive, Color32, ImageData, Primitive, TextureId, Vertex, textures::TexturesDelta,
};
use std::collections::{HashMap, hash_map::Entry};

struct Texture {
    width: usize,
    height: usize,
    pixels: Vec<[u8; 4]>,
}

pub struct SoftwareRenderer {
    textures: HashMap<TextureId, Texture>,
    pixels: Vec<u32>,
    width: usize,
    height: usize,
}

impl SoftwareRenderer {
    pub fn new() -> Self {
        Self {
            textures: HashMap::new(),
            pixels: Vec::new(),
            width: 0,
            height: 0,
        }
    }

    pub fn resize(&mut self, width: usize, height: usize) {
        if self.width == width && self.height == height && self.pixels.len() == width * height {
            return;
        }
        self.width = width;
        self.height = height;
        let len = width.saturating_mul(height);
        self.pixels.resize(len, pack_bgra(255, 255, 255, 255));
        if self.pixels.capacity() > len.saturating_mul(2) {
            self.pixels.shrink_to(len);
        }
    }

    pub fn apply_textures(&mut self, delta: TexturesDelta) {
        for (id, image_deltas) in &delta.set {
            for image_delta in image_deltas {
                apply_image_delta(&mut self.textures, *id, &image_delta.image, image_delta.pos);
            }
        }
        for id in &delta.free {
            self.textures.remove(id);
        }
    }

    pub fn render(&mut self, primitives: &[ClippedPrimitive], pixels_per_point: f32) {
        self.pixels.fill(pack_bgra(255, 255, 255, 255));
        if self.width == 0 || self.height == 0 {
            return;
        }
        let ppp = pixels_per_point.max(0.5);
        for primitive in primitives {
            let clip = pixel_clip(primitive.clip_rect, ppp, self.width, self.height);
            let Some(clip) = clip else {
                continue;
            };
            match &primitive.primitive {
                Primitive::Mesh(mesh) => {
                    if !mesh.is_valid() {
                        continue;
                    }
                    let texture = self.textures.get(&mesh.texture_id);
                    for triangle in mesh.indices.chunks_exact(3) {
                        let v0 = mesh.vertices[triangle[0] as usize];
                        let v1 = mesh.vertices[triangle[1] as usize];
                        let v2 = mesh.vertices[triangle[2] as usize];
                        if v0.color.a() | v1.color.a() | v2.color.a() == 0 {
                            continue;
                        }
                        rasterize_triangle(
                            &mut self.pixels,
                            self.width,
                            self.height,
                            clip,
                            v0,
                            v1,
                            v2,
                            texture,
                            ppp,
                        );
                    }
                }
                Primitive::Callback(_) => {}
            }
        }
    }

    pub fn bgra(&self) -> &[u32] {
        &self.pixels
    }
}

fn apply_image_delta(
    textures: &mut HashMap<TextureId, Texture>,
    id: TextureId,
    image: &ImageData,
    pos: Option<[usize; 2]>,
) {
    let ImageData::Color(color) = image;
    let patch_w = color.width();
    let patch_h = color.height();
    if patch_w == 0 || patch_h == 0 {
        return;
    }
    let src = &color.pixels;
    if let Some([x0, y0]) = pos {
        let Some(texture) = textures.get_mut(&id) else {
            return;
        };
        for y in 0..patch_h {
            let dst_y = y0 + y;
            if dst_y >= texture.height {
                break;
            }
            for x in 0..patch_w {
                let dst_x = x0 + x;
                if dst_x >= texture.width {
                    break;
                }
                let pixel = src.get(y * patch_w + x).copied().unwrap_or(Color32::TRANSPARENT);
                texture.pixels[dst_y * texture.width + dst_x] = pixel.to_array();
            }
        }
        return;
    }
    match textures.entry(id) {
        Entry::Occupied(mut occupied) => {
            let texture = occupied.get_mut();
            texture.width = patch_w;
            texture.height = patch_h;
            texture.pixels.clear();
            texture.pixels.extend(src.iter().map(Color32::to_array));
        }
        Entry::Vacant(vacant) => {
            vacant.insert(Texture {
                width: patch_w,
                height: patch_h,
                pixels: src.iter().map(Color32::to_array).collect(),
            });
        }
    }
}

#[derive(Clone, Copy)]
struct Clip {
    min_x: i32,
    min_y: i32,
    max_x: i32,
    max_y: i32,
}

fn pixel_clip(rect: egui::Rect, ppp: f32, width: usize, height: usize) -> Option<Clip> {
    if !rect.is_finite() {
        return None;
    }
    let min_x = (rect.min.x * ppp).floor() as i32;
    let min_y = (rect.min.y * ppp).floor() as i32;
    let max_x = (rect.max.x * ppp).ceil() as i32;
    let max_y = (rect.max.y * ppp).ceil() as i32;
    let min_x = min_x.clamp(0, width as i32);
    let min_y = min_y.clamp(0, height as i32);
    let max_x = max_x.clamp(0, width as i32);
    let max_y = max_y.clamp(0, height as i32);
    (max_x > min_x && max_y > min_y).then_some(Clip {
        min_x,
        min_y,
        max_x,
        max_y,
    })
}

fn rasterize_triangle(
    pixels: &mut [u32],
    width: usize,
    height: usize,
    clip: Clip,
    v0: Vertex,
    v1: Vertex,
    v2: Vertex,
    texture: Option<&Texture>,
    ppp: f32,
) {
    let p0 = to_pixel(v0.pos, ppp);
    let p1 = to_pixel(v1.pos, ppp);
    let p2 = to_pixel(v2.pos, ppp);
    let min_x = p0.0.min(p1.0).min(p2.0).floor() as i32;
    let min_y = p0.1.min(p1.1).min(p2.1).floor() as i32;
    let max_x = p0.0.max(p1.0).max(p2.0).ceil() as i32;
    let max_y = p0.1.max(p1.1).max(p2.1).ceil() as i32;
    let min_x = min_x.max(clip.min_x).max(0);
    let min_y = min_y.max(clip.min_y).max(0);
    let max_x = max_x.min(clip.max_x).min(width as i32);
    let max_y = max_y.min(clip.max_y).min(height as i32);
    if max_x <= min_x || max_y <= min_y {
        return;
    }

    let area = edge(p0, p1, p2);
    if area.abs() < 0.25 {
        return;
    }

    for y in min_y..max_y {
        let py = y as f32 + 0.5;
        for x in min_x..max_x {
            let px = x as f32 + 0.5;
            let w0 = edge((px, py), p1, p2) / area;
            let w1 = edge((px, py), p2, p0) / area;
            let w2 = edge((px, py), p0, p1) / area;
            if w0 < 0.0 || w1 < 0.0 || w2 < 0.0 {
                continue;
            }
            let color = lerp_color(v0.color, v1.color, v2.color, w0, w1, w2);
            let uv = (
                v0.uv.x * w0 + v1.uv.x * w1 + v2.uv.x * w2,
                v0.uv.y * w0 + v1.uv.y * w1 + v2.uv.y * w2,
            );
            let texel = sample_texture(texture, uv);
            let src = mul_premul(color, texel);
            if src[3] == 0 {
                continue;
            }
            let index = y as usize * width + x as usize;
            if index < pixels.len() && (y as usize) < height {
                blend_premul(&mut pixels[index], src);
            }
        }
    }
}

fn to_pixel(pos: egui::Pos2, ppp: f32) -> (f32, f32) {
    (pos.x * ppp, pos.y * ppp)
}

fn edge(p: (f32, f32), a: (f32, f32), b: (f32, f32)) -> f32 {
    (p.0 - a.0) * (b.1 - a.1) - (p.1 - a.1) * (b.0 - a.0)
}

fn lerp_color(c0: Color32, c1: Color32, c2: Color32, w0: f32, w1: f32, w2: f32) -> [u8; 4] {
    let a0 = c0.to_array();
    let a1 = c1.to_array();
    let a2 = c2.to_array();
    [
        (a0[0] as f32 * w0 + a1[0] as f32 * w1 + a2[0] as f32 * w2).round() as u8,
        (a0[1] as f32 * w0 + a1[1] as f32 * w1 + a2[1] as f32 * w2).round() as u8,
        (a0[2] as f32 * w0 + a1[2] as f32 * w1 + a2[2] as f32 * w2).round() as u8,
        (a0[3] as f32 * w0 + a1[3] as f32 * w1 + a2[3] as f32 * w2).round() as u8,
    ]
}

fn sample_texture(texture: Option<&Texture>, uv: (f32, f32)) -> [u8; 4] {
    let Some(texture) = texture else {
        return [255, 255, 255, 255];
    };
    if texture.width == 0 || texture.height == 0 || texture.pixels.is_empty() {
        return [255, 255, 255, 255];
    }
    let x = uv.0.mul_add(texture.width as f32, -0.5);
    let y = uv.1.mul_add(texture.height as f32, -0.5);
    let x0 = x.floor() as i32;
    let y0 = y.floor() as i32;
    let fx = x - x0 as f32;
    let fy = y - y0 as f32;
    let c00 = texel(texture, x0, y0);
    let c10 = texel(texture, x0 + 1, y0);
    let c01 = texel(texture, x0, y0 + 1);
    let c11 = texel(texture, x0 + 1, y0 + 1);
    lerp_bytes(lerp_bytes(c00, c10, fx), lerp_bytes(c01, c11, fx), fy)
}

fn texel(texture: &Texture, x: i32, y: i32) -> [u8; 4] {
    let x = x.clamp(0, texture.width.saturating_sub(1) as i32) as usize;
    let y = y.clamp(0, texture.height.saturating_sub(1) as i32) as usize;
    texture.pixels[y * texture.width + x]
}

fn lerp_bytes(a: [u8; 4], b: [u8; 4], t: f32) -> [u8; 4] {
    let t = t.clamp(0.0, 1.0);
    [
        (a[0] as f32 + (b[0] as f32 - a[0] as f32) * t).round() as u8,
        (a[1] as f32 + (b[1] as f32 - a[1] as f32) * t).round() as u8,
        (a[2] as f32 + (b[2] as f32 - a[2] as f32) * t).round() as u8,
        (a[3] as f32 + (b[3] as f32 - a[3] as f32) * t).round() as u8,
    ]
}

fn mul_premul(color: [u8; 4], texel: [u8; 4]) -> [u8; 4] {
    [
        ((color[0] as u16 * texel[0] as u16) / 255) as u8,
        ((color[1] as u16 * texel[1] as u16) / 255) as u8,
        ((color[2] as u16 * texel[2] as u16) / 255) as u8,
        ((color[3] as u16 * texel[3] as u16) / 255) as u8,
    ]
}

fn blend_premul(dst: &mut u32, src: [u8; 4]) {
    if src[3] == 255 {
        *dst = pack_bgra(src[0], src[1], src[2], src[3]);
        return;
    }
    let [dr, dg, db, da] = unpack_bgra(*dst);
    let inv = 255 - src[3] as u32;
    let r = src[0] as u32 + (dr as u32 * inv + 127) / 255;
    let g = src[1] as u32 + (dg as u32 * inv + 127) / 255;
    let b = src[2] as u32 + (db as u32 * inv + 127) / 255;
    let a = src[3] as u32 + (da as u32 * inv + 127) / 255;
    *dst = pack_bgra(r as u8, g as u8, b as u8, a as u8);
}

fn pack_bgra(r: u8, g: u8, b: u8, a: u8) -> u32 {
    u32::from_le_bytes([b, g, r, a])
}

fn unpack_bgra(pixel: u32) -> [u8; 4] {
    let [b, g, r, a] = pixel.to_le_bytes();
    [r, g, b, a]
}
