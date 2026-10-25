use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};

use cad_ir::{Aabb, DEFAULT_COLOR, Diagnostic, MeshQuality, Plate, TessellatedScene};
use std::time::Instant;

use cad_view::{
    FileEntry, FolderListing, SceneUpdate, Settings, ViewError, ViewMode, ViewSelection, ViewerCommand,
    ViewerMessage, ViewportApp, loading_status,
};
use three_d::egui;
use three_d::*;
use winit::event_loop::EventLoopProxy;

/// Wakes the render loop, which sleeps until the next window event.
type Waker = EventLoopProxy<()>;

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

/// Size the window opens at (and its maximum), in logical pixels.
const WINDOW_SIZE: (u32, u32) = (1280, 800);

/// Resource id of the app icon in the executable. The `plate` binary embeds
/// `assets/plate.ico` under it (see `crates/platecad/build.rs`).
pub const ICON_RESOURCE_ID: u16 = 1;

/// three-d window with an orbit camera and an egui status overlay.
pub struct ThreeDViewport {
    pub title: String,
}

impl Default for ThreeDViewport {
    fn default() -> Self {
        Self {
            title: "Plate CAD".into(),
        }
    }
}

impl ViewportApp for ThreeDViewport {
    fn run(
        self,
        initial: Vec<ViewerMessage>,
        messages: Receiver<ViewerMessage>,
        commands: Sender<ViewerCommand>,
    ) -> Result<(), ViewError> {
        // Settings come first among the initial messages; the window needs
        // the antialiasing choice before it opens.
        let settings = initial_settings(&initial);
        let (window, waker) = open_window(&self.title, surface_settings(&settings))?;
        let context = window.gl();
        let messages = forward_waking(messages, waker.clone());

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
            folder: None,
            model: None,
            notice: None,
            settings,
        };
        for message in initial {
            state.receive(message);
        }
        state.reframe_if_needed(&mut camera, &mut control);
        let picking = Arc::new(AtomicBool::new(false));
        let mut settings_open = false;

        window.render_loop(move |mut frame_input| {
            loop {
                match messages.try_recv() {
                    Ok(message) => state.receive(message),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        state.disconnected = true;
                        break;
                    }
                }
            }

            // Right edge of the files panel, in egui points; the 3D view fills the rest.
            let mut files_right = 0.0;
            gui.update(
                &mut frame_input.events,
                frame_input.accumulated_time,
                frame_input.viewport,
                frame_input.device_pixel_ratio,
                |ui| {
                    files_right = egui::Panel::left("files")
                        .resizable(true)
                        .default_size(220.0)
                        .show_inside(ui, |ui| {
                            let busy = picking.load(Ordering::Relaxed);
                            let folder = state.folder.as_ref().map(|f| f.path.clone());
                            ui.horizontal(|ui| {
                                if ui.add_enabled(!busy, egui::Button::new("Open folder…")).clicked() {
                                    let waker = waker.clone();
                                    pick_folder(Picker::Open, folder.clone(), picking.clone(), commands.clone(), waker);
                                }
                                let new = ui
                                    .add_enabled(!busy, egui::Button::new("New project…"))
                                    .on_hover_text(
                                        "Pick or create an empty folder for a printer config, \
                                         the DSL and a starter model.",
                                    );
                                if new.clicked() {
                                    // Start beside the open folder: new projects are usually siblings.
                                    let start = folder.as_deref().and_then(Path::parent).map(Path::to_path_buf);
                                    let waker = waker.clone();
                                    pick_folder(Picker::NewProject, start, picking.clone(), commands.clone(), waker);
                                }
                            });
                            ui.separator();
                            match &state.folder {
                                Some(folder) => {
                                    ui.strong(folder_label(&folder.path));
                                    egui::ScrollArea::vertical().show(ui, |ui| {
                                        if folder.entries.is_empty() {
                                            ui.weak("No .ts files here.");
                                        }
                                        file_tree(ui, &folder.entries, state.model.as_deref(), &commands);
                                    });
                                }
                                None => {
                                    ui.weak("Open a folder to list its models.");
                                }
                            }
                        })
                        .response
                        .rect
                        .right();
                    egui::Window::new("Model")
                        .default_pos([240.0, 12.0])
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
                                ui.separator();
                                if ui.selectable_label(settings_open, "⚙").on_hover_text("Settings").clicked() {
                                    settings_open = !settings_open;
                                }
                            });
                            let can_export = state.active().is_some();
                            let export = ui
                                .add_enabled(can_export, egui::Button::new("Export 3MF"))
                                .on_hover_text("Writes the view on screen to outputs/<model>_<view>.3mf")
                                .on_disabled_hover_text("Open a model that builds to export it.");
                            if export.clicked() {
                                let view = state.selection.active(state.print.is_some());
                                let _ = commands.send(ViewerCommand::Export(view));
                            }
                            if let Some(notice) = &state.notice {
                                let [r, g, b] = diagnostic_color(notice);
                                ui.colored_label(egui::Color32::from_rgb(r, g, b), &notice.message);
                            }
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
                            if state.model.is_none() && state.loading.is_none() {
                                ui.weak("Pick a model from the file list.");
                            }
                            if state.disconnected {
                                ui.label("Reload thread stopped.");
                            }
                            ui.separator();
                            ui.weak("Drag to orbit, scroll to zoom");
                        });
                    egui::Window::new("Settings")
                        .open(&mut settings_open)
                        .default_pos([240.0, 280.0])
                        .resizable(false)
                        .show(ui.ctx(), |ui| {
                            let before = state.settings;
                            settings_ui(ui, &mut state.settings);
                            if state.settings != before {
                                let _ = commands.send(ViewerCommand::Settings(state.settings));
                            }
                        });
                },
            );

            state.reframe_if_needed(&mut camera, &mut control);
            camera.set_viewport(scene_viewport(
                frame_input.viewport,
                files_right,
                frame_input.device_pixel_ratio,
            ));
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

            FrameOutput {
                wait_next_event: !keep_drawing(state.loading.is_some(), gui.context().has_requested_repaint()),
                ..Default::default()
            }
        });

        Ok(())
    }
}

