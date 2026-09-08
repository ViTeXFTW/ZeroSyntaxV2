use glam::{Quat, Vec2, Vec3};

use crate::{
    push_name, W3dError, W3dFile, MAX_CHUNKS_PER_CONTAINER, MAX_CHUNK_DEPTH, MAX_FILE_BYTES,
    MAX_TRIANGLES, MAX_VERTICES,
};

const MESH: u32 = 0x0000;
const VERTICES: u32 = 0x0002;
const NORMALS: u32 = 0x0003;
const TEXCOORDS: u32 = 0x000d;
const VERTEX_INFLUENCES: u32 = 0x000e;
const MESH_HEADER3: u32 = 0x001f;
const TRIANGLES: u32 = 0x0020;
const MATERIAL_INFO: u32 = 0x0028;
const VERTEX_MATERIALS: u32 = 0x002a;
const VERTEX_MATERIAL: u32 = 0x002b;
const VERTEX_MATERIAL_INFO: u32 = 0x002d;
const TEXTURES: u32 = 0x0030;
const TEXTURE: u32 = 0x0031;
const TEXTURE_NAME: u32 = 0x0032;
const MATERIAL_PASS: u32 = 0x0038;
const DCG: u32 = 0x003b;
const TEXTURE_STAGE: u32 = 0x0048;
const TEXTURE_IDS: u32 = 0x0049;
const STAGE_TEXCOORDS: u32 = 0x004a;
const PER_FACE_TEXCOORD_IDS: u32 = 0x004b;
const HIERARCHY: u32 = 0x0100;
const HIERARCHY_HEADER: u32 = 0x0101;
const PIVOTS: u32 = 0x0102;
const VERTEX_COLORS: u32 = 0x0115;
const HLOD: u32 = 0x0700;
const HLOD_HEADER: u32 = 0x0701;
const HLOD_LOD_ARRAY: u32 = 0x0702;
const HLOD_ARRAY_HEADER: u32 = 0x0703;
const HLOD_SUB_OBJECT: u32 = 0x0704;
const HLOD_AGGREGATES: u32 = 0x0705;

#[derive(Debug, Default)]
pub(crate) struct Mesh {
    pub name: String,
    pub container: String,
    pub attributes: u32,
    pub vertices: Vec<Vec3>,
    pub normals: Vec<Vec3>,
    pub uvs: Vec<Vec2>,
    pub triangles: Vec<Triangle>,
    pub influences: Vec<u16>,
    pub colors: Vec<[u8; 4]>,
    pub material_diffuse: [u8; 4],
    pub textures: Vec<String>,
    pub pass: MaterialPass,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Triangle {
    pub indices: [u32; 3],
}

#[derive(Debug, Default)]
pub(crate) struct MaterialPass {
    pub colors: Vec<[u8; 4]>,
    pub texture_ids: Vec<u32>,
    pub uvs: Vec<Vec2>,
    pub per_face_uv_ids: Vec<u32>,
}

#[derive(Debug, Default)]
pub(crate) struct Hierarchy {
    pub name: String,
    pub pivots: Vec<Pivot>,
}

#[derive(Debug)]
pub(crate) struct Pivot {
    pub name: String,
    pub parent: Option<usize>,
    pub translation: Vec3,
    pub rotation: Quat,
}

#[derive(Debug, Default)]
pub(crate) struct Hlod {
    pub name: String,
    pub hierarchy: String,
    pub lods: Vec<Lod>,
    pub aggregates: Vec<SubObject>,
}

#[derive(Debug, Default)]
pub(crate) struct Lod {
    pub max_screen_size: f32,
    pub subobjects: Vec<SubObject>,
}

#[derive(Debug)]
pub(crate) struct SubObject {
    pub bone: usize,
    pub name: String,
}

struct Chunk<'a> {
    kind: u32,
    data: &'a [u8],
}

