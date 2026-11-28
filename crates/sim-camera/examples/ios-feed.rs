//! Serves a camera source to one iOS simulator's capture shim until stdin closes; each line read
//! from stdin relinks the feed to that source id.
//!
//! ```text
//! cargo run -p simee-camera --example ios-feed -- <socket> <source> [WIDTHxHEIGHT@FPS]
//! ```

#[cfg(unix)]
fn main() -> Result<(), String> {
    use std::io::BufRead;
    use std::sync::Arc;

    use simee_camera::{FrameSpec, HostOpener, Hub, SourceSpec, feed};

    let args: Vec<String> = std::env::args().skip(1).collect();
    let [socket, source, rest @ ..] = args.as_slice() else {
        return Err("usage: ios-feed <socket> <source> [WIDTHxHEIGHT@FPS]".into());
    };
    let spec = match rest.first() {
        Some(s) => parse_spec(s)?,
        None => FrameSpec::new(720, 960, 30).map_err(|e| e.to_string())?,
    };
    let source = SourceSpec::parse(source).map_err(|e| e.to_string())?;
    let hub = Arc::new(Hub::new(HostOpener::new()));
    let feed = feed::Feed::serve(hub, socket, source.clone(), spec).map_err(|e| e.to_string())?;
    println!("serving {source} at {}x{}@{} on {}", spec.width, spec.height, spec.fps, feed.path().display());
    for line in std::io::stdin().lock().lines() {
        let line = line.map_err(|e| e.to_string())?;
        let id = line.trim();
        if id.is_empty() {
            continue;
        }
        match SourceSpec::parse(id) {
            Ok(source) => {
                let line = source.to_string();
                println!("generation {}: {line}", feed.set_source(source));
            }
            Err(e) => eprintln!("{id}: {e}"),
        }
    }
    Ok(())
}

#[cfg(unix)]
fn parse_spec(s: &str) -> Result<simee_camera::FrameSpec, String> {
    let bad = || format!("{s}: expected WIDTHxHEIGHT@FPS");
    let (size, fps) = s.split_once('@').ok_or_else(bad)?;
    let (w, h) = size.split_once('x').ok_or_else(bad)?;
    let num = |v: &str| v.parse::<u32>().map_err(|_| bad());
    simee_camera::FrameSpec::new(num(w)?, num(h)?, num(fps)?).map_err(|e| e.to_string())
}

#[cfg(not(unix))]
fn main() {
    eprintln!("ios-feed needs Unix sockets");
}