/// Whether to draw the next frame right away instead of sleeping until an
/// input event or app message: while a build runs (its timer ticks) and
/// while egui animates or has a delayed repaint (a tooltip) pending.
fn keep_drawing(loading: bool, gui_wants_repaint: bool) -> bool {
    loading || gui_wants_repaint
}

/// The settings among the viewer's first messages, or the defaults.
fn initial_settings(initial: &[ViewerMessage]) -> Settings {
    initial
        .iter()
        .find_map(|m| match m {
            ViewerMessage::Settings(settings) => Some(*settings),
            _ => None,
        })
        .unwrap_or_default()
}

/// three-d's default surface, with multisampling only if antialiasing is on.
fn surface_settings(settings: &Settings) -> SurfaceSettings {
    SurfaceSettings {
        multisamples: if settings.antialiasing { 4 } else { 0 },
        ..Default::default()
    }
}

/// Relays `incoming` to the returned receiver, waking the render loop for
/// each message, and once more when `incoming` closes so the loop notices.
fn forward_waking<T: Send + 'static>(incoming: Receiver<T>, waker: Waker) -> Receiver<T> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for message in incoming {
            if tx.send(message).is_err() {
                return;
            }
            let _ = waker.send_event(());
        }
        drop(tx);
        let _ = waker.send_event(());
    });
    rx
}

/// The settings window's contents. Edits `settings` in place.
fn settings_ui(ui: &mut egui::Ui, settings: &mut Settings) {
    egui::Grid::new("settings").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
        ui.label("Mesh on screen")
            .on_hover_text("Coarser meshes rebuild and draw faster. Changing it rebuilds the model.");
        quality_picker(ui, &mut settings.view_quality);
        ui.end_row();
        ui.label("Mesh for export")
            .on_hover_text("Used by Export 3MF. A model on screen at another quality is meshed again.");
        quality_picker(ui, &mut settings.export_quality);
        ui.end_row();
    });
    ui.separator();
    ui.checkbox(&mut settings.show_edges, "Show edges");
    ui.checkbox(&mut settings.antialiasing, "Antialiasing")
        .on_hover_text("Smooths jagged edges at some GPU cost. Applies the next time Plate CAD opens.");
}

fn quality_picker(ui: &mut egui::Ui, quality: &mut MeshQuality) {
    ui.horizontal(|ui| {
        for q in MeshQuality::ALL {
            ui.selectable_value(quality, q, q.label())
                .on_hover_text(format!("Within {} mm of the true surface", q.chord_tolerance()));
        }
    });
}

