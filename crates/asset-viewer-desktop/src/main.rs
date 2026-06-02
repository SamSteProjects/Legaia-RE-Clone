use anyhow::{Context, Result};
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Component, Path, PathBuf};
use std::thread;
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoop};
use tao::window::WindowBuilder;
use wry::WebViewBuilder;

#[cfg(windows)]
#[link(name = "advapi32")]
unsafe extern "C" {}

fn main() -> Result<()> {
    let repo = repo_root()?;
    ensure_site_generated(&repo)?;

    let site_root = repo.join("site");
    let listener = TcpListener::bind("127.0.0.1:0").context("bind local asset server")?;
    let page = selected_page();
    let url = format!("http://{}/{}", listener.local_addr()?, page);
    start_static_server(listener, site_root);

    let event_loop = EventLoop::new();
    let window = WindowBuilder::new()
        .with_title(window_title(page))
        .with_inner_size(tao::dpi::LogicalSize::new(1280.0, 860.0))
        .with_min_inner_size(tao::dpi::LogicalSize::new(960.0, 640.0))
        .build(&event_loop)
        .context("create desktop window")?;

    let _webview = WebViewBuilder::new()
        .with_url(&url)
        .build(&window)
        .context("create WebView2 viewer")?;

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        if let Event::WindowEvent {
            event: WindowEvent::CloseRequested,
            ..
        } = event
        {
            *control_flow = ControlFlow::Exit;
        }
    });
}

fn selected_page() -> &'static str {
    if std::env::current_exe()
        .ok()
        .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
        .is_some_and(|stem| stem.contains("seq-studio"))
    {
        return "seq-studio.html";
    }

    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "viewer" | "--viewer" => return "viewer.html",
            "seq" | "--seq" | "seq-studio" | "--seq-studio" => return "seq-studio.html",
            "characters" | "--characters" | "character" | "--character" => {
                return "characters.html";
            }
            "media" | "--media" => return "media.html",
            "world" | "--world" => return "world.html",
            "world-overview" | "--world-overview" => return "world-overview.html",
            _ => {}
        }
    }

    "index.html"
}

fn window_title(page: &str) -> &'static str {
    match page {
        "viewer.html" => "Legaia Asset Viewer",
        "seq-studio.html" => "Legaia SEQ Studio",
        "characters.html" => "Legaia Character Viewer",
        _ => "Legaia RE Desktop Suite",
    }
}

fn repo_root() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("current exe path")?;
    let release_dir = exe.parent().context("exe has no parent")?;
    let target_dir = release_dir.parent().context("release dir has no parent")?;
    let repo = target_dir.parent().context("target dir has no parent")?;
    if repo.join("site").is_dir() {
        return Ok(repo.to_path_buf());
    }
    std::env::current_dir().context("current directory")
}

fn ensure_site_generated(repo: &Path) -> Result<()> {
    let viewer = repo.join("site/viewer.html");
    if viewer.exists() {
        return Ok(());
    }

    let status = std::process::Command::new("python")
        .arg("site/_gen.py")
        .current_dir(repo)
        .env("PYTHONUTF8", "1")
        .status()
        .context("run site generator")?;
    if !status.success() {
        anyhow::bail!("site generator failed with {status}");
    }
    Ok(())
}

fn start_static_server(listener: TcpListener, site_root: PathBuf) {
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let root = site_root.clone();
            thread::spawn(move || {
                let _ = handle_request(stream, &root);
            });
        }
    });
}

fn handle_request(mut stream: TcpStream, site_root: &Path) -> Result<()> {
    let mut buf = [0u8; 8192];
    let n = stream.read(&mut buf)?;
    let req = String::from_utf8_lossy(&buf[..n]);
    let mut parts = req.lines().next().unwrap_or_default().split_whitespace();
    let method = parts.next().unwrap_or_default();
    let raw_path = parts.next().unwrap_or("/");

    if method != "GET" && method != "HEAD" {
        return respond(
            &mut stream,
            405,
            "text/plain",
            b"method not allowed",
            method,
        );
    }

    let Some(path) = resolve_path(site_root, raw_path) else {
        return respond(&mut stream, 404, "text/plain", b"not found", method);
    };

    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(_) => return respond(&mut stream, 404, "text/plain", b"not found", method),
    };
    respond(&mut stream, 200, mime_for(&path), &bytes, method)
}

fn resolve_path(site_root: &Path, raw_path: &str) -> Option<PathBuf> {
    let url_path = raw_path.split('?').next().unwrap_or("/");
    let rel = url_path.trim_start_matches('/');
    let rel = if rel.is_empty() { "index.html" } else { rel };
    let mut clean = PathBuf::new();
    for component in Path::new(rel).components() {
        match component {
            Component::Normal(part) => clean.push(part),
            _ => return None,
        }
    }
    let path = site_root.join(clean);
    if path.is_file() { Some(path) } else { None }
}

fn respond(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
    method: &str,
) -> Result<()> {
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "OK",
    };
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nContent-Type: {content_type}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    if method != "HEAD" {
        stream.write_all(body)?;
    }
    Ok(())
}

fn mime_for(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
    {
        "html" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "wasm" => "application/wasm",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "mp4" => "video/mp4",
        _ => "application/octet-stream",
    }
}
