use std::sync::mpsc::{Receiver, TryRecvError};

use cad_ir::{Aabb, DEFAULT_COLOR, Diagnostic, TessellatedScene};
use cad_view::{SceneUpdate, ViewError, ViewMode, ViewSelection, ViewportApp};
use three_d::egui;
use three_d::*;

/// three-d window with an orbit camera and an egui status overlay.
pub struct ThreeDViewport {
    pub title: String,
}

impl Default for ThreeDViewport {
    fn default() -> Self {
        Self {
            title: "CAD playground".into(),
        }
    }
}

impl ViewportApp for ThreeDViewport {
    fn run(self, initial: SceneUpdate, updates: Receiver<SceneUpdate>) -> Result<(), ViewError> {
        let window = Window::new(WindowSettings {
            title: self.title,
            max_size: Some((1280, 800)),
            ..Default::default()
        })
        .map_err(|e| ViewError::Message(e.to_string()))?;
        let context = window.gl();

        let mut camera = Camera::new_perspective(
            window.viewport(),
            vec3(5.0, -15.0, 12.0),
            vec3(5.0, 5.0, 0.0),
            vec3(0.0, 0.0, 1.0),
            degrees(45.0),
            0.1,
            1000.0,
        );
        let mut control = OrbitControl::new(vec3(5.0, 5.0, 0.0), 1.0, 500.0);
        let mut gui = GUI::new(&context);

        let ambient = AmbientLight::new(&context, 0.4, Srgba::WHITE);
        let key = DirectionalLight::new(&context, 2.0, Srgba::WHITE, vec3(-0.35, 0.8, -0.6));
        let fill = DirectionalLight::new(&context, 1.0, Srgba::WHITE, vec3(0.5, -0.5, 1.0));

        let mut state = ViewState {
            context: context.clone(),
            assembled: None,
            print: None,
            selection: ViewSelection::default(),
            framed: None,
            diagnostics: Vec::new(),
            disconnected: false,
        };
        state.apply(initial);
        state.reframe_if_needed(&mut camera, &mut control);

        window.render_loop(move |mut frame_input| {
            loop {
                match updates.try_recv() {
                    Ok(update) => state.apply(update),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        state.disconnected = true;
                        break;
                    }
                }
            }

            gui.update(
                &mut frame_input.events,
                frame_input.accumulated_time,
                frame_input.viewport,
                frame_input.device_pixel_ratio,
                |ui| {
                    egui::Window::new("Model")
                        .default_pos([12.0, 12.0])
                        .resizable(false)
                        .show(ui.ctx(), |ui| {
                            let has_print = state.print.is_some();
                            ui.horizontal(|ui| {
                                ui.selectable_value(
                                    &mut state.selection.requested,
                                    ViewMode::Assembled,
                                    "Assembled",
                                );
                                ui.add_enabled_ui(has_print, |ui| {
                                    ui.selectable_value(
                                        &mut state.selection.requested,
                                        ViewMode::Print,
                                        "Print",
                                    )
                                })
                                .response
                                .on_disabled_hover_text(
                                    "This model has no print view. Export { assembled, print }.",
                                );
                            });
                            ui.separator();
                            for d in &state.diagnostics {
                                let color = if d.ok {
                                    egui::Color32::from_rgb(120, 200, 120)
                                } else {
                                    egui::Color32::from_rgb(230, 100, 100)
                                };
                                ui.colored_label(color, &d.message);
                            }
                            if state.disconnected {
                                ui.label("Reload thread stopped.");
                            }
                            ui.separator();
                            ui.weak("Drag to orbit, scroll to zoom");
                        });
                },
            );

            state.reframe_if_needed(&mut camera, &mut control);
            camera.set_viewport(frame_input.viewport);
            control.handle_events(&mut camera, &mut frame_input.events);

            frame_input
                .screen()
                .clear(ClearState::color_and_depth(0.12, 0.13, 0.15, 1.0, 1.0))
                .render(&camera, state.objects(), &[&ambient, &key, &fill])
                .write(|| gui.render())
                .unwrap();

            FrameOutput::default()
        });