/// The window three-d would open for `WindowSettings { max_size: WINDOW_SIZE }`,
/// built here so it can carry the app icon, plus a way to wake its render loop.
fn open_window(title: &str, surface: SurfaceSettings) -> Result<(Window, Waker), ViewError> {
    use winit::dpi::LogicalSize;
    let event_loop = winit::event_loop::EventLoop::new();
    let waker = event_loop.create_proxy();
    let (width, height) = WINDOW_SIZE;
    let builder = winit::window::WindowBuilder::new()
        .with_title(title)
        .with_min_inner_size(LogicalSize::new(2, 2))
        .with_inner_size(LogicalSize::new(width, height))
        .with_max_inner_size(LogicalSize::new(width, height));
    let window = with_app_icon(builder)
        .build(&event_loop)
        .map_err(|e| ViewError::Message(e.to_string()))?;
    window.focus_window();
    let window =
        Window::from_winit_window(window, event_loop, surface, false).map_err(|e| ViewError::Message(e.to_string()))?;
    Ok((window, waker))
}

/// Title bar and taskbar icon from the executable's embedded icon resource.
/// Without one (test binaries, other hosts), the window keeps the default.
#[cfg(windows)]
fn with_app_icon(builder: winit::window::WindowBuilder) -> winit::window::WindowBuilder {
    use winit::platform::windows::{IconExtWindows, WindowBuilderExtWindows};
    match winit::window::Icon::from_resource(ICON_RESOURCE_ID, None) {
        Ok(icon) => builder.with_window_icon(Some(icon.clone())).with_taskbar_icon(Some(icon)),
        Err(_) => builder,
    }
}

#[cfg(not(windows))]
fn with_app_icon(builder: winit::window::WindowBuilder) -> winit::window::WindowBuilder {
    builder
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

/// What a folder picked in the native dialog is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Picker {
    Open,
    NewProject,
}

impl Picker {
    fn title(self) -> &'static str {
        match self {
            Picker::Open => "Open folder",
            Picker::NewProject => "New project: pick or create an empty folder",
        }
    }

    fn command(self, folder: PathBuf) -> ViewerCommand {
        match self {
            Picker::Open => ViewerCommand::OpenFolder(folder),
            Picker::NewProject => ViewerCommand::NewProject(folder),
        }
    }
}

/// Shows the native folder picker on its own thread, so the window keeps
/// drawing, and sends the choice as `picker`'s command. Wakes the render
/// loop when the dialog closes, to re-enable the buttons.
fn pick_folder(
    picker: Picker,
    start: Option<PathBuf>,
    picking: Arc<AtomicBool>,
    commands: Sender<ViewerCommand>,
    waker: Waker,
) {
    picking.store(true, Ordering::Relaxed);
    std::thread::spawn(move || {
        let mut dialog = rfd::FileDialog::new().set_title(picker.title());
        if let Some(start) = start {
            dialog = dialog.set_directory(start);
        }
        if let Some(folder) = dialog.pick_folder() {
            let _ = commands.send(picker.command(folder));
        }
        picking.store(false, Ordering::Relaxed);
        let _ = waker.send_event(());
    });
}

/// Heading for the file list: the folder's name, or the whole path for a drive root.
fn folder_label(path: &Path) -> String {
    match path.file_name() {
        Some(name) => name.to_string_lossy().into_owned(),
        None => path.display().to_string(),
    }
}

/// Folders as collapsible headers, files as rows that open the model on click.
fn file_tree(ui: &mut egui::Ui, entries: &[FileEntry], open: Option<&Path>, commands: &Sender<ViewerCommand>) {
    for entry in entries {
        match entry {
            FileEntry::Dir { name, children } => {
                egui::CollapsingHeader::new(name.as_str())
                    .default_open(contains(children, open))
                    .show(ui, |ui| file_tree(ui, children, open, commands));
            }
            FileEntry::File { name, path } => {
                if ui.selectable_label(open == Some(path.as_path()), name.as_str()).clicked() {
                    let _ = commands.send(ViewerCommand::OpenModel(path.clone()));
                }
            }
        }
    }
}

/// Whether `open` is somewhere in `entries`, so its folders start expanded.
fn contains(entries: &[FileEntry], open: Option<&Path>) -> bool {
    let Some(open) = open else { return false };
    entries.iter().any(|entry| match entry {
        FileEntry::Dir { children, .. } => contains(children, Some(open)),
        FileEntry::File { path, .. } => path == open,
    })
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
    folder: Option<FolderListing>,
    /// The model shown, highlighted in the file list.
    model: Option<PathBuf>,
    /// The last command outcome (e.g. "Wrote outputs/cube_print.3mf").
    notice: Option<Diagnostic>,
    /// As shown in the settings window; the app saves every change.
    settings: Settings,
}