pub(crate) fn parse(bytes: &[u8]) -> Result<W3dFile, W3dError> {
    if bytes.len() > MAX_FILE_BYTES {
        return Err(W3dError::new("W3D file exceeds 128 MiB"));
    }
    let mut file = W3dFile::default();
    for chunk in chunks(bytes, 0)? {
        match chunk.kind {
            MESH => file.meshes.push(parse_mesh(chunk.data)?),
            HIERARCHY => file.hierarchies.push(parse_hierarchy(chunk.data)?),
            HLOD => file.hlods.push(parse_hlod(chunk.data)?),
            // Raw, compressed (time-coded/adaptive delta), and morph headers
            // share Version, Name[16], HierarchyName[16]. See w3d_file.h.
            0x0200 | 0x0280 | 0x02c0 => {
                for header in chunks(chunk.data, 1)? {
                    if header.kind == chunk.kind + 1 {
                        if header.data.len() < 44 {
                            return Err(W3dError::new("truncated animation header"));
                        }
                        let name = fixed_name(&header.data[4..20]);
                        let hierarchy = fixed_name(&header.data[20..36]);
                        if !name.is_empty() && !hierarchy.is_empty() {
                            file.animations.push(crate::AnimationCatalogEntry {
                                name: format!("{hierarchy}.{name}"),
                                hierarchy,
                            });
                        }
                    }
                }
            }
            _ => {}
        }
        collect_catalog_names(
            chunk.kind,
            chunk.data,
            &mut file.extra_names,
            &mut file.extra_members,
            0,
        )?;
    }
    let vertices: usize = file.meshes.iter().map(|mesh| mesh.vertices.len()).sum();
    let triangles: usize = file.meshes.iter().map(|mesh| mesh.triangles.len()).sum();
    if vertices > MAX_VERTICES || triangles > MAX_TRIANGLES {
        return Err(W3dError::new("W3D geometry budget exceeded"));
    }
    Ok(file)
}

fn parse_mesh(bytes: &[u8]) -> Result<Mesh, W3dError> {
    let mut mesh = Mesh {
        material_diffuse: [204, 204, 204, 255],
        ..Mesh::default()
    };
    for chunk in chunks(bytes, 1)? {
        match chunk.kind {
            MESH_HEADER3 if chunk.data.len() >= 40 => {
                mesh.attributes = u32_at(chunk.data, 4)?;
                mesh.name = fixed_name(&chunk.data[8..24]);
                mesh.container = fixed_name(&chunk.data[24..40]);
            }
            VERTICES => mesh.vertices = vec3s(chunk.data)?,
            NORMALS => mesh.normals = vec3s(chunk.data)?,
            TEXCOORDS => mesh.uvs = vec2s(chunk.data, true)?,
            TRIANGLES => {
                if chunk.data.len() % 32 != 0 {
                    return Err(W3dError::new("invalid triangle chunk size"));
                }
                mesh.triangles = chunk
                    .data
                    .chunks(32)
                    .map(|bytes| {
                        Ok(Triangle {
                            indices: [u32_at(bytes, 0)?, u32_at(bytes, 4)?, u32_at(bytes, 8)?],
                        })
                    })
                    .collect::<Result<_, W3dError>>()?;
            }
            VERTEX_INFLUENCES => {
                if chunk.data.len() % 8 != 0 {
                    return Err(W3dError::new("invalid vertex influence chunk size"));
                }
                mesh.influences = chunk
                    .data
                    .chunks(8)
                    .map(|value| u16::from_le_bytes([value[0], value[1]]))
                    .collect();
            }
            VERTEX_COLORS => mesh.colors = rgba(chunk.data)?,
            VERTEX_MATERIALS => parse_vertex_materials(chunk.data, &mut mesh)?,
            TEXTURES => mesh.textures = parse_textures(chunk.data)?,
            MATERIAL_PASS if mesh.pass.texture_ids.is_empty() => {
                mesh.pass = parse_material_pass(chunk.data)?;
            }
            MATERIAL_INFO => {}
            _ => {}
        }
    }
    Ok(mesh)
}

