use anyhow::{anyhow, Result};
use axum::{response::Html, routing::get, Router};
use clap::Parser;
use regex::Regex;
use std::fs;
use tokio;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    // The file containing the output from photodedupe to process
    filename: String,
}

#[derive(Debug)]
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
    println!("{:?}", args);

    for group in parse_dups(&args.filename).unwrap() {
        for img in group {
            println!("{:?}", img);
        }
        println!("==============");
    }

    let app = Router::new().route("/", get(handler));

    axum::Server::bind(&"0.0.0.0:3000".parse().unwrap())
        .serve(app.into_make_service())
        .await
        .unwrap();
}

async fn handler() -> Html<&'static str> {
    Html("<h1>Hello, World!</h1>")
}
