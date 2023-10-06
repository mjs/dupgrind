use anyhow::{anyhow, Result};
use askama::Template;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    routing::get,
    Router,
};
use clap::Parser;
use regex::Regex;
use std::fs;
use std::sync::Arc;
use tokio;
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

fn parse_dups(filename: &str) -> Result<Vec<DupGroup>> {
    let line_re = Regex::new(r"^\s*\w+\((\d+)x(\d+)\): (.+)")?;

    let mut groups = Vec::new();
    let mut group = Vec::new();

    let buf = fs::read_to_string(filename)?;
    for line in buf.lines() {
        if !line.starts_with('\t') {
            if !group.is_empty() {
                groups.push(group);
            }
            group = Vec::new();
        }

        let caps = line_re
            .captures(line)
            .ok_or_else(|| anyhow!("Invalid line: {}", line))?;

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
    return Ok(groups);
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let base_dir = std::path::Path::new(&args.filename).parent().unwrap();
    println!("base_dir: {}", base_dir.display());

    let dups = parse_dups(&args.filename).unwrap();
    let dups = Arc::new(dups);

    let mut app = Router::new()
        .route("/", get(root).with_state(Arc::clone(&dups)))
        .with_state(Arc::clone(&dups));

    // Add a route for each image
    for img in dups.iter().flatten() {
        app = app.nest_service(
            format!("/image/{}", &img.path).as_str(),
            ServeFile::new(base_dir.join(&img.path).as_os_str()),
        );
    }

    axum::Server::bind(&"0.0.0.0:3000".parse().unwrap())
        .serve(app.into_make_service())
        .await
        .unwrap();
}

async fn root(State(dups): State<Arc<Vec<DupGroup>>>) -> impl IntoResponse {
    let template = ListTemplate {
        dups: dups.to_vec(),
    };
    HtmlTemplate(template)
}

#[derive(Template)]
#[template(path = "list.html")]
struct ListTemplate {
    dups: Vec<DupGroup>,
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
