// XXX check for unused deps
use anyhow::{anyhow, Result};
use askama::Template;
use axum::{
    body::StreamBody,
    debug_handler,
    extract::{Path, State},
    http::header,
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
use tokio_util::io::ReaderStream;
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
    // XXX line numbers in errors
    for line in reader.lines() {
        let line = line?;
        if !line.starts_with('\t') {
            // A line without a tab means a new group
            if !group.is_empty() {
                groups.push(group);
            }
            group = Vec::new();
        }

        let caps = line_re
            .captures(&line)
            .ok_or_else(|| anyhow!("Line does not match expected format: {}", line))?;

        let Some(path_cap) = caps.get(3) else { 
            return Err(anyhow!("Missing path on line: {}", line)) 
        };
        let Some(width_cap) = caps.get(1) else { 
            return Err(anyhow!("Missing width on line: {}", line))
        };
        let Some(height_cap) = caps.get(2) else { 
            return Err(anyhow!("Missing height on line: {}", line)) 
        };

        // XXX customize errors for failed parsing
        let info = ImgInfo {
            path: path_cap.as_str().to_string(),
            width: width_cap.as_str().parse()?,
            height: height_cap.as_str().parse()?,
        };
        group.push(info);
    }
    if !group.is_empty() {
        groups.push(group);
    }
    Ok(groups)
}

// XXX gracefully handle errors in main
// XXX avoid all the unwraps
#[tokio::main]
async fn main() {
    let args = Args::parse();
    let base_dir = std::path::Path::new(&args.filename).parent().unwrap();
    let trash_dir = base_dir.join("trash");

    let dups = parse_dups(&args.filename).unwrap();
    // XXX bail if no dups

    let state = Arc::new(AppState {
        dups,
        base_dir: base_dir.to_path_buf(),
        trash_dir,
    });

    let mut app = Router::new()
        .route("/", get(|| async { Redirect::permanent("/group/0") }))
        .route("/group/:group_idx", get(group).with_state(Arc::clone(&state)))
        .route(
            "/group/:group_idx/image/:image_idx",
            get(get_image).with_state(Arc::clone(&state)),
        )
        .route(
            "/group/:group_idx/image/:image_idx",
            delete(trash_image).with_state(Arc::clone(&state)),
        );

    // Add a route for each image
    // XXX handle with a single handler
    for img in state.dups.iter().flatten() {
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
async fn group(Path(group_idx): Path<usize>, State(state): State<Arc<AppState>>) -> Response {
    let Some(group) = state.dups.get(group_idx) else {
        return Redirect::to("/group/0").into_response();
    };

    let template = GroupTemplate {
        group_idx,
        is_next_group: group_idx < state.dups.len() - 1,
        group: group.to_vec(),
    };
    HtmlTemplate(template).into_response()
}

#[derive(Template)]
#[template(path = "group.html")]
struct GroupTemplate {
    group_idx: usize,
    is_next_group: bool,
    group: DupGroup,
}

#[debug_handler]
async fn get_image(
    Path((group_idx, image_idx)): Path<(usize, usize)>,
    State(state): State<Arc<AppState>>,
) -> Response {
    // XXX let Some (or extract common helper)
    let group = match state.dups.get(group_idx) {
        Option::Some(group) => group,
        Option::None => {
            return (StatusCode::NOT_FOUND, "Invalid group index".to_string()).into_response();
        }
    };

    let image = match group.get(image_idx) {
        Option::Some(image) => image,
        Option::None => {
            return (StatusCode::NOT_FOUND, "Invalid image index".to_string()).into_response();
        }
    };

    let source_path = state.base_dir.join(&image.path);

    // `File` implements `AsyncRead`
    let file = match tokio::fs::File::open(source_path).await {
        Ok(file) => file,
        Err(err) => {
            return (StatusCode::NOT_FOUND, format!("File not found: {}", err)).into_response();
        }
    };

    let stream = ReaderStream::new(file);
    let body = StreamBody::new(stream);

    let content_type = mime_guess::from_path(&image.path)
        .first_raw()
        .unwrap_or("application/octet-stream");

    // XXX include size header?
    let headers = [
        (header::CONTENT_TYPE, content_type),
    ];
    (headers, body).into_response()
}

#[debug_handler]
async fn trash_image(
    Path((group_idx, image_idx)): Path<(usize, usize)>,
    State(state): State<Arc<AppState>>,
) -> (StatusCode, String) {

    let Some(group) = state.dups.get(group_idx) else {
        return (StatusCode::NOT_FOUND, "Invalid group index".to_string());
    };

    let Some(image) = group.get(image_idx) else {
        return (StatusCode::NOT_FOUND, "Invalid image index".to_string());
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