impl ViewState {
    fn receive(&mut self, message: ViewerMessage) {
        match message {
            ViewerMessage::Scene(update) => self.apply(update),
            ViewerMessage::Folder(listing) => self.folder = Some(listing),
            ViewerMessage::Model(model) => {
                if model != self.model {
                    // A different model: drop the old meshes and frame the next ones.
                    self.assembled = None;
                    self.print = None;
                    self.framed = None;
                    self.diagnostics.clear();
                    self.loading = None;
                    self.notice = None;
                }
                self.model = model;
            }
            ViewerMessage::Notice(notice) => self.notice = Some(notice),
            ViewerMessage::Settings(settings) => self.settings = settings,
        }
    }

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
            if let Some(edges) = scene.edges.as_ref().filter(|_| self.settings.show_edges) {
                objects.push(edges);
            }
            if let Some(plates) = &scene.plates {
                objects.push(plates);
            }
        }
        objects
    }
}

/// The part of the window right of the files panel, where the model is drawn
/// and centered. `panel_right` is in egui points; viewports are in physical
/// pixels. At least one pixel column is always left.
fn scene_viewport(window: Viewport, panel_right: f32, device_pixel_ratio: f32) -> Viewport {
    let left = ((panel_right * device_pixel_ratio).round().max(0.0) as u32).min(window.width.saturating_sub(1));
    Viewport {
        x: window.x + left as i32,
        y: window.y,
        width: window.width - left,
        height: window.height,
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

    #[test]
    fn sleeps_between_events_unless_building_or_animating() {
        assert!(!keep_drawing(false, false));
        assert!(keep_drawing(true, false));
        assert!(keep_drawing(false, true));
    }

    #[test]
    fn window_takes_initial_settings_for_antialiasing() {
        let off = Settings {
            antialiasing: false,
            ..Default::default()
        };
        let initial = [
            ViewerMessage::Scene(SceneUpdate::loading("Loading x.ts…")),
            ViewerMessage::Settings(off),
        ];
        assert_eq!(initial_settings(&initial), off);
        assert_eq!(surface_settings(&off).multisamples, 0);
        assert_eq!(initial_settings(&[]), Settings::default());
        assert_eq!(surface_settings(&Settings::default()).multisamples, 4);
    }

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

    fn file(path: &str) -> FileEntry {
        let path = PathBuf::from(path);
        FileEntry::File {
            name: path.file_name().unwrap().to_string_lossy().into_owned(),
            path,
        }
    }

    fn dir(name: &str, children: Vec<FileEntry>) -> FileEntry {
        FileEntry::Dir {
            name: name.into(),
            children,
        }
    }

    #[test]
    fn folders_holding_the_open_model_start_expanded() {
        let tree = vec![
            dir("models", vec![dir("parts", vec![file("/p/models/parts/leg.ts")]), file("/p/models/a.ts")]),
            file("/p/b.ts"),
        ];
        let FileEntry::Dir { children, .. } = &tree[0] else { unreachable!() };
        assert!(contains(children, Some(Path::new("/p/models/parts/leg.ts"))));
        assert!(contains(children, Some(Path::new("/p/models/a.ts"))));
        assert!(!contains(children, Some(Path::new("/p/b.ts"))));
        assert!(!contains(children, None));
    }

    #[test]
    fn pickers_send_their_own_command() {
        let folder = PathBuf::from("/p/new");
        assert_eq!(Picker::Open.command(folder.clone()), ViewerCommand::OpenFolder(folder.clone()));
        assert_eq!(Picker::NewProject.command(folder.clone()), ViewerCommand::NewProject(folder));
    }

    #[test]
    fn folder_label_is_the_folder_name() {
        assert_eq!(folder_label(Path::new("/home/me/shelves")), "shelves");
        assert_eq!(folder_label(Path::new("/")), "/");
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
    fn scene_viewport_starts_right_of_the_files_panel() {
        let window = Viewport {
            x: 0,
            y: 0,
            width: 1280,
            height: 800,
        };
        let scene = scene_viewport(window, 220.0, 1.0);
        assert_eq!((scene.x, scene.y, scene.width, scene.height), (220, 0, 1060, 800));

        // Points scale to pixels on high-DPI screens.
        let hidpi = Viewport { width: 2560, height: 1600, ..window };
        let scene = scene_viewport(hidpi, 220.4, 2.0);
        assert_eq!((scene.x, scene.width, scene.height), (441, 2119, 1600));

        // No panel yet, or a panel dragged wider than the window.
        assert_eq!(scene_viewport(window, 0.0, 1.0), window);
        assert_eq!(scene_viewport(window, 5000.0, 1.0).width, 1);
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
