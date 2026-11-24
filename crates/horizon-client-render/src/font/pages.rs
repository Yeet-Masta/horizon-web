//! Vanilla's shader-visible font textures, independent of the native HUD atlas.

use super::*;
use std::sync::Mutex;

const SIZE: u32 = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PageKind {
    Color,
    Grayscale,
}

impl PageKind {
    fn pixel_size(self) -> u32 {
        match self {
            Self::Color => 4,
            Self::Grayscale => 1,
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Baked {
    pub glyph: Glyph,
    pub page: u32,
}

#[derive(Clone, Copy)]
struct Slot {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
}

#[derive(Clone, Copy)]
enum Node {
    Free(Slot),
    Used,
    Split(usize),
}

struct DynamicAtlasTree(Vec<Node>);

impl DynamicAtlasTree {
    fn new() -> Self {
        Self(vec![Node::Free(Slot {
            x: 0,
            y: 0,
            w: SIZE,
            h: SIZE,
        })])
    }

    /// `DynamicAtlasTree.insertInner`: visit the first child before the second; split
    /// vertically only when the unused width exceeds the unused height. The separator
    /// is outside the glyph, so a glyph can reach the texture's last column or row.
    fn insert_inner(&mut self, index: usize, w: u32, h: u32) -> Option<(u32, u32)> {
        match self.0[index] {
            Node::Used => None,
            Node::Split(first) => self
                .insert_inner(first, w, h)
                .or_else(|| self.insert_inner(first + 1, w, h)),
            Node::Free(slot) => {
                let dw = slot.w.checked_sub(w)?;
                let dh = slot.h.checked_sub(h)?;
                if dw == 0 && dh == 0 {
                    self.0[index] = Node::Used;
                    return Some((slot.x, slot.y));
                }
                let (first, second) = if dw > dh {
                    (
                        Slot { w, ..slot },
                        Slot {
                            x: slot.x + w + 1,
                            w: dw - 1,
                            ..slot
                        },
                    )
                } else {
                    (
                        Slot { h, ..slot },
                        Slot {
                            y: slot.y + h + 1,
                            h: dh - 1,
                            ..slot
                        },
                    )
                };
                let next = self.0.len();
                self.0.extend([Node::Free(first), Node::Free(second)]);
                self.0[index] = Node::Split(next);
                self.insert_inner(next, w, h)
            }
        }
    }
}

struct FontTexture {
    id: u32,
    kind: PageKind,
    tree: DynamicAtlasTree,
}

#[derive(Default)]
struct Textures {
    generation: u64,
    pages: Vec<Texture>,
    dirty: Vec<usize>,
}

struct Texture {
    kind: PageKind,
    pending: Vec<Patch>,
    gpu: Option<(wgpu::Texture, wgpu::TextureView)>,
}

/// One resource generation's pages, shared by GUI and world pipelines. Pixel buffers
/// exist only until the next GPU upload; an unrequested font never allocates a page.
#[derive(Clone, Default)]
pub(crate) struct FontPages(Arc<Mutex<Textures>>);

impl FontPages {
    fn create(&self, kind: PageKind) -> u32 {
        let mut textures = self.0.lock().unwrap();
        textures.pages.push(Texture {
            kind,
            pending: Vec::new(),
            gpu: None,
        });
        textures.pages.len() as u32
    }

    fn write(&self, id: u32, patch: Patch) {
        let mut textures = self.0.lock().unwrap();
        let index = id as usize - 1;
        if textures.pages[index].pending.is_empty() {
            textures.dirty.push(index);
        }
        textures.pages[index].pending.push(patch);
    }

    pub(crate) fn needs_upload(&self) -> bool {
        !self.0.lock().unwrap().dirty.is_empty()
    }

    pub(crate) fn generation(&self) -> u64 {
        self.0.lock().unwrap().generation
    }

    pub(crate) fn for_each_view(
        &self,
        skip: usize,
        mut f: impl FnMut(PageKind, &wgpu::TextureView),
    ) {
        let textures = self.0.lock().unwrap();
        for texture in textures.pages.iter().skip(skip) {
            let Some((_, view)) = &texture.gpu else { break };
            f(texture.kind, view);
        }
    }

    /// `GlyphStitcher.reset`: release this resource generation's font textures.
    pub(crate) fn reset(&self) {
        let mut textures = self.0.lock().unwrap();
        textures.pages.clear();
        textures.dirty.clear();
        textures.generation += 1;
    }

    /// `FontTexture.add`: the glyph uploads its original texels into a 256×256
    /// RGBA8 or R8 UNORM image, not an sRGB texture or a shared HUD sheet.
    pub(crate) fn upload(&self, device: &wgpu::Device, queue: &wgpu::Queue) {
        let mut textures = self.0.lock().unwrap();
        let Textures { pages, dirty, .. } = &mut *textures;
        for index in dirty.drain(..) {
            let page = &mut pages[index];
            let (texture, _) = page.gpu.get_or_insert_with(|| {
                let texture = device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("Resource pack font page"),
                    size: wgpu::Extent3d {
                        width: SIZE,
                        height: SIZE,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: match page.kind {
                        PageKind::Color => wgpu::TextureFormat::Rgba8Unorm,
                        PageKind::Grayscale => wgpu::TextureFormat::R8Unorm,
                    },
                    usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                    view_formats: &[],
                });
                let view = texture.create_view(&Default::default());
                (texture, view)
            });
            for patch in page.pending.drain(..) {
                queue.write_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture,
                        origin: wgpu::Origin3d {
                            x: patch.x,
                            y: patch.y,
                            z: 0,
                        },
                        mip_level: 0,
                        aspect: wgpu::TextureAspect::All,
                    },
                    &patch.rgba,
                    wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(patch.width * page.kind.pixel_size()),
                        rows_per_image: None,
                    },
                    wgpu::Extent3d {
                        width: patch.width,
                        height: patch.height,
                        depth_or_array_layers: 1,
                    },
                );
            }
        }
    }
}

