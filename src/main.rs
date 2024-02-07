use anyhow::{anyhow, Result};
use askama::Template;
use axum::{
    body::StreamBody,
    debug_handler,
    extract::{Path, State},
    headers::{ETag, IfNoneMatch},
    http::header,
    http::StatusCode,
    response::{Html, IntoResponse, Redirect, Response},
    routing::{delete, get},
    Router,
    TypedHeader,
};
use clap::Parser;
use regex::Regex;
use log::{debug, info, error};
use sha256;
use std::fs;
use std::io::{BufRead, BufReader};
use std::sync::Arc;
use tokio_util::io::ReaderStream;
use tower_http::services::ServeDir;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    // The file containing the output from photodedupe to process
    filename: String,
}

/// The path and size of a single - potentially duplicate - image.
#[derive(Clone)]
struct ImgInfo {
    path: String,
    width: u32,
    height: u32,
}

/// A set of (potentially) duplicate images.
type DupGroup = Vec<ImgInfo>;

/// All sets of duplicate images found in a photodedupe run.
struct DupGroups {
    groups: Vec<DupGroup>,
}

impl DupGroups {
    fn new(size_guess: usize) -> Self {
        Self { groups: Vec::with_capacity(size_guess) }
    }

    fn push_group(&mut self, group: DupGroup) {
        self.groups.push(group);
    }

    fn num_groups(&self) -> usize {
        self.groups.len()
    }

    fn get_group(&self, group_idx: usize) -> Option<&DupGroup> {
        self.groups.get(group_idx)
    }

    fn get_image(&self, group_idx: usize, image_idx: usize) -> Option<&ImgInfo> {
        let Some(group) = self.groups.get(group_idx) else {
            return None;
        };
        group.get(image_idx)
    }
}

/// Shared state for passing to route handlers.
struct AppState {
    dups: DupGroups,
    base_dir: std::path::PathBuf,
    trash_dir: std::path::PathBuf,
}

fn parse_dups(filename: &str) -> Result<DupGroups> {
    let line_re = Regex::new(r"^\s*\w+\((\d+)x(\d+)\): (.+)")?;

    // XXX guess initial size?
    let mut dups = DupGroups::new(20);

    let reader = BufReader::new(fs::File::open(filename)?);
    let mut group = Vec::new();
    // XXX line numbers in errors
    for line in reader.lines() {
        let line = line?;
        if !line.starts_with('\t') {
            // A line without a tab means a new group
            if !group.is_empty() {
                dups.push_group(group);
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

        // XXX customize errors for failed int parsing
        let info = ImgInfo {
            path: path_cap.as_str().to_string(),
            width: width_cap.as_str().parse()?,
            height: height_cap.as_str().parse()?,
        };
        group.push(info);
    }
    if !group.is_empty() {
        dups.push_group(group);
    }

    dups.groups.sort_unstable_by_key(|group| group[0].path.clone());
    Ok(dups)
}

// XXX gracefully handle errors in main
// XXX avoid all the unwraps
#[tokio::main]
async fn main() {
    env_logger::Builder::from_env(
        env_logger::Env::default().
            default_filter_or("info")).
        init();

    let args = Args::parse();
    // XXX make these optional
    let base_dir = std::path::Path::new(&args.filename).parent().unwrap();
    let trash_dir = base_dir.join("trash");

    info!("base dir: {}", base_dir.to_string_lossy());
    info!("trash dir: {}", trash_dir.to_string_lossy());

    let dups = parse_dups(&args.filename).unwrap();
    // XXX bail if no dups

    let state = Arc::new(AppState {
        dups,
        base_dir: base_dir.to_path_buf(),
        trash_dir,
    });

    // XXX log requests
    let app = Router::new()
        .route("/", get(|| async { Redirect::permanent("/group/0") }))
        .route("/group/:group_idx", get(group).with_state(Arc::clone(&state)))
        .route(
            "/group/:group_idx/image/:image_idx",
            get(get_image).with_state(Arc::clone(&state)),
        )
        .route(
            "/group/:group_idx/image/:image_idx",
            delete(trash_image).with_state(Arc::clone(&state)),
        // static should be cached for a bit
        ).nest_service( "/static", ServeDir::new("assets"));  // XXX package assets into binary

    // XXX port should be an arg
    axum::Server::bind(&"0.0.0.0:3000".parse().unwrap())
        .serve(app.into_make_service())
        .await
        .unwrap();
}

#[debug_handler]
async fn group(Path(group_idx): Path<usize>, State(state): State<Arc<AppState>>) -> Response {
    let Some(group) = state.dups.get_group(group_idx) else {
        return Redirect::to("/group/0").into_response();
    };

    let template = GroupTemplate {
        group_idx,
        is_next_group: group_idx < state.dups.num_groups() - 1,
        group: group.to_vec(),  // XXX likely clone, avoid
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
    TypedHeader(if_none_match): TypedHeader<IfNoneMatch>,
) -> Response {
    let Some(image) = state.dups.get_image(group_idx, image_idx) else {
        return (StatusCode::NOT_FOUND, "Invalid group or image index".to_string()).into_response();
    };

    let content_type = mime_guess::from_path(&image.path)
        .first_raw()
        .unwrap_or("application/octet-stream");

    // XXX this all feels icky
    let etag_value = format!("\"{}\"", sha256::digest(
            format!("{}:{}:{}:{}", state.base_dir.display(), group_idx, image_idx, &image.path)));
    debug!("etag: {}", etag_value);

    let mut headers = header::HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, content_type.parse().unwrap());
    headers.insert(header::ETAG, etag_value.parse().unwrap());

    let Ok(etag) = etag_value.parse::<ETag>() else {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to parse etag: {}", etag_value)).into_response();
    };
    if !if_none_match.precondition_passes(&etag) {
        return (StatusCode::NOT_MODIFIED, headers).into_response();
    }

    let source_path = state.base_dir.join(&image.path);

    // `File` implements `AsyncRead`
    let Ok(file) = tokio::fs::File::open(source_path).await else {
        return Redirect::to("/static/missing.png").into_response();
    };

    let Ok(stat) = file.metadata().await else {
        return (StatusCode::INTERNAL_SERVER_ERROR, "Failed to stat file").into_response();
    };

    let stream = ReaderStream::new(file);
    let body = StreamBody::new(stream);

    headers.insert(header::CONTENT_LENGTH, stat.len().to_string().parse().unwrap());
    (headers, body).into_response()
}

#[debug_handler]
async fn trash_image(
    Path((group_idx, image_idx)): Path<(usize, usize)>,
    State(state): State<Arc<AppState>>,
) -> (StatusCode, String) {
    let Some(image) = state.dups.get_image(group_idx, image_idx) else {
        return (StatusCode::NOT_FOUND, "Invalid group or image index".to_string());
    };

    let source_path = state.base_dir.join(&image.path);
    let target_path = state.trash_dir.join(&image.path);

    debug!("trashing {} to {}", source_path.display(), target_path.display());

    let Some(target_parent) = target_path.parent() else {
        return (StatusCode::INTERNAL_SERVER_ERROR, "Target has no parent".to_string());
    };

    // Ensure that destination directory exists
    match fs::create_dir_all(target_parent) {
        Ok(_) => (),
        Err(err) => return (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()),
    }

    // XXX This doesn't work for cross file system moves
    match fs::rename(&source_path, &target_path) {
        Ok(_) => (),
        Err(err) => {
            error!("failed to move {} to {}: {}", source_path.display(), target_path.display(), err);
            return (StatusCode::INTERNAL_SERVER_ERROR, err.to_string());
        },
    }

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
