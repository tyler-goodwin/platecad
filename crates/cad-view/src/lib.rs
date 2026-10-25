use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

use cad_ir::{Diagnostic, TessellatedScene, Views};
use thiserror::Error;

/// Tessellated assembled view plus the optional print layout.
pub type SceneViews = Views<TessellatedScene>;

/// One evaluation result pushed from the reload thread to the viewer.
#[derive(Debug, Clone, Default)]
pub struct SceneUpdate {
    /// New meshes to show; `None` keeps the last good ones (e.g. after a script error).
    pub views: Option<SceneViews>,
    pub diagnostics: Vec<Diagnostic>,
    /// Set when an evaluation is starting (e.g. "Loading model.ts…"). The
    /// viewer shows it with a timer until the result arrives, keeping the
    /// current meshes and diagnostics meanwhile.
    pub loading: Option<String>,
}

impl SceneUpdate {
    pub fn ok(views: SceneViews, message: impl Into<String>) -> Self {
        Self {
            views: Some(views),
            diagnostics: vec![Diagnostic::ok(message)],
            loading: None,
        }
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self {
            views: None,
            diagnostics: vec![Diagnostic::error(message)],
            loading: None,
        }
    }

    pub fn loading(message: impl Into<String>) -> Self {
        Self {
            views: None,
            diagnostics: Vec::new(),
            loading: Some(message.into()),
        }
    }
}

/// Overlay text for an evaluation that has been running for `elapsed`.
pub fn loading_status(message: &str, elapsed: Duration) -> String {
    format!("{message} {:.1} s", elapsed.as_secs_f32())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ViewMode {
    #[default]
    Assembled,
    Print,
}

/// Which view the user asked for vs. which one can be shown. Asking for Print on
/// a model without a print layout shows Assembled, and the request is kept so
/// Print comes back by itself once a reload adds one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ViewSelection {
    pub requested: ViewMode,
}

impl ViewSelection {
    pub fn active(self, has_print: bool) -> ViewMode {
        match self.requested {
            ViewMode::Print if has_print => ViewMode::Print,
            _ => ViewMode::Assembled,
        }
    }

    /// The scene to draw for this selection.
    pub fn scene(self, views: &SceneViews) -> &TessellatedScene {
        match (self.active(views.print.is_some()), &views.print) {
            (ViewMode::Print, Some(print)) => print,
            _ => &views.assembled,
        }
    }
}

#[derive(Debug, Error)]
pub enum ViewError {
    #[error("viewer failed: {0}")]
    Message(String),
}

impl ViewMode {
    /// Lowercase name, as used in export file names.
    pub fn label(self) -> &'static str {
        match self {
            ViewMode::Assembled => "assembled",
            ViewMode::Print => "print",
        }
    }
}

/// A folder or `.ts` file in the opened folder's file browser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileEntry {
    Dir { name: String, children: Vec<FileEntry> },
    File { name: String, path: PathBuf },
}

/// The opened folder and the model files under it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderListing {
    pub path: PathBuf,
    pub entries: Vec<FileEntry>,
}

/// What the viewer asks the app to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewerCommand {
    /// Browse this folder and hot-reload from it. Closes the open model.
    OpenFolder(PathBuf),
    /// Load this model and watch its dependencies.
    OpenModel(PathBuf),
    /// Write this view (the one on screen) to a 3MF file.
    Export(ViewMode),
}

/// What the app tells the viewer.
#[derive(Debug, Clone)]
pub enum ViewerMessage {
    Scene(SceneUpdate),
    /// A folder was opened, or model files were added or removed under it.
    Folder(FolderListing),
    /// The model now shown (`None`: none). The viewer drops the old meshes,
    /// and frames the new ones when they arrive.
    Model(Option<PathBuf>),
    /// The outcome of a one-off command, such as an export.
    Notice(Diagnostic),
}

impl From<SceneUpdate> for ViewerMessage {
    fn from(update: SceneUpdate) -> Self {
        Self::Scene(update)
    }
}

/// A windowed viewport. Blocks the calling (main) thread until the window
/// closes, polling `messages` each frame and sending user actions on `commands`.
pub trait ViewportApp {
    fn run(
        self,
        initial: Vec<ViewerMessage>,
        messages: Receiver<ViewerMessage>,
        commands: Sender<ViewerCommand>,
    ) -> Result<(), ViewError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scene(marker: f32) -> TessellatedScene {
        TessellatedScene {
            positions: vec![[marker; 3]],
            ..Default::default()
        }
    }

    fn select(requested: ViewMode) -> ViewSelection {
        ViewSelection { requested }
    }

    #[test]
    fn loading_updates_carry_no_result() {
        let update = SceneUpdate::loading("Loading cube.ts…");
        assert_eq!(update.loading.as_deref(), Some("Loading cube.ts…"));
        assert!(update.views.is_none() && update.diagnostics.is_empty());
        assert!(SceneUpdate::error("x").loading.is_none());
    }

    #[test]
    fn loading_status_shows_elapsed_seconds() {
        assert_eq!(
            loading_status("Loading cube.ts…", Duration::from_millis(2340)),
            "Loading cube.ts… 2.3 s"
        );
        assert_eq!(loading_status("Rebuilding x.ts…", Duration::ZERO), "Rebuilding x.ts… 0.0 s");
    }

    #[test]
    fn defaults_to_assembled() {
        assert_eq!(ViewSelection::default().active(true), ViewMode::Assembled);
    }

    #[test]
    fn print_shown_only_when_available() {
        assert_eq!(select(ViewMode::Print).active(true), ViewMode::Print);
        assert_eq!(select(ViewMode::Print).active(false), ViewMode::Assembled);
        assert_eq!(select(ViewMode::Assembled).active(true), ViewMode::Assembled);
    }

    #[test]
    fn scene_follows_active_view() {
        let both = SceneViews {
            assembled: scene(1.0),
            print: Some(scene(2.0)),
        };
        let assembled_only = SceneViews::assembled_only(scene(1.0));

        assert_eq!(select(ViewMode::Assembled).scene(&both), &scene(1.0));
        assert_eq!(select(ViewMode::Print).scene(&both), &scene(2.0));
        assert_eq!(select(ViewMode::Print).scene(&assembled_only), &scene(1.0));
    }
}
