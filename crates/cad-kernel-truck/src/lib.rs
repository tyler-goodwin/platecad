use cad_ir::{ModelSpec, TessellatedScene};
use cad_kernel::{Kernel, KernelError};
use truck_meshalgo::tessellation::{MeshableShape, MeshedShape};
use truck_modeling::{Point3, Vector3, builder, topology::Shell};

const TESSELLATION_TOL: f64 = 0.01;

#[derive(Debug, Default)]
pub struct TruckKernel;

impl Kernel for TruckKernel {
    fn name(&self) -> &'static str {
        "truck"
    }

    fn build(&mut self, spec: &ModelSpec) -> Result<TessellatedScene, KernelError> {
        match spec {
            ModelSpec::Rect { width, height } => build_rect(*width, *height),
        }
    }
}

fn build_rect(width: f64, height: f64) -> Result<TessellatedScene, KernelError> {
    if width <= 0.0 || height <= 0.0 {
        return Err(KernelError::Invalid(
            "rect width and height must be positive".into(),
        ));
    }

    let v = builder::vertex(Point3::new(0.0, 0.0, 0.0));
    let e = builder::tsweep(&v, Vector3::new(width, 0.0, 0.0));
    let face = builder::tsweep(&e, Vector3::new(0.0, height, 0.0));
    let shell = Shell::from(vec![face]);
    let mesh = shell.triangulation(TESSELLATION_TOL).to_polygon();

    let positions: Vec<[f32; 3]> = mesh
        .positions()
        .iter()
        .map(|p| [p.x as f32, p.y as f32, p.z as f32])
        .collect();

    let mut indices = Vec::new();
    for tri in mesh.faces().triangle_iter() {
        indices.push(tri[0].pos as u32);
        indices.push(tri[1].pos as u32);
        indices.push(tri[2].pos as u32);
    }

    let normals = face_normals(&positions, &indices);

    let w = width as f32;
    let h = height as f32;
    let edges = vec![
        [0.0, 0.0, 0.0],
        [w, 0.0, 0.0],
        [w, 0.0, 0.0],
        [w, h, 0.0],
        [w, h, 0.0],
        [0.0, h, 0.0],
        [0.0, h, 0.0],
        [0.0, 0.0, 0.0],
    ];

    Ok(TessellatedScene {
        positions,
        normals,
        indices,
        edges,
    })
}

fn face_normals(positions: &[[f32; 3]], indices: &[u32]) -> Vec<[f32; 3]> {
    let mut normals = vec![[0.0, 0.0, 0.0]; positions.len()];
    for tri in indices.chunks_exact(3) {
        let a = positions[tri[0] as usize];
        let b = positions[tri[1] as usize];
        let c = positions[tri[2] as usize];
        let ab = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
        let ac = [c[0] - a[0], c[1] - a[1], c[2] - a[2]];
        let n = [
            ab[1] * ac[2] - ab[2] * ac[1],
            ab[2] * ac[0] - ab[0] * ac[2],
            ab[0] * ac[1] - ab[1] * ac[0],
        ];
        for i in tri {
            let dst = &mut normals[*i as usize];
            dst[0] += n[0];
            dst[1] += n[1];
            dst[2] += n[2];
        }
    }
    for n in &mut normals {
        let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
        if len > 0.0 {
            n[0] /= len;
            n[1] /= len;
            n[2] /= len;
        } else {
            *n = [0.0, 0.0, 1.0];
        }
    }
    normals
}

#[cfg(test)]
mod tests {
    use super::*;
    use cad_ir::ModelSpec;

    #[test]
    fn rect_tessellates_to_xy_square() {
        let mut kernel = TruckKernel;
        let scene = kernel
            .build(&ModelSpec::Rect {
                width: 10.0,
                height: 10.0,
            })
            .unwrap();

        assert!(scene.indices.len() >= 6);
        assert_eq!(scene.indices.len() % 3, 0);

        let aabb = scene.aabb().unwrap();
        for i in 0..2 {
            assert!((aabb.min[i] - 0.0).abs() < 1e-4);
            assert!((aabb.max[i] - 10.0).abs() < 1e-4);
        }
        assert!((aabb.min[2]).abs() < 1e-4);
        assert!((aabb.max[2]).abs() < 1e-4);
    }

    #[test]
    fn rejects_non_positive_rect() {
        let mut kernel = TruckKernel;
        assert!(
            kernel
                .build(&ModelSpec::Rect {
                    width: 0.0,
                    height: 10.0,
                })
                .is_err()
        );
    }
}
