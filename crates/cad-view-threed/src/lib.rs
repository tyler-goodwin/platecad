use std::sync::mpsc::{Receiver, TryRecvError};

use cad_ir::{Aabb, DEFAULT_COLOR, Diagnostic, Plate, TessellatedScene};
use std::time::Instant;

use cad_view::{SceneUpdate, ViewError, ViewMode, ViewSelection, ViewportApp, loading_status};
use three_d::egui;
use three_d::*;

/// Vertical field of view of the viewport camera.
const FOV_Y_DEGREES: f32 = 45.0;

/// Viewport background.
const BACKGROUND: [u8; 3] = [31, 33, 38];
/// B-rep edge lines: a slate between the background and typical face colors,
/// so edges read against both.
const EDGE_COLOR: [u8; 3] = [72, 80, 96];
/// Edge line radius as a fraction of the model's bounding diagonal.
const EDGE_RADIUS_FRACTION: f32 = 0.0008;
/// Build plate outlines in the print view.
const PLATE_COLOR: [u8; 3] = [90, 150, 210];

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
            degrees(FOV_Y_DEGREES),
            0.1,
            1000.0, // Placeholder: replaced by `frame_camera` once a model loads.
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
            loading: None,
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
                            if let Some((message, since)) = &state.loading {
                                ui.horizontal(|ui| {
                                    ui.spinner();
                                    ui.label(loading_status(message, since.elapsed()));
                                });
                                ui.separator();
                            }
                            if let Some(scene) = state.active() {
                                if !scene.parts.is_empty() {
                                    egui::CollapsingHeader::new(format!("Parts ({})", scene.parts.len()))
                                        .default_open(false)
                                        .show(ui, |ui| {
                                            for (name, triangles) in &scene.parts {
                                                ui.label(format!("{name} ({triangles} triangles)"));
                                            }
                                        });
                                    ui.separator();
                                }
                            }
                            for d in &state.diagnostics {
                                let [r, g, b] = diagnostic_color(d);
                                ui.colored_label(egui::Color32::from_rgb(r, g, b), &d.message);
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
                .clear(ClearState::color_and_depth(
                    BACKGROUND[0] as f32 / 255.0,
                    BACKGROUND[1] as f32 / 255.0,
                    BACKGROUND[2] as f32 / 255.0,
                    1.0,
                    1.0,
                ))
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
    plates: Option<Gm<InstancedMesh, ColorMaterial>>,
    aabb: Option<Aabb>,
    parts: Vec<(String, usize)>,
}

impl GpuScene {
    fn new(context: &Context, scene: &TessellatedScene) -> Self {
        let aabb = view_bounds(scene);
        let radius = aabb.map(|b| framing(b).edge_radius).unwrap_or(1.0);
        let edges = scene.edges.chunks_exact(2).map(|pair| (pair[0], pair[1]));
        Self {
            model: build_model(context, scene),
            edges: build_lines(context, edges, radius, EDGE_COLOR),
            plates: build_lines(context, plate_segments(&scene.plates), radius, PLATE_COLOR),
            aabb,
            parts: part_list(scene),
        }
    }
}

/// What the camera should frame: the geometry plus any build plates.
fn view_bounds(scene: &TessellatedScene) -> Option<Aabb> {
    let corners = scene.plates.iter().flat_map(|p| {
        let [x, y] = p.min;
        let [w, d] = p.size;
        [[x, y, 0.0], [x + w, y + d, 0.0]]
    });
    let points: Vec<[f32; 3]> = scene.positions.iter().copied().chain(corners).collect();
    TessellatedScene {
        positions: points,
        ..Default::default()
    }
    .aabb()
}

/// Outline segments of each plate, at z = 0.
fn plate_segments(plates: &[Plate]) -> Vec<([f32; 3], [f32; 3])> {
    plates
        .iter()
        .flat_map(|p| {
            let [x0, y0] = p.min;
            let [x1, y1] = [x0 + p.size[0], y0 + p.size[1]];
            let c = [[x0, y0, 0.0], [x1, y0, 0.0], [x1, y1, 0.0], [x0, y1, 0.0]];
            (0..4).map(move |i| (c[i], c[(i + 1) % 4]))
        })
        .collect()
}

/// Overlay text color: green for OK, amber for warnings, red for errors.
fn diagnostic_color(d: &Diagnostic) -> [u8; 3] {
    match (d.ok, d.warning) {
        (true, false) => [120, 200, 120],
        (true, true) => [235, 180, 70],
        (false, _) => [230, 100, 100],
    }
}