        Ok(())
    }
}

/// One view's meshes on the GPU.
struct GpuScene {
    model: Option<Gm<Mesh, PhysicalMaterial>>,
    edges: Option<Gm<InstancedMesh, ColorMaterial>>,
    aabb: Option<Aabb>,
}

impl GpuScene {
    fn new(context: &Context, scene: &TessellatedScene) -> Self {
        Self {
            model: build_model(context, scene),
            edges: build_edges(context, scene),
            aabb: scene.aabb(),
        }
    }
}

struct ViewState {
    context: Context,
    assembled: Option<GpuScene>,
    print: Option<GpuScene>,
    selection: ViewSelection,
    /// View the camera was last framed for; reframe when the shown view changes.
    framed: Option<ViewMode>,
    diagnostics: Vec<Diagnostic>,
    disconnected: bool,
}

impl ViewState {
    fn apply(&mut self, update: SceneUpdate) {
        if let Some(views) = update.views {
            self.assembled = Some(GpuScene::new(&self.context, &views.assembled));
            self.print = views.print.as_ref().map(|s| GpuScene::new(&self.context, s));
        }
        self.diagnostics = update.diagnostics;
    }

    fn active(&self) -> Option<&GpuScene> {
        match self.selection.active(self.print.is_some()) {
            ViewMode::Print => self.print.as_ref(),
            ViewMode::Assembled => self.assembled.as_ref(),
        }
    }

    /// Frames the camera on the shown view the first time it appears, and
    /// whenever the user switches views. Reloads keep the user's camera.
    fn reframe_if_needed(&mut self, camera: &mut Camera, control: &mut OrbitControl) {
        let mode = self.selection.active(self.print.is_some());
        if !needs_reframe(self.framed, mode) {
            return;
        }
        if let Some(aabb) = self.active().and_then(|s| s.aabb) {
            frame_camera(aabb, camera, control);
            self.framed = Some(mode);
        }
    }

    fn objects(&self) -> Vec<&dyn Object> {
        let mut objects: Vec<&dyn Object> = Vec::new();
        if let Some(scene) = self.active() {
            if let Some(model) = &scene.model {
                objects.push(model);
            }
            if let Some(edges) = &scene.edges {
                objects.push(edges);
            }
        }
        objects
    }
}

fn needs_reframe(framed: Option<ViewMode>, active: ViewMode) -> bool {
    framed != Some(active)
}

fn build_model(context: &Context, scene: &TessellatedScene) -> Option<Gm<Mesh, PhysicalMaterial>> {
    if scene.is_empty() {
        return None;
    }
    let cpu_mesh = CpuMesh {
        positions: Positions::F32(scene.positions.iter().copied().map(to_vec3).collect()),
        indices: Indices::U32(scene.indices.clone()),
        normals: (scene.normals.len() == scene.positions.len())
            .then(|| scene.normals.iter().copied().map(to_vec3).collect()),
        colors: vertex_colors(scene),
        ..Default::default()
    };
    // three-d multiplies albedo by vertex color, so use white when the mesh has colors.
    let [r, g, b] = if cpu_mesh.colors.is_some() {
        [255; 3]
    } else {
        DEFAULT_COLOR
    };
    let mut material = PhysicalMaterial::new_opaque(
        context,
        &CpuMaterial {
            albedo: Srgba::new(r, g, b, 255),
            roughness: 0.6,
            metallic: 0.0,
            ..Default::default()
        },
    );
    // Planar faces should read from both sides.
    material.render_states.cull = Cull::None;
    Some(Gm::new(Mesh::new(context, &cpu_mesh), material))
}

/// Per-vertex colors, or `None` if the scene carries none (or a mismatched count).
fn vertex_colors(scene: &TessellatedScene) -> Option<Vec<Srgba>> {
    (!scene.colors.is_empty() && scene.colors.len() == scene.positions.len()).then(|| {
        scene
            .colors
            .iter()
            .map(|&[r, g, b]| Srgba::new(r, g, b, 255))
            .collect()
    })
}

