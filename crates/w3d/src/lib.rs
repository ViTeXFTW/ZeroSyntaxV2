//! Small, renderer-agnostic reader for Command & Conquer W3D assets.
#![allow(clippy::manual_is_multiple_of)] // `is_multiple_of` is newer than the 1.75 MSRV.

mod parse;
mod render;

use std::fmt;

pub use render::RenderedThumbnail;

const MAX_FILE_BYTES: usize = 128 * 1024 * 1024;
const MAX_VERTICES: usize = 1_000_000;
const MAX_TRIANGLES: usize = 1_000_000;
const MAX_CHUNKS_PER_CONTAINER: usize = 1_000_000;
const MAX_CHUNK_DEPTH: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelCatalogEntry {
    pub name: String,
    pub members: Vec<String>,
    pub hierarchy: Option<String>,
}

/// Engine animation name (`Hierarchy.Animation`) and its target skeleton.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnimationCatalogEntry {
    pub name: String,
    pub hierarchy: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct W3dError(String);

impl W3dError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for W3dError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for W3dError {}

#[derive(Debug, Default)]
pub struct W3dFile {
    pub(crate) meshes: Vec<parse::Mesh>,
    pub(crate) hierarchies: Vec<parse::Hierarchy>,
    pub(crate) hlods: Vec<parse::Hlod>,
    extra_names: Vec<String>,
    extra_members: Vec<String>,
    pub(crate) animations: Vec<AnimationCatalogEntry>,
}

impl W3dFile {
    pub fn parse(bytes: &[u8]) -> Result<Self, W3dError> {
        parse::parse(bytes)
    }

    pub fn catalog(&self, fallback_name: &str) -> Vec<ModelCatalogEntry> {
        // Animation-only files are not renderable models.
        if !self.animations.is_empty()
            && self.meshes.is_empty()
            && self.hierarchies.is_empty()
            && self.hlods.is_empty()
            && self.extra_names.is_empty()
        {
            return Vec::new();
        }
        let mut names = Vec::new();
        let mut members = self.extra_members.clone();
        push_name(&mut names, fallback_name);
        names.extend(self.extra_names.iter().cloned());
        for mesh in &self.meshes {
            push_name(&mut names, &mesh.name);
            push_name(&mut members, &mesh.name);
        }
        for hierarchy in &self.hierarchies {
            push_name(&mut names, &hierarchy.name);
            for pivot in &hierarchy.pivots {
                push_name(&mut members, &pivot.name);
            }
        }
        for hlod in &self.hlods {
            push_name(&mut names, &hlod.name);
            for subobject in hlod
                .lods
                .iter()
                .flat_map(|lod| &lod.subobjects)
                .chain(&hlod.aggregates)
            {
                push_name(&mut members, &subobject.name);
            }
        }
        dedup(&mut names);
        dedup(&mut members);
        names
            .into_iter()
            .filter(|name| !name.is_empty())
            .map(|name| ModelCatalogEntry {
                hierarchy: self
                    .hlods
                    .iter()
                    .find(|hlod| hlod.name.eq_ignore_ascii_case(&name))
                    .or_else(|| {
                        (name.eq_ignore_ascii_case(fallback_name) && self.hlods.len() == 1)
                            .then(|| &self.hlods[0])
                    })
                    .map(|hlod| hlod.hierarchy.clone())
                    .or_else(|| {
                        self.hierarchies
                            .iter()
                            .find(|h| h.name.eq_ignore_ascii_case(&name))
                            .map(|h| h.name.clone())
                    })
                    .filter(|name| !name.is_empty()),
                name,
                members: members.clone(),
            })
            .collect()
    }

    pub fn animations(&self) -> &[AnimationCatalogEntry] {
        &self.animations
    }

    pub fn render_thumbnail(
        &self,
        model: &str,
        zoom: f32,
        load_texture: impl FnMut(&str) -> Option<Vec<u8>>,
    ) -> Result<RenderedThumbnail, W3dError> {
        render::thumbnail(self, model, zoom, load_texture)
    }
}

fn push_name(out: &mut Vec<String>, name: &str) {
    let name = name.trim();
    if name.is_empty() {
        return;
    }
    out.push(name.to_string());
    if let Some((_, short)) = name.rsplit_once('.') {
        if !short.is_empty() {
            out.push(short.to_string());
        }
    }
}

fn dedup(values: &mut Vec<String>) {
    let mut seen = std::collections::HashSet::new();
    values.retain(|value| seen.insert(value.to_ascii_lowercase()));
}

#[cfg(test)]
mod tests {
    use image::GenericImageView;

    use super::*;

    fn chunk(kind: u32, payload: Vec<u8>) -> Vec<u8> {
        let mut out = kind.to_le_bytes().to_vec();
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend(payload);
        out
    }