pub(crate) fn nearest_repeat(device: &wgpu::Device) -> wgpu::Sampler {
    device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("FontTexture nearest repeat"),
        address_mode_u: wgpu::AddressMode::Repeat,
        address_mode_v: wgpu::AddressMode::Repeat,
        ..Default::default()
    })
}

struct Stitcher {
    pages: Vec<FontTexture>,
    textures: FontPages,
}

impl Stitcher {
    /// `GlyphStitcher.stitch`: try each page in creation order, including older pages
    /// after a new one was needed. A page accepts only glyphs with its pixel format.
    fn stitch(
        &mut self,
        glyph: Glyph,
        kind: PageKind,
        [w, h]: [u32; 2],
        pixels: Vec<u8>,
    ) -> Option<Baked> {
        if w == 0 || h == 0 || w > SIZE || h > SIZE {
            return None;
        }
        let (page, (x, y)) = self
            .pages
            .iter_mut()
            .filter(|p| p.kind == kind)
            .find_map(|p| Some((p.id, p.tree.insert_inner(0, w, h)?)))
            .unwrap_or_else(|| {
                let mut tree = DynamicAtlasTree::new();
                let origin = tree.insert_inner(0, w, h).unwrap();
                let id = self.textures.create(kind);
                self.pages.push(FontTexture { id, tree, kind });
                (id, origin)
            });
        self.textures.write(
            page,
            Patch {
                x,
                y,
                width: w,
                height: h,
                rgba: pixels,
            },
        );
        let rect = Rect {
            u0: (x as f32 + 0.01) / SIZE as f32,
            v0: (y as f32 + 0.01) / SIZE as f32,
            u1: (x as f32 - 0.01 + w as f32) / SIZE as f32,
            v1: (y as f32 - 0.01 + h as f32) / SIZE as f32,
            w,
            h,
        };
        Some(Baked {
            glyph: Glyph {
                rect: Some(rect),
                ..glyph
            },
            page,
        })
    }
}

/// `SpecialGlyphs.generate`: RGBA white outline and a transparent interior.
pub(super) fn missing_pixels() -> Vec<u8> {
    let mut border = vec![255; 5 * 8 * 4];
    for y in 1..7 {
        border[(y * 5 + 1) * 4..(y * 5 + 4) * 4].fill(0);
    }
    border
}