/// Draws the B-rep edge list as thin cylinders (GL line width isn't portable).
fn build_edges(context: &Context, scene: &TessellatedScene) -> Option<Gm<InstancedMesh, ColorMaterial>> {
    let framing = framing(scene.aabb()?);
    let transformations: Vec<Mat4> = scene
        .edges
        .chunks_exact(2)
        .filter_map(|pair| edge_transform(to_vec3(pair[0]), to_vec3(pair[1])))
        .collect();
    if transformations.is_empty() {
        return None;
    }

    let mut cylinder = CpuMesh::cylinder(8);
    cylinder
        .transform(Mat4::from_nonuniform_scale(1.0, framing.edge_radius, framing.edge_radius))
        .ok()?;
    let instances = Instances {
        transformations,
        ..Default::default()
    };
    let material = ColorMaterial {
        color: Srgba::new(25, 30, 40, 255),
        ..Default::default()
    };
    Some(Gm::new(InstancedMesh::new(context, &instances, &cylinder), material))
}

/// Maps three-d's unit cylinder (x from 0 to 1) onto the segment from `a` to `b`.
/// `None` for degenerate segments.
fn edge_transform(a: Vec3, b: Vec3) -> Option<Mat4> {
    let length = (b - a).magnitude();
    (length > 1e-6).then(|| {
        Mat4::from_translation(a)
            * Mat4::from(Quat::from_arc(vec3(1.0, 0.0, 0.0), (b - a) / length, None))
            * Mat4::from_nonuniform_scale(length, 1.0, 1.0)
    })
}

/// Camera placement and scale-dependent sizes derived from the model bounds.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Framing {
    center: Vec3,
    eye: Vec3,
    min_distance: f32,
    max_distance: f32,
    edge_radius: f32,
}

/// Three-quarter view (front, right and top visible, Z up) that fits the bounds.
fn framing(aabb: Aabb) -> Framing {
    let min = to_vec3(aabb.min);
    let max = to_vec3(aabb.max);
    let center = (min + max) * 0.5;
    let diagonal = (max - min).magnitude();
    let radius = (diagonal * 0.5).max(1.0);
    Framing {
        center,
        eye: center + vec3(1.0, -1.6, 1.1).normalize() * radius * 3.0,
        min_distance: radius * 0.2,
        max_distance: radius * 50.0,
        edge_radius: (diagonal * 0.002).max(1e-3),
    }
}

fn frame_camera(aabb: Aabb, camera: &mut Camera, control: &mut OrbitControl) {
    let f = framing(aabb);
    camera.set_view(f.eye, f.center, vec3(0.0, 0.0, 1.0));
    *control = OrbitControl::new(f.center, f.min_distance, f.max_distance);
}

