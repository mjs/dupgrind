use anyhow::{anyhow, Result};
use askama::Template;
use axum::{
    debug_handler,
    extract::{Path, State},
    http::StatusCode,
    response::{Html, IntoResponse, Redirect, Response},
    routing::{delete, get},
    Router,
};
use clap::Parser;
use regex::Regex;
use std::fs;
use std::io::{BufRead, BufReader};
use std::sync::Arc;
use tower_http::services::{ServeDir, ServeFile};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    // The file containing the output from photodedupe to process
    filename: String,
}

#[derive(Debug, Clone)]
struct ImgInfo {
    path: String,
    width: u32,
    height: u32,
}

type DupGroup = Vec<ImgInfo>;

#[derive(Debug, Clone)]
struct AppState {
    dups: Vec<DupGroup>,
    base_dir: std::path::PathBuf,
    trash_dir: std::path::PathBuf,
}

fn parse_dups(filename: &str) -> Result<Vec<DupGroup>> {
    // XXX pre-compile
    let line_re = Regex::new(r"^\s*\w+\((\d+)x(\d+)\): (.+)")?;

    // XXX guess initial size?
    let mut groups = Vec::new();

    let reader = BufReader::new(fs::File::open(filename)?);
    let mut group = Vec::new();
    for line in reader.lines() {
        let line = line?;
        if !line.starts_with('\t') {
            if !group.is_empty() {
                groups.push(group);
            }
            group = Vec::new();
        }

        let caps = line_re
            .captures(&line)
            .ok_or_else(|| anyhow!("Line does not match expected format: {}", line))?;

        // XXX clean up unwraps here
        let info = ImgInfo {
            path: caps.get(3).unwrap().as_str().to_string(),
            width: caps.get(1).unwrap().as_str().parse()?,
            height: caps.get(2).unwrap().as_str().parse()?,
        };
        group.push(info);
    }
    if !group.is_empty() {
        groups.push(group);
    }
    Ok(groups)
}

// XXX gracefully handle errors in main
#[tokio::main]
async fn main() {
    let args = Args::parse();
    let base_dir = std::path::Path::new(&args.filename).parent().unwrap();
    let trash_dir = base_dir.join("trash");

    let dups = parse_dups(&args.filename).unwrap();
    // XXX bail if no dups

    // XXX avoid clones?
    let state = Arc::new(AppState {
        dups: dups.clone(),
        base_dir: base_dir.to_path_buf(),
        trash_dir: trash_dir.clone(),
    });

    let mut app = Router::new()
        .route("/", get(|| async { Redirect::permanent("/group/0") }))
        .route("/group/:index", get(group).with_state(Arc::clone(&state)))
        .route(
            "/group/:group_idx/image/:image_idx",
            delete(trash_image).with_state(Arc::clone(&state)),
        );

    // Add a route for each image
    // XXX handle with a single handler
    for img in dups.iter().flatten() {
        app = app.nest_service(
            format!("/image/{}", &img.path).as_str(),
            ServeDir::new(base_dir.join(&img.path).as_os_str())
                .not_found_service(ServeFile::new("assets/missing.png")),
        );
    }

    axum::Server::bind(&"0.0.0.0:3000".parse().unwrap())
        .serve(app.into_make_service())
        .await
        .unwrap();
}

#[debug_handler]
async fn group(Path(index): Path<usize>, State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let template = GroupTemplate {
        index,
        next_group: index < state.dups.len() - 1,
        group: state.dups.get(index).unwrap().to_vec(),
    };
    HtmlTemplate(template)
}

#[derive(Template)]
#[template(path = "group.html")]
struct GroupTemplate {
    index: usize,
    next_group: bool,
    group: DupGroup,
}

#[debug_handler]
async fn trash_image(
    Path((group_idx, image_idx)): Path<(usize, usize)>,
    State(state): State<Arc<AppState>>,
) -> (StatusCode, String) {
    let group = match state.dups.get(group_idx) {
        Option::Some(group) => group,
        Option::None => {
            return (StatusCode::NOT_FOUND, "Invalid group index".to_string());
        }
    };

    let image = match group.get(image_idx) {
        Option::Some(image) => image,
        Option::None => {
            return (StatusCode::NOT_FOUND, "Invalid image index".to_string());
        }
    };

    let source_path = state.base_dir.join(&image.path);
    let target_path = state.trash_dir.join(&image.path);

    // Ensure that destination directory exists
    // XXX deal with unwrap
    match fs::create_dir_all(target_path.parent().unwrap()) {
        Ok(_) => (),
        Err(err) => return (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()),
    }

    // XXX deal with unwrap
    // XXX This doesn't work for cross file system moves
    fs::rename(source_path, target_path).unwrap();

    (StatusCode::OK, "Deleted".to_string())
}

struct HtmlTemplate<T>(T);

impl<T> IntoResponse for HtmlTemplate<T>
where
    T: Template,
{
    fn into_response(self) -> Response {
        match self.0.render() {
            Ok(html) => Html(html).into_response(),
            Err(err) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to render template. Error: {}", err),
            )
                .into_response(),
        }
    }
}
