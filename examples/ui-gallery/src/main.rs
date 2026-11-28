//! ui-gallery opens the window. `--page NAME` starts on a page,
//! `--shot OUT.png [--size WxH] [--do STEPS]` renders headless instead.

use std::process::ExitCode;

use stein_arena::Bump;
use ui_gallery::text::{Text, find_font};
use ui_gallery::{ARENA, Gallery, GalleryError, Page, shot};

struct Args {
    page: Page,
    shot: Option<std::path::PathBuf>,
    size: [u32; 2],
    steps: String,
}

fn args() -> Result<Args, String> {
    let mut out = Args {
        page: Page::Shapes,
        shot: None,
        size: [1280, 860],
        steps: String::new(),
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let value = it.next().ok_or_else(|| format!("{flag} wants a value"))?;
        match flag.as_str() {
            "--page" => out.page = Page::parse(&value).ok_or_else(|| format!("no page {value}"))?,
            "--shot" => out.shot = Some(value.into()),
            "--do" => out.steps = value,
            "--size" => {
                let (w, h) = value.split_once('x').ok_or("size is WxH")?;
                out.size = [w.parse().map_err(|_| "bad width")?, h.parse().map_err(|_| "bad height")?];
            }
            _ => return Err(format!("unknown flag {flag}")),
        }
    }
    Ok(out)
}

fn gallery() -> Result<Gallery, GalleryError> {
    let sans = find_font("UI_GALLERY_SANS", &["Inter", "sans-serif"]).ok_or(GalleryError::NoFont)?;
    let mono = find_font("UI_GALLERY_MONO", &["JetBrains Mono", "monospace"]).ok_or(GalleryError::NoFont)?;
    let bump: &'static Bump = Box::leak(Box::new(Bump::reserve(ARENA)?));
    Gallery::new(bump, Text::load(sans, mono)?)
}

fn main() -> ExitCode {
    let args = match args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("ui-gallery: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut gallery = match gallery() {
        Ok(g) => g,
        Err(e) => {
            eprintln!("ui-gallery: {e}");
            return ExitCode::FAILURE;
        }
    };
    gallery.demo.page = args.page;
    let result = match &args.shot {
        Some(out) => shot::parse(&args.steps)
            .and_then(|steps| shot::shot(&mut gallery, args.size, &steps, out))
            .map_err(|e| e.to_string()),
        None => stein_shell::run(Box::new(gallery)).map_err(|e| e.to_string()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ui-gallery: {e}");
            ExitCode::FAILURE
        }
    }
}