pub(super) struct PageSet {
    stitcher: Stitcher,
    providers: Vec<Provider>,
    glyphs: crate::fast_hash::FxHashMap<char, Baked>,
    non_fishy: crate::fast_hash::FxHashMap<char, Baked>,
    pub missing: Baked,
    pub white: Baked,
}

impl PageSet {
    /// `FontSet.resetTextures`: MISSING then WHITE, in the first color texture.
    pub(super) fn new(textures: FontPages, providers: Vec<Provider>, missing: Glyph) -> Self {
        let mut stitcher = Stitcher {
            pages: Vec::new(),
            textures,
        };
        let missing = stitcher
            .stitch(missing, PageKind::Color, [5, 8], missing_pixels())
            .unwrap();
        let white = stitcher
            .stitch(
                Glyph::default(),
                PageKind::Color,
                [5, 8],
                vec![255; 5 * 8 * 4],
            )
            .unwrap();
        Self {
            stitcher,
            providers,
            glyphs: Default::default(),
            non_fishy: Default::default(),
            missing,
            white,
        }
    }

    /// Metrics-only lookup matching [`Self::request`] without stitching or caching a glyph.
    pub(super) fn metric(&mut self, font: &Font, cache: &mut Cache, ch: char) -> Glyph {
        if let Some(&baked) = self.glyphs.get(&ch) {
            return baked.glyph;
        }
        for provider in &mut self.providers {
            match provider {
                Provider::Space(advances) => {
                    if let Some(&advance) = advances.get(&ch) {
                        return Glyph {
                            advance,
                            offset: 1.0,
                            ..Default::default()
                        };
                    }
                }
                Provider::Bitmap(filter, glyphs) if filter.passes(font.uniform, font.japanese) => {
                    match glyphs.get(&ch) {
                        Some(BitmapGlyph::Sheet(cell)) => {
                            if cell.w > SIZE || cell.h > SIZE {
                                return self.missing.glyph;
                            }
                            return cell.glyph;
                        }
                        Some(BitmapGlyph::Atlas(empty)) => return *empty,
                        None => continue,
                    }
                }
                Provider::Unihex(filter, lazy, loaded)
                    if filter.passes(font.uniform, font.japanese) =>
                {
                    let table = ensure_unihex(&font.source, lazy, loaded, &mut cache.unihex);
                    if let Some(advance) = table.advance(u32::from(ch)) {
                        return Glyph {
                            advance,
                            offset: 0.5,
                            ..Default::default()
                        };
                    }
                }
                Provider::TrueType(filter, ttf) if filter.passes(font.uniform, font.japanese) => {
                    let Some(id) = ttf.glyph_id(ch) else { continue };
                    if !ttf.fits_atlas(id, (SIZE, SIZE)) {
                        return self.missing.glyph;
                    }
                    return Glyph {
                        advance: ttf.advance(id),
                        offset: 1.0,
                        ..Default::default()
                    };
                }
                _ => continue,
            }
        }
        self.missing.glyph
    }