fn to_vec3(p: [f32; 3]) -> Vec3 {
    vec3(p[0], p[1], p[2])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(m: Mat4, p: Vec3) -> Vec3 {
        let q = m * p.extend(1.0);
        q.truncate() / q.w
    }

    fn assert_close(a: Vec3, b: Vec3) {
        assert!((a - b).magnitude() < 1e-4, "{a:?} != {b:?}");
    }

    fn aabb(min: [f32; 3], max: [f32; 3]) -> Aabb {
        Aabb { min, max }
    }

    #[test]
    fn edge_transform_maps_unit_cylinder_onto_segment() {
        let cases = [
            (vec3(0.0, 0.0, 0.0), vec3(10.0, 0.0, 0.0)),
            (vec3(10.0, 0.0, 0.0), vec3(10.0, 10.0, 0.0)),
            (vec3(0.0, 0.0, 0.0), vec3(0.0, 0.0, 10.0)),
            (vec3(1.0, 2.0, 3.0), vec3(-4.0, 5.0, -6.0)),
            // Antiparallel to the cylinder axis: the rotation's degenerate case.
            (vec3(10.0, 0.0, 0.0), vec3(0.0, 0.0, 0.0)),
        ];
        for (a, b) in cases {
            let m = edge_transform(a, b).unwrap();
            assert_close(apply(m, vec3(0.0, 0.0, 0.0)), a);
            assert_close(apply(m, vec3(1.0, 0.0, 0.0)), b);
            // Radius is untouched: a point 1 unit off-axis stays 1 unit from the segment.
            let off_axis = apply(m, vec3(0.5, 1.0, 0.0));
            assert!(((off_axis - (a + b) * 0.5).magnitude() - 1.0).abs() < 1e-4);
        }
    }

    fn scene_with(positions: usize, colors: Vec<[u8; 3]>) -> TessellatedScene {
        TessellatedScene {
            positions: vec![[0.0; 3]; positions],
            colors,
            ..Default::default()
        }
    }

    #[test]
    fn vertex_colors_pass_srgb_through_opaque() {
        let scene = scene_with(2, vec![[176, 122, 79], [0, 255, 10]]);
        assert_eq!(
            vertex_colors(&scene),
            Some(vec![Srgba::new(176, 122, 79, 255), Srgba::new(0, 255, 10, 255)])
        );
    }

    #[test]
    fn vertex_colors_absent_or_mismatched_fall_back() {
        assert_eq!(vertex_colors(&scene_with(3, vec![])), None);
        assert_eq!(vertex_colors(&scene_with(3, vec![[1, 2, 3]])), None);
    }

    #[test]
    fn reframes_on_first_show_and_view_switch_only() {
        assert!(needs_reframe(None, ViewMode::Assembled));
        assert!(!needs_reframe(Some(ViewMode::Assembled), ViewMode::Assembled));
        assert!(needs_reframe(Some(ViewMode::Assembled), ViewMode::Print));
        assert!(needs_reframe(Some(ViewMode::Print), ViewMode::Assembled));
    }

    #[test]
    fn edge_transform_skips_degenerate_segments() {
        assert!(edge_transform(vec3(1.0, 1.0, 1.0), vec3(1.0, 1.0, 1.0)).is_none());
    }

    #[test]
    fn framing_targets_center_from_front_right_top() {
        let f = framing(aabb([0.0; 3], [10.0; 3]));
        assert_close(f.center, vec3(5.0, 5.0, 5.0));
        let view = f.eye - f.center;
        assert!(view.x > 0.0 && view.y < 0.0 && view.z > 0.0, "{view:?}");
    }

    #[test]
    fn framing_keeps_model_in_view_and_orbit_range() {
        let f = framing(aabb([0.0; 3], [10.0; 3]));
        let radius = 75f32.sqrt(); // half the cube diagonal
        let distance = (f.eye - f.center).magnitude();
        // Far enough that the bounding sphere fits the 45 degree vertical FOV.
        assert!(distance * 22.5f32.to_radians().sin() > radius, "{distance}");
        assert!(f.min_distance < distance && distance < f.max_distance);
        assert!(f.min_distance > radius * 0.1);
    }

    #[test]
    fn framing_scales_with_model_size() {
        let small = framing(aabb([0.0; 3], [10.0; 3]));
        let large = framing(aabb([0.0; 3], [1000.0; 3]));
        let ratio = (large.eye - large.center).magnitude() / (small.eye - small.center).magnitude();
        assert!((ratio - 100.0).abs() < 1e-2, "{ratio}");
        assert!((large.edge_radius / small.edge_radius - 100.0).abs() < 1e-2);
    }

    #[test]
    fn framing_handles_flat_and_point_bounds() {
        let flat = framing(aabb([0.0; 3], [10.0, 10.0, 0.0]));
        assert!(flat.eye.z > flat.center.z);

        // A single point still gets a usable camera distance and edge radius.
        let point = framing(aabb([3.0; 3], [3.0; 3]));
        assert!((point.eye - point.center).magnitude() >= 1.0);
        assert!(point.edge_radius > 0.0);
    }
}