/// Part names with triangle counts, in first-seen order. A part split into
/// several runs (by a nested part) is listed once.
fn part_list(scene: &TessellatedScene) -> Vec<(String, usize)> {
    let mut list: Vec<(String, usize)> = Vec::new();
    for part in &scene.parts {
        let triangles = part.indices.len() / 3;
        match list.iter_mut().find(|(name, _)| *name == part.name) {
            Some((_, count)) => *count += triangles,
            None => list.push((part.name.clone(), triangles)),
        }
    }
    list
}

struct ViewState {
    context: Context,
    assembled: Option<GpuScene>,
    print: Option<GpuScene>,
    selection: ViewSelection,
    /// View the camera was last framed for; reframe when the shown view changes.
    framed: Option<ViewMode>,
    diagnostics: Vec<Diagnostic>,
    /// An evaluation in progress, and when it started.
    loading: Option<(String, Instant)>,
    disconnected: bool,
}

impl ViewState {
    fn apply(&mut self, update: SceneUpdate) {
        if let Some(message) = update.loading {
            self.loading = Some((message, Instant::now()));
            return;
        }
        self.loading = None;
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
            if let Some(plates) = &scene.plates {
                objects.push(plates);
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

/// Draws line segments as thin cylinders (GL line width isn't portable).
fn build_lines(
    context: &Context,
    segments: impl IntoIterator<Item = ([f32; 3], [f32; 3])>,
    radius: f32,
    [r, g, b]: [u8; 3],
) -> Option<Gm<InstancedMesh, ColorMaterial>> {
    let transformations: Vec<Mat4> = segments
        .into_iter()
        .filter_map(|(a, b)| edge_transform(to_vec3(a), to_vec3(b)))
        .collect();
    if transformations.is_empty() {
        return None;
    }

    let mut cylinder = CpuMesh::cylinder(8);
    cylinder
        .transform(Mat4::from_nonuniform_scale(1.0, radius, radius))
        .ok()?;
    let instances = Instances {
        transformations,
        ..Default::default()
    };
    let material = ColorMaterial {
        color: Srgba::new(r, g, b, 255),
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
    /// Clip planes that keep the whole model visible across the orbit range.
    z_near: f32,
    z_far: f32,
    edge_radius: f32,
}

/// Three-quarter view (front, right and top visible, Z up) that fits the bounds.
fn framing(aabb: Aabb) -> Framing {
    let min = to_vec3(aabb.min);
    let max = to_vec3(aabb.max);
    let center = (min + max) * 0.5;
    let diagonal = (max - min).magnitude();
    let radius = (diagonal * 0.5).max(1.0);
    let min_distance = radius * 0.2;
    let max_distance = radius * 50.0;
    Framing {
        center,
        eye: center + vec3(1.0, -1.6, 1.1).normalize() * radius * 3.0,
        min_distance,
        max_distance,
        // Scale with the model: a fixed far plane clips anything big (a 1 m
        // bookshelf framed from ~2 m vanished behind a 1000 mm far plane).
        z_near: min_distance * 0.05,
        z_far: max_distance + radius * 2.0,
        edge_radius: (diagonal * EDGE_RADIUS_FRACTION).max(1e-3),
    }
}

fn frame_camera(aabb: Aabb, camera: &mut Camera, control: &mut OrbitControl) {
    let f = framing(aabb);
    camera.set_perspective_projection(degrees(FOV_Y_DEGREES), f.z_near, f.z_far);
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
    fn part_list_merges_split_runs_in_first_seen_order() {
        let part = |name: &str, indices: std::ops::Range<u32>| cad_ir::ScenePart {
            name: name.into(),
            vertices: 0..0,
            indices,
        };
        let scene = TessellatedScene {
            parts: vec![part("frame", 0..6), part("leg", 6..42), part("frame", 42..48)],
            ..Default::default()
        };
        assert_eq!(
            part_list(&scene),
            vec![("frame".to_string(), 4), ("leg".to_string(), 12)]
        );
        assert!(part_list(&TessellatedScene::default()).is_empty());
    }

    fn plate(min: [f32; 2], size: [f32; 2]) -> Plate {
        Plate { min, size }
    }

    #[test]
    fn plate_outlines_are_closed_rectangles_on_the_bed() {
        let segments = plate_segments(&[plate([0.0, 0.0], [256.0, 200.0]), plate([300.0, 0.0], [256.0, 200.0])]);
        assert_eq!(segments.len(), 8);
        for chunk in segments.chunks(4) {
            for i in 0..4 {
                assert_eq!(chunk[i].1, chunk[(i + 1) % 4].0, "outline must close");
                assert_eq!((chunk[i].0[2], chunk[i].1[2]), (0.0, 0.0));
            }
        }
        assert_eq!(segments[4].0, [300.0, 0.0, 0.0]);
        assert_eq!(segments[5].1, [556.0, 200.0, 0.0]);
    }

    #[test]
    fn view_bounds_include_plates() {
        let scene = TessellatedScene {
            positions: vec![[10.0, 10.0, 0.0], [20.0, 20.0, 5.0]],
            plates: vec![plate([0.0, 0.0], [256.0, 256.0])],
            ..Default::default()
        };
        let b = view_bounds(&scene).unwrap();
        assert_eq!((b.min, b.max), ([0.0, 0.0, 0.0], [256.0, 256.0, 5.0]));

        let bare = TessellatedScene {
            positions: vec![[1.0, 2.0, 3.0]],
            ..Default::default()
        };
        assert_eq!(view_bounds(&bare), bare.aabb());
        assert_eq!(view_bounds(&TessellatedScene::default()), None);
    }

    #[test]
    fn diagnostics_are_green_amber_red() {
        let [ok, warning, error] = [
            diagnostic_color(&Diagnostic::ok("")),
            diagnostic_color(&Diagnostic::warning("")),
            diagnostic_color(&Diagnostic::error("")),
        ];
        assert!(ok[1] > ok[0] && ok[1] > ok[2], "ok should read green: {ok:?}");
        assert!(warning[0] > warning[2] && warning[1] > warning[2], "warning should read amber: {warning:?}");
        assert!(error[0] > error[1] && error[0] > error[2], "error should read red: {error:?}");
    }

    /// Approximate perceived brightness (Rec. 709 weights on sRGB bytes).
    fn luma([r, g, b]: [u8; 3]) -> f32 {
        0.2126 * r as f32 + 0.7152 * g as f32 + 0.0722 * b as f32
    }

    #[test]
    fn edges_stand_out_from_background_and_faces() {
        let edge = luma(EDGE_COLOR);
        assert!(edge - luma(BACKGROUND) >= 30.0, "edges blend into the background");
        // Faces: the default color and the bookshelf's wood and backing.
        for face in [DEFAULT_COLOR, [0xB0, 0x7A, 0x4F], [0xE8, 0xE0, 0xD0]] {
            assert!(luma(face) - edge >= 40.0, "edges blend into {face:?}");
        }
        assert!(luma(PLATE_COLOR) - luma(BACKGROUND) >= 30.0, "plates blend into the background");
    }

    #[test]
    fn edges_are_thin_next_to_thin_parts() {
        // An 18 mm board in a 1 m bookshelf: the line should be well under a
        // quarter of the board's thickness.
        let f = framing(aabb([0.0; 3], [1000.0, 300.0, 800.0]));
        assert!(f.edge_radius * 2.0 < 18.0 / 4.0, "{}", f.edge_radius);
        // A 10 mm cube still gets a visible line.
        assert!(framing(aabb([0.0; 3], [10.0; 3])).edge_radius >= 1e-3);
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

    /// The model's bounding sphere stays between the clip planes when framed and
    /// at every orbit distance, from a 1 mm part to a 10 m assembly.
    #[test]
    fn clip_planes_contain_model_at_every_scale() {
        for size in [1.0, 10.0, 300.0, 1000.0, 10_000.0] {
            let f = framing(aabb([0.0; 3], [size, size * 0.3, size * 0.8]));
            let radius = vec3(size, size * 0.3, size * 0.8).magnitude() * 0.5;
            let framed = (f.eye - f.center).magnitude();
            for distance in [framed, f.max_distance] {
                assert!(distance + radius < f.z_far, "size {size}: far side clipped at {distance}");
                assert!(distance - radius > f.z_near, "size {size}: near side clipped at {distance}");
            }
            // Zoomed all the way in, the camera is inside the model: the near
            // plane must still be closer than the closest orbit distance.
            assert!(f.z_near < f.min_distance, "size {size}");
            assert!(f.z_far / f.z_near < 1e5, "size {size}: depth precision ratio too large");
        }
    }

    #[test]
    fn bookshelf_scale_regression() {
        // models/bookshelf.ts defaults: 1000 x 300 x 800 mm.
        let f = framing(aabb([0.0; 3], [1000.0, 300.0, 800.0]));
        let framed = (f.eye - f.center).magnitude();
        assert!(framed > 1000.0, "premise: framed beyond the old fixed 1000 mm far plane");
        assert!(framed + 700.0 < f.z_far);
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