    /// `FontSet.DelayedBake.get`: choose the provider before uploading, and bake only
    /// the selected glyph on first request (not when enumerating a provider's sheet).
    pub(super) fn request(
        &mut self,
        font: &Font,
        cache: &mut Cache,
        ch: char,
        non_fishy: bool,
    ) -> Baked {
        let cached = if non_fishy {
            &self.non_fishy
        } else {
            &self.glyphs
        };
        if let Some(&glyph) = cached.get(&ch) {
            return glyph;
        }
        let mut selected = self.missing;
        let mut same_selection = true;
        for provider in &mut self.providers {
            let mut bitmap = None;
            let mut kind = PageKind::Color;
            let glyph = match provider {
                Provider::Space(advances) => {
                    let Some(&advance) = advances.get(&ch) else {
                        continue;
                    };
                    Glyph {
                        advance,
                        offset: 1.0,
                        ..Default::default()
                    }
                }
                Provider::Bitmap(filter, glyphs) if filter.passes(font.uniform, font.japanese) => {
                    match glyphs.get(&ch) {
                        Some(BitmapGlyph::Sheet(cell)) => {
                            if non_fishy && page_fishy(cell.glyph) {
                                same_selection = false;
                                continue;
                            }
                            if cell.w > SIZE || cell.h > SIZE {
                                same_selection &= !page_fishy(cell.glyph);
                                break;
                            }
                            let Some(rgba) = cell.pixels(&mut cache.strips) else {
                                continue;
                            };
                            bitmap = Some(([cell.w, cell.h], rgba));
                            cell.glyph
                        }
                        Some(BitmapGlyph::Atlas(empty)) => *empty,
                        None => continue,
                    }
                }
                Provider::Unihex(filter, lazy, loaded)
                    if filter.passes(font.uniform, font.japanese) =>
                {
                    let table = ensure_unihex(&font.source, lazy, loaded, &mut cache.unihex);
                    let Some(hex) = table.get(u32::from(ch)) else {
                        continue;
                    };
                    let width = hex.right.saturating_sub(hex.left) + 1;
                    let glyph = hex_glyph(&hex, width);
                    if non_fishy && page_fishy(glyph) {
                        same_selection = false;
                        continue;
                    }
                    bitmap = Some(([width, 16], hex_pixels(&hex, width)));
                    glyph
                }
                Provider::TrueType(filter, ttf) if filter.passes(font.uniform, font.japanese) => {
                    let Some(id) = ttf.glyph_id(ch) else { continue };
                    let metrics = Glyph {
                        advance: ttf.advance(id),
                        offset: 1.0,
                        ..Default::default()
                    };
                    if non_fishy && page_fishy(metrics) {
                        same_selection = false;
                        continue;
                    }
                    let Some((glyph, coverage)) = ttf.glyph(id, (SIZE, SIZE), true) else {
                        same_selection &= !page_fishy(metrics);
                        break;
                    };
                    bitmap = coverage.map(|(w, h, pixels)| ([w, h], pixels));
                    kind = PageKind::Grayscale;
                    glyph
                }
                _ => continue,
            };
            if page_fishy(glyph) {
                same_selection = false;
                if non_fishy {
                    continue;
                }
            }
            selected = match bitmap {
                Some((size, pixels)) => self
                    .stitcher
                    .stitch(glyph, kind, size, pixels)
                    .unwrap_or(self.missing),
                None => Baked { glyph, page: 0 },
            };
            break;
        }
        if non_fishy || same_selection {
            self.non_fishy.insert(ch, selected);
        }
        if !non_fishy || same_selection {
            self.glyphs.insert(ch, selected);
        }
        selected
    }
}

fn page_fishy(glyph: Glyph) -> bool {
    fishy_advance(glyph.advance) || fishy_advance(glyph.advance + glyph.offset)
}