fn parse_vertex_materials(bytes: &[u8], mesh: &mut Mesh) -> Result<(), W3dError> {
    for material in chunks(bytes, 2)? {
        if material.kind != VERTEX_MATERIAL {
            continue;
        }
        for chunk in chunks(material.data, 3)? {
            if chunk.kind == VERTEX_MATERIAL_INFO && chunk.data.len() >= 11 {
                mesh.material_diffuse[..3].copy_from_slice(&chunk.data[8..11]);
                if chunk.data.len() >= 28 {
                    let opacity = f32_at(chunk.data, 24)?.clamp(0.0, 1.0);
                    mesh.material_diffuse[3] = (opacity * 255.0).round() as u8;
                }
                return Ok(());
            }
        }
    }
    Ok(())
}

fn parse_textures(bytes: &[u8]) -> Result<Vec<String>, W3dError> {
    let mut textures = Vec::new();
    for texture in chunks(bytes, 2)? {
        if texture.kind != TEXTURE {
            continue;
        }
        let mut name = String::new();
        for chunk in chunks(texture.data, 3)? {
            if chunk.kind == TEXTURE_NAME {
                name = fixed_name(chunk.data);
            }
        }
        textures.push(name);
    }
    Ok(textures)
}

fn parse_material_pass(bytes: &[u8]) -> Result<MaterialPass, W3dError> {
    let mut pass = MaterialPass::default();
    for chunk in chunks(bytes, 2)? {
        match chunk.kind {
            DCG => pass.colors = rgba(chunk.data)?,
            TEXTURE_STAGE if pass.texture_ids.is_empty() => {
                for stage in chunks(chunk.data, 3)? {
                    match stage.kind {
                        TEXTURE_IDS => pass.texture_ids = u32s(stage.data)?,
                        STAGE_TEXCOORDS => pass.uvs = vec2s(stage.data, true)?,
                        PER_FACE_TEXCOORD_IDS => pass.per_face_uv_ids = u32s(stage.data)?,
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    Ok(pass)
}

fn parse_hierarchy(bytes: &[u8]) -> Result<Hierarchy, W3dError> {
    let mut hierarchy = Hierarchy::default();
    for chunk in chunks(bytes, 1)? {
        match chunk.kind {
            HIERARCHY_HEADER if chunk.data.len() >= 20 => {
                hierarchy.name = fixed_name(&chunk.data[4..20]);
            }
            PIVOTS => {
                if chunk.data.len() % 60 != 0 {
                    return Err(W3dError::new("invalid pivot chunk size"));
                }
                for pivot in chunk.data.chunks(60) {
                    let parent = u32_at(pivot, 16)?;
                    let rotation = Quat::from_xyzw(
                        f32_at(pivot, 44)?,
                        f32_at(pivot, 48)?,
                        f32_at(pivot, 52)?,
                        f32_at(pivot, 56)?,
                    );
                    hierarchy.pivots.push(Pivot {
                        name: fixed_name(&pivot[..16]),
                        parent: (parent != u32::MAX).then_some(parent as usize),
                        translation: vec3_at(pivot, 20)?,
                        rotation: if rotation.is_finite() && rotation.length_squared() > 0.0 {
                            rotation.normalize()
                        } else {
                            Quat::IDENTITY
                        },
                    });
                }
            }
            _ => {}
        }
    }
    Ok(hierarchy)
}

fn parse_hlod(bytes: &[u8]) -> Result<Hlod, W3dError> {
    let mut hlod = Hlod::default();
    for chunk in chunks(bytes, 1)? {
        match chunk.kind {
            HLOD_HEADER if chunk.data.len() >= 40 => {
                hlod.name = fixed_name(&chunk.data[8..24]);
                hlod.hierarchy = fixed_name(&chunk.data[24..40]);
            }
            HLOD_LOD_ARRAY => hlod.lods.push(parse_lod(chunk.data)?),
            HLOD_AGGREGATES => hlod.aggregates = parse_subobjects(chunk.data, 2)?,
            _ => {}
        }
    }
    Ok(hlod)
}

fn parse_lod(bytes: &[u8]) -> Result<Lod, W3dError> {
    let mut lod = Lod::default();
    for chunk in chunks(bytes, 2)? {
        match chunk.kind {
            HLOD_ARRAY_HEADER if chunk.data.len() >= 8 => {
                lod.max_screen_size = f32_at(chunk.data, 4)?;
            }
            HLOD_SUB_OBJECT => lod.subobjects.push(parse_subobject(chunk.data)?),
            _ => {}
        }
    }
    Ok(lod)
}

fn parse_subobjects(bytes: &[u8], depth: usize) -> Result<Vec<SubObject>, W3dError> {
    chunks(bytes, depth)?
        .into_iter()
        .filter(|chunk| chunk.kind == HLOD_SUB_OBJECT)
        .map(|chunk| parse_subobject(chunk.data))
        .collect()
}

fn parse_subobject(bytes: &[u8]) -> Result<SubObject, W3dError> {
    if bytes.len() < 36 {
        return Err(W3dError::new("invalid HLOD subobject"));
    }
    Ok(SubObject {
        bone: u32_at(bytes, 0)? as usize,
        name: fixed_name(&bytes[4..36]),
    })
}

fn collect_catalog_names(
    kind: u32,
    bytes: &[u8],
    names: &mut Vec<String>,
    members: &mut Vec<String>,
    depth: usize,
) -> Result<(), W3dError> {
    match kind {
        MESH_HEADER3 if bytes.len() >= 40 => {
            push_name(members, &fixed_name(&bytes[8..24]));
            push_name(names, &fixed_name(&bytes[24..40]));
        }
        HIERARCHY_HEADER | 0x0501 | 0x0601 if bytes.len() >= 20 => {
            push_name(names, &fixed_name(&bytes[4..20]));
        }
        PIVOTS => {
            for pivot in bytes.chunks(60).filter(|pivot| pivot.len() == 60) {
                push_name(members, &fixed_name(&pivot[..16]));
            }
        }
        HLOD_HEADER if bytes.len() >= 40 => {
            push_name(names, &fixed_name(&bytes[8..24]));
            push_name(names, &fixed_name(&bytes[24..40]));
        }
        HLOD_SUB_OBJECT if bytes.len() >= 36 => {
            push_name(members, &fixed_name(&bytes[4..36]));
        }
        0x0740 if bytes.len() >= 40 => push_name(members, &fixed_name(&bytes[8..40])),
        0x0750 if bytes.len() >= 48 => push_name(members, &fixed_name(&bytes[16..48])),
        _ => {}
    }
    if depth >= MAX_CHUNK_DEPTH || !is_container(kind) {
        return Ok(());
    }
    for child in chunks(bytes, depth + 1)? {
        collect_catalog_names(child.kind, child.data, names, members, depth + 1)?;
    }
    Ok(())
}

fn chunks(bytes: &[u8], depth: usize) -> Result<Vec<Chunk<'_>>, W3dError> {
    if depth > MAX_CHUNK_DEPTH {
        return Err(W3dError::new("W3D chunk nesting exceeds 16"));
    }
    let mut out = Vec::new();
    let mut position = 0usize;
    while position + 8 <= bytes.len() {
        if out.len() == MAX_CHUNKS_PER_CONTAINER {
            return Err(W3dError::new("W3D chunk count budget exceeded"));
        }
        let kind = u32_at(bytes, position)?;
        let size = (u32_at(bytes, position + 4)? & 0x7fff_ffff) as usize;
        let start = position + 8;
        let end = start
            .checked_add(size)
            .filter(|end| *end <= bytes.len())
            .ok_or_else(|| W3dError::new("truncated W3D chunk"))?;
        out.push(Chunk {
            kind,
            data: &bytes[start..end],
        });
        position = end;
    }
    if bytes[position..].iter().any(|byte| *byte != 0) {
        return Err(W3dError::new("trailing bytes after W3D chunk"));
    }
    Ok(out)
}

fn is_container(kind: u32) -> bool {
    matches!(
        kind,
        MESH | VERTEX_MATERIALS
            | VERTEX_MATERIAL
            | TEXTURES
            | TEXTURE
            | MATERIAL_PASS
            | TEXTURE_STAGE
            | HIERARCHY
            | HLOD
            | HLOD_LOD_ARRAY
            | HLOD_AGGREGATES
            | 0x0500
            | 0x0600
    )
}

fn fixed_name(bytes: &[u8]) -> String {
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).trim().to_string()
}

fn u32_at(bytes: &[u8], offset: usize) -> Result<u32, W3dError> {
    let value = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| W3dError::new("truncated u32"))?;
    Ok(u32::from_le_bytes(value.try_into().unwrap()))
}

fn f32_at(bytes: &[u8], offset: usize) -> Result<f32, W3dError> {
    Ok(f32::from_bits(u32_at(bytes, offset)?))
}

fn vec3_at(bytes: &[u8], offset: usize) -> Result<Vec3, W3dError> {
    Ok(Vec3::new(
        f32_at(bytes, offset)?,
        f32_at(bytes, offset + 4)?,
        f32_at(bytes, offset + 8)?,
    ))
}

fn vec3s(bytes: &[u8]) -> Result<Vec<Vec3>, W3dError> {
    if bytes.len() % 12 != 0 {
        return Err(W3dError::new("invalid vector3 chunk size"));
    }
    (0..bytes.len())
        .step_by(12)
        .map(|offset| vec3_at(bytes, offset))
        .collect()
}

fn vec2s(bytes: &[u8], flip_v: bool) -> Result<Vec<Vec2>, W3dError> {
    if bytes.len() % 8 != 0 {
        return Err(W3dError::new("invalid vector2 chunk size"));
    }
    (0..bytes.len())
        .step_by(8)
        .map(|offset| {
            let u = f32_at(bytes, offset)?;
            let v = f32_at(bytes, offset + 4)?;
            Ok(Vec2::new(u, if flip_v { 1.0 - v } else { v }))
        })
        .collect()
}

fn u32s(bytes: &[u8]) -> Result<Vec<u32>, W3dError> {
    if bytes.len() % 4 != 0 {
        return Err(W3dError::new("invalid u32 array size"));
    }
    (0..bytes.len())
        .step_by(4)
        .map(|offset| u32_at(bytes, offset))
        .collect()
}

fn rgba(bytes: &[u8]) -> Result<Vec<[u8; 4]>, W3dError> {
    if bytes.len() % 4 != 0 {
        return Err(W3dError::new("invalid RGBA array size"));
    }
    Ok(bytes
        .chunks(4)
        .map(|color| [color[0], color[1], color[2], color[3]])
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(kind: u32, payload: &[u8]) -> Vec<u8> {
        let mut out = kind.to_le_bytes().to_vec();
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(payload);
        out
    }

    #[test]
    fn parses_catalog_and_rejects_truncation() {
        let mut header = vec![0; 116];
        header[8..12].copy_from_slice(b"Body");
        header[24..28].copy_from_slice(b"Tank");
        let mesh = chunk(MESH, &chunk(MESH_HEADER3, &header));
        let file = parse(&mesh).unwrap();
        assert!(file
            .catalog("Fallback")
            .iter()
            .any(|model| model.name == "Tank"));

        let mut truncated = mesh;
        truncated.pop();
        assert!(parse(&truncated).is_err());
    }
}