    fn floats(values: &[f32]) -> Vec<u8> {
        values
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .collect()
    }

    #[test]
    fn animation_headers_keep_qualified_names_and_stay_out_of_models() {
        for kind in [0x200, 0x280, 0x2c0] {
            let mut header = vec![0; if kind == 0x2c0 { 48 } else { 44 }];
            header[4..7].copy_from_slice(b"Run");
            header[20..28].copy_from_slice(b"HumanSKL");
            let file = W3dFile::parse(&chunk(kind, chunk(kind + 1, header))).unwrap();
            assert!(file.catalog("Run").is_empty());
            assert_eq!(
                file.animations(),
                &[AnimationCatalogEntry {
                    name: "HumanSKL.Run".into(),
                    hierarchy: "HumanSKL".into(),
                }]
            );
            assert!(W3dFile::parse(&chunk(kind, chunk(kind + 1, vec![0; 35]))).is_err());
        }
    }

    #[test]
    fn catalog_associates_each_hlod_with_its_own_skeleton() {
        fn hlod(name: &str, hierarchy: &str) -> Vec<u8> {
            let mut header = vec![0; 40];
            header[8..8 + name.len()].copy_from_slice(name.as_bytes());
            header[24..24 + hierarchy.len()].copy_from_slice(hierarchy.as_bytes());
            chunk(0x700, chunk(0x701, header))
        }
        let bytes = [hlod("Soldier", "HumanSKL"), hlod("Plane", "PlaneSKL")].concat();
        let file = W3dFile::parse(&bytes).unwrap();
        let catalog = file.catalog("Bundle");
        assert_eq!(
            catalog
                .iter()
                .find(|m| m.name == "Soldier")
                .unwrap()
                .hierarchy
                .as_deref(),
            Some("HumanSKL")
        );
        assert_eq!(
            catalog
                .iter()
                .find(|m| m.name == "Plane")
                .unwrap()
                .hierarchy
                .as_deref(),
            Some("PlaneSKL")
        );
        assert_eq!(
            catalog
                .iter()
                .find(|m| m.name == "Bundle")
                .unwrap()
                .hierarchy,
            None
        );
    }

    #[test]
    fn parses_and_renders_a_textured_w3d() {
        let mut header = vec![0; 116];
        header[8..16].copy_from_slice(b"Triangle");
        header[24..31].copy_from_slice(b"Preview");
        let mut triangle = Vec::new();
        triangle.extend([0u32, 1, 2].into_iter().flat_map(u32::to_le_bytes));
        triangle.extend_from_slice(&0u32.to_le_bytes());
        triangle.extend(floats(&[0.0, -1.0, 0.0, 0.0]));
        let texture = chunk(0x30, chunk(0x31, chunk(0x32, b"Preview.tga\0".to_vec())));
        let mut material_info = vec![0; 32];
        material_info[8..12].copy_from_slice(&[255, 255, 255, 0]);
        material_info[24..28].copy_from_slice(&1.0f32.to_le_bytes());
        let vertex_material = chunk(0x2a, chunk(0x2b, chunk(0x2d, material_info)));
        let material = chunk(0x38, chunk(0x48, chunk(0x49, 0u32.to_le_bytes().to_vec())));
        let mesh = chunk(
            0,
            [
                chunk(0x1f, header),
                chunk(
                    0x02,
                    floats(&[-1.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0]),
                ),
                chunk(
                    0x03,
                    floats(&[0.0, -1.0, 0.0, 0.0, -1.0, 0.0, 0.0, -1.0, 0.0]),
                ),
                chunk(0x0d, floats(&[0.0, 0.0, 1.0, 0.0, 0.5, 1.0])),
                chunk(0x20, triangle),
                vertex_material,
                texture,
                material,
            ]
            .concat(),
        );
        let mut tga = vec![0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2, 0, 2, 0, 32, 0x20];
        tga.extend_from_slice(&[
            0, 0, 255, 255, 0, 255, 0, 255, 255, 0, 0, 255, 255, 255, 255, 255,
        ]);

        let file = W3dFile::parse(&mesh).unwrap();
        assert_eq!(file.meshes[0].material_diffuse, [255, 255, 255, 255]);
        let rendered = file
            .render_thumbnail("Preview", 1.0, |name| {
                name.eq_ignore_ascii_case("Preview.tga")
                    .then(|| tga.clone())
            })
            .unwrap();
        assert!(rendered.missing_textures.is_empty());
        let image = image::load_from_memory(&rendered.png).unwrap();
        assert_eq!(image.dimensions(), (render::WIDTH, render::HEIGHT));
        assert!(
            image
                .to_rgb8()
                .pixels()
                .any(|pixel| pixel[0] != pixel[1] || pixel[1] != pixel[2]),
            "expected textured geometry over the grayscale checkerboard"
        );
    }
}