#[cfg(test)]
pub(crate) fn marker_font_atlas(shader: bool) -> (tempfile::TempDir, crate::hud::HudAtlas) {
    use std::fs;
    let dir = tempfile::tempdir().unwrap();
    let assets = dir.path().join("assets");
    for (name, rgba) in [("one", [255, 0, 0, 255]), ("two", [0, 255, 0, 255])] {
        let font = assets.join(format!("fixture/font/{name}.json"));
        let image = assets.join(format!("fixture/textures/font/{name}.png"));
        fs::create_dir_all(font.parent().unwrap()).unwrap();
        fs::create_dir_all(image.parent().unwrap()).unwrap();
        fs::write(
            font,
            format!(r#"{{"providers":[{{"type":"bitmap","file":"fixture:font/{name}.png","height":2,"ascent":2,"chars":["A"]}}]}}"#),
        ).unwrap();
        let mut encoder = png::Encoder::new(fs::File::create(image).unwrap(), 2, 2);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder
            .write_header()
            .unwrap()
            .write_image_data(&rgba.repeat(4))
            .unwrap();
    }
    fs::write(
        assets.join("fixture/font/gray.json"),
        r#"{"providers":[{"type":"ttf","file":"fixture:gray.ttf","size":11,"oversample":2}]}"#,
    )
    .unwrap();
    fs::write(
        assets.join("fixture/font/gray.ttf"),
        horizon_theme::fonts::GEIST_MONO,
    )
    .unwrap();
    if shader {
        let shaders = assets.join("minecraft/shaders/core");
        fs::create_dir_all(&shaders).unwrap();
        fs::write(
            shaders.join("text.vsh"),
            r#"#version 330
in vec3 Position;
in vec2 UV0;
out vec2 uv;
void main() {
#ifdef IS_GUI
#ifdef IS_GRAYSCALE
    int corner = gl_VertexID % 4;
    float x = corner >= 2 ? 1.0 : -1.0;
    float y = corner == 1 || corner == 2 ? -1.0 : 1.0;
    gl_Position = vec4(x, y, 0.5, 1.0);
#else
    gl_Position = vec4(Position.x * 0.5 - 1.0, 1.0 - Position.y, 0.5, 1.0);
#endif
#else
    gl_Position = vec4(Position, 1.0);
#endif
    uv = UV0;
}
"#,
        )
        .unwrap();
        fs::write(
            shaders.join("text.fsh"),
            r#"#version 330
uniform sampler2D Sampler0;
in vec2 uv;
out vec4 fragColor;
void main() {
    ivec2 size = textureSize(Sampler0, 0);
#ifdef IS_GRAYSCALE
    vec4 gray = texture(Sampler0, uv);
    if (any(notEqual(size, ivec2(256))) || gray.g != 0.0 || gray.b != 0.0 || gray.a != 1.0)
        fragColor = vec4(1.0, 0.0, 1.0, 1.0);
    else
        fragColor = vec4(0.0, gray.r, 0.0, 1.0);
#else
    ivec2 cell = ivec2(floor(uv * vec2(size)));
    if (any(notEqual(size, ivec2(256))) ||
        any(lessThan(cell, ivec2(0, 18))) || any(greaterThan(cell, ivec2(1, 19)))) {
        fragColor = vec4(1.0, 0.0, 1.0, 1.0);
    } else {
        vec4 marker = texelFetch(Sampler0, ivec2(0, 18), 0);
        fragColor = vec4(marker.brg, 1.0);
    }
#endif
}
"#,
        )
        .unwrap();
    }
    let source = ResourceSource::directory(dir.path().to_owned());
    let atlas = crate::hud::HudAtlas::build_over(&source, &ResourceSource::default()).unwrap();
    (dir, atlas)
}

#[cfg(test)]
pub(crate) fn marker_text(name: &str) -> Text {
    Text::styled(
        "A",
        Style {
            font: Some(format!("fixture:{name}")),
            ..Default::default()
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vanilla_font_tree_uses_first_fit_and_one_texel_separators() {
        let mut set = PageSet::new(FontPages::default(), Vec::new(), Glyph::default());
        let origin = |baked: Baked| {
            let uv = baked.glyph.rect.unwrap();
            (
                (uv.u0 * 256.0).floor() as u32,
                (uv.v0 * 256.0).floor() as u32,
            )
        };
        assert_eq!((origin(set.missing), origin(set.white)), ((0, 0), (0, 9)));
        // A wide strip uses the 250 columns beside the reserved glyphs.
        // A full page then forces a new color page. Small glyphs return to page one.
        let mut place = |kind: PageKind, w, h| {
            let b = set
                .stitcher
                .stitch(
                    Glyph::default(),
                    kind,
                    [w, h],
                    vec![0; (w * h * kind.pixel_size()) as usize],
                )
                .unwrap();
            (b.page, origin(b))
        };
        assert_eq!(place(PageKind::Color, 250, 20), (1, (6, 0)));
        assert_eq!(place(PageKind::Color, 256, 256), (2, (0, 0)));
        assert_eq!(place(PageKind::Grayscale, 256, 255), (3, (0, 0)));
        assert_eq!(place(PageKind::Color, 3, 8), (1, (0, 18)));
        assert_eq!(place(PageKind::Color, 3, 8), (1, (0, 27)));
        assert_eq!(place(PageKind::Grayscale, 1, 1), (4, (0, 0)));
    }
}
