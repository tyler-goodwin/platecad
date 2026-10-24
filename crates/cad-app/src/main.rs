use std::process::ExitCode;
use std::sync::mpsc;

use cad_app::{Args, Pipeline, export_3mf, load_and_watch, project_root, resolve_model};
use cad_view::{SceneUpdate, ViewportApp};
use cad_view_threed::ThreeDViewport;
use clap::Parser;

fn main() -> ExitCode {
    let args = Args::parse();
    let cwd = std::env::current_dir().expect("current dir");

    let model = match resolve_model(&args.model, &cwd) {
        Ok(model) => model,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
    };
    let root = project_root(&model);

    let mut pipeline = match Pipeline::new(&root, args.kernel) {
        Ok(pipeline) => pipeline,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    if let Some(out) = &args.export {
        return match export_3mf(&mut pipeline, &model, args.view, out) {
            Ok(summary) => {
                println!("{summary}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::FAILURE
            }
        };
    }

    // Open the window right away; the model loads on the reload thread.
    let name = model.file_name().unwrap_or_default().to_string_lossy();
    let initial = SceneUpdate::loading(format!("Loading {name}…"));

    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        if let Err(e) = load_and_watch(&model, pipeline, tx.clone()) {
            let _ = tx.send(SceneUpdate::error(format!("file watcher stopped: {e}")));
        }
    });

    if let Err(e) = ThreeDViewport::default().run(initial, rx) {
        eprintln!("error: {e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
