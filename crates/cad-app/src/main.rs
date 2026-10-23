use std::process::ExitCode;
use std::sync::mpsc;

use cad_app::{Args, Pipeline, project_root, report, resolve_model, watch_and_reload};
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
    let initial = pipeline.evaluate(&model);
    report(&initial);

    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        if let Err(e) = watch_and_reload(&model, pipeline, tx.clone()) {
            let _ = tx.send(SceneUpdate::error(format!("file watcher stopped: {e}")));
        }
    });

    if let Err(e) = ThreeDViewport::default().run(initial, rx) {
        eprintln!("error: {e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
