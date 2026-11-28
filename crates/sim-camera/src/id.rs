//! Source ids: `webcam:<id or part of the name>`, `pattern:testcard`, `qr:<text>`, `text:<text>`,
//! `image:<path>`, `video:<path>`, `solid:<colour>`, `device:<udid or name>[:x,y,w,h]`, `feed:<socket>`. A string with no known prefix and no `:` is a
//! webcam name, so `obs` finds "OBS Virtual Camera".

use std::fmt;
use std::path::PathBuf;

use crate::catalog::HostCamera;
use crate::device::DeviceSpec;
use crate::draw::Colour;
use crate::error::{Error, Result};

/// The test patterns `pattern:` draws.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Pattern {
    Testcard,
}

impl Pattern {
    const ALL: &'static [(&'static str, Pattern)] = &[("testcard", Pattern::Testcard)];

    pub fn name(self) -> &'static str {
        match self {
            Pattern::Testcard => "testcard",
        }
    }
}

/// A parsed source id.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum SourceSpec {
    /// A host camera, by id, exact name, index (`webcam:0`) or part of its name; resolved against
    /// the catalog with [`resolve_webcam`].
    Webcam(String),
    Pattern(Pattern),
    Qr(String),
    Text(String),
    Image(PathBuf),
    Video(PathBuf),
    /// The colour as written, so the id round-trips; parsed when the source opens (and checked
    /// when the id is parsed).
    Solid(String),
    /// Another device's screen, optionally cropped ([`crate::device`]).
    Device(DeviceSpec),
    /// A camera feed's socket ([`crate::feed`]): what the simee server serves another device's
    /// screen to emu on. emu links an Android camera's `device:` source as this.
    Feed(PathBuf),
}

const KINDS: &[&str] = &["webcam", "pattern", "qr", "text", "image", "video", "solid", "device", "feed"];

impl SourceSpec {
    pub fn parse(id: &str) -> Result<SourceSpec> {
        let id = id.trim();
        if id.is_empty() {
            return Err(Error::Invalid(format!("empty source id; use one of {}", examples())));
        }
        let Some((kind, rest)) = id.split_once(':') else {
            return Ok(SourceSpec::Webcam(id.to_string()));
        };
        let nonempty = |what: &str| {
            if rest.is_empty() { Err(Error::Invalid(format!("{kind}: needs {what}, as in {kind}:{}", example(kind)))) } else { Ok(()) }
        };
        let spec = match kind.to_ascii_lowercase().as_str() {
            "webcam" => {
                nonempty("a camera id or part of its name")?;
                SourceSpec::Webcam(rest.to_string())
            }
            "pattern" => {
                let pattern = Pattern::ALL.iter().find(|(n, _)| n.eq_ignore_ascii_case(rest)).map(|&(_, p)| p);
                let names: Vec<&str> = Pattern::ALL.iter().map(|(n, _)| *n).collect();
                SourceSpec::Pattern(
                    pattern.ok_or_else(|| Error::Invalid(format!("unknown pattern {rest:?}; patterns: {}", names.join(", "))))?,
                )
            }
            "qr" => {
                nonempty("the text to encode")?;
                SourceSpec::Qr(rest.to_string())
            }
            "text" => {
                nonempty("the text to show")?;
                SourceSpec::Text(rest.to_string())
            }
            "image" => {
                nonempty("a PNG or JPEG path")?;
                SourceSpec::Image(PathBuf::from(rest))
            }
            "video" => {
                nonempty("a video file path")?;
                SourceSpec::Video(PathBuf::from(rest))
            }
            "solid" => {
                nonempty("a colour")?;
                Colour::parse(rest)?;
                SourceSpec::Solid(rest.to_string())
            }
            "device" => SourceSpec::Device(DeviceSpec::parse(rest)?),
            "feed" => {
                if !rest.starts_with('/') {
                    return Err(Error::Invalid(format!("feed: needs a camera feed's absolute socket path, as in feed:{}", example(kind))));
                }
                SourceSpec::Feed(PathBuf::from(rest))
            }
            _ => {
                return Err(Error::Invalid(format!(
                    "unknown source kind {kind:?}; use one of {} (a camera named with a colon: webcam:<name>)",
                    examples()
                )));
            }
        };
        Ok(spec)
    }

    /// The kind prefix (`webcam`, `qr`, …).
    pub fn kind(&self) -> &'static str {
        match self {
            SourceSpec::Webcam(_) => "webcam",
            SourceSpec::Pattern(_) => "pattern",
            SourceSpec::Qr(_) => "qr",
            SourceSpec::Text(_) => "text",
            SourceSpec::Image(_) => "image",
            SourceSpec::Video(_) => "video",
            SourceSpec::Solid(_) => "solid",
            SourceSpec::Device(_) => "device",
            SourceSpec::Feed(_) => "feed",
        }
    }
}

impl std::str::FromStr for SourceSpec {
    type Err = Error;
    fn from_str(s: &str) -> Result<SourceSpec> {
        SourceSpec::parse(s)
    }
}

impl fmt::Display for SourceSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SourceSpec::Webcam(q) => write!(f, "webcam:{q}"),
            SourceSpec::Pattern(p) => write!(f, "pattern:{}", p.name()),
            SourceSpec::Qr(t) => write!(f, "qr:{t}"),
            SourceSpec::Text(t) => write!(f, "text:{t}"),
            SourceSpec::Image(p) => write!(f, "image:{}", p.display()),
            SourceSpec::Video(p) => write!(f, "video:{}", p.display()),
            SourceSpec::Solid(c) => write!(f, "solid:{c}"),
            SourceSpec::Device(d) => write!(f, "device:{d}"),
            SourceSpec::Feed(p) => write!(f, "feed:{}", p.display()),
        }
    }
}

fn example(kind: &str) -> &'static str {
    match kind {
        "device" => return "<udid or name>",
        "feed" => return "/path/to/cam.sock",
        _ => {}
    }
    crate::catalog::SYNTHETIC
        .iter()
        .find(|k| k.prefix == kind)
        .and_then(|k| k.example.split_once(':'))
        .map(|(_, e)| e)
        .unwrap_or("<camera name>")
}

fn examples() -> String {
    KINDS.iter().map(|k| format!("{k}:{}", example(k))).collect::<Vec<_>>().join(", ")
}

/// Lowercase letters and digits only, so "obs virtual", "OBS-Virtual" and "obsvirtual" all match
/// "OBS Virtual Camera".
fn normalized(s: &str) -> String {
    s.chars().filter(char::is_ascii_alphanumeric).map(|c| c.to_ascii_lowercase()).collect()
}

/// Finds the camera a `webcam:` query names, trying in order: the exact id; the exact name
/// (ignoring case); an index into the list (`0`, as stock emulator's `webcam0`); then the name or
/// id containing the query, ignoring case, spaces and punctuation. More than one match at the
/// first step that matches anything is an [`Error::Ambiguous`] listing them.
pub fn resolve_webcam<'a>(query: &str, cameras: &'a [HostCamera]) -> Result<&'a HostCamera> {
    let q = query.trim();
    let q = q.strip_prefix("webcam:").unwrap_or(q);
    if let Some(c) = cameras.iter().find(|c| c.id == q) {
        return Ok(c);
    }
    let one = |matches: Vec<&'a HostCamera>| -> Option<Result<&'a HostCamera>> {
        match matches.as_slice() {
            [] => None,
            [c] => Some(Ok(*c)),
            many => Some(Err(Error::Ambiguous {
                query: query.to_string(),
                matches: many.iter().map(|c| format!("{} ({})", c.name, c.id)).collect(),
            })),
        }
    };
    if let Some(r) = one(cameras.iter().filter(|c| c.name.eq_ignore_ascii_case(q)).collect()) {
        return r;
    }
    let index = q.strip_prefix("webcam").unwrap_or(q);
    if !index.is_empty()
        && index.chars().all(|c| c.is_ascii_digit())
        && let Some(c) = index.parse::<usize>().ok().and_then(|i| cameras.get(i))
    {
        return Ok(c);
    }
    let nq = normalized(q);
    if !nq.is_empty() {
        let hits = cameras.iter().filter(|c| normalized(&c.name).contains(&nq) || normalized(&c.id).contains(&nq)).collect();
        if let Some(r) = one(hits) {
            return r;
        }
    }
    Err(Error::NotFound { query: query.to_string(), available: cameras.iter().map(|c| c.name.clone()).collect() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::tests::camera;

    #[test]
    fn every_kind_parses_and_round_trips() {
        for id in [
            "webcam:0x1234",
            "pattern:testcard",
            "qr:hello world",
            "qr:https://example.com/a?b=c:d",
            "text:Hi: there",
            "image:/tmp/a.png",
            "video:/tmp/clip.mp4",
            "solid:#336699",
            "solid:red",
        ] {
            let spec = SourceSpec::parse(id).unwrap_or_else(|e| panic!("{id}: {e}"));
            assert_eq!(spec.to_string(), id);
            assert_eq!(spec.to_string().parse::<SourceSpec>().ok(), Some(spec));
        }
        assert_eq!(SourceSpec::parse("qr:a:b"), Ok(SourceSpec::Qr("a:b".into())));
        assert_eq!(SourceSpec::parse("PATTERN:TestCard"), Ok(SourceSpec::Pattern(Pattern::Testcard)));
    }

    #[test]
    fn a_bare_name_is_a_webcam_query() {
        assert_eq!(SourceSpec::parse("OBS Virtual Camera"), Ok(SourceSpec::Webcam("OBS Virtual Camera".into())));
        assert_eq!(SourceSpec::parse("  obs "), Ok(SourceSpec::Webcam("obs".into())));
    }

    #[test]
    fn bad_ids_explain_themselves() {
        let cases = [
            ("", "empty source id"),
            ("foo:bar", "unknown source kind \"foo\""),
            ("pattern:zebra", "patterns: testcard"),
            ("qr:", "qr: needs the text to encode, as in qr:hello"),
            ("webcam:", "needs a camera id"),
            ("solid:mauve", "unknown colour"),
            ("image:", "PNG or JPEG"),
        ];
        for (id, want) in cases {
            let err = SourceSpec::parse(id).expect_err("must fail").to_string();
            assert!(err.contains(want), "{id:?}: {err}");
        }
    }

    fn cameras() -> Vec<HostCamera> {
        vec![
            camera("0x1100000046d0825", "FaceTime HD Camera"),
            camera("obs-virtual-cam-uid", "OBS Virtual Camera"),
            camera("usb-0000:00:14.0-1", "HD Pro Webcam C920"),
            camera("usb-0000:00:14.0-2", "HD Pro Webcam C920 #2"),
        ]
    }

    #[test]
    fn webcams_resolve_by_id_name_index_and_part_of_the_name() {
        let cams = cameras();
        let name = |q: &str| resolve_webcam(q, &cams).map(|c| c.name.as_str());
        assert_eq!(name("obs-virtual-cam-uid"), Ok("OBS Virtual Camera"));
        assert_eq!(name("webcam:obs-virtual-cam-uid"), Ok("OBS Virtual Camera"));
        assert_eq!(name("facetime hd camera"), Ok("FaceTime HD Camera"));
        assert_eq!(name("obs"), Ok("OBS Virtual Camera"));
        assert_eq!(name("OBS-virtual"), Ok("OBS Virtual Camera"));
        assert_eq!(name("facetime"), Ok("FaceTime HD Camera"));
        assert_eq!(name("1"), Ok("OBS Virtual Camera"));
        assert_eq!(name("webcam0"), Ok("FaceTime HD Camera"));
        // An exact name wins over the names that contain it.
        assert_eq!(name("HD Pro Webcam C920"), Ok("HD Pro Webcam C920"));
        assert_eq!(name("C920 #2"), Ok("HD Pro Webcam C920 #2"));
    }

    #[test]
    fn ambiguous_and_missing_webcams_name_the_candidates() {
        let cams = cameras();
        let err = resolve_webcam("hd", &cams).expect_err("must fail");
        let Error::Ambiguous { matches, .. } = &err else { panic!("{err:?}") };
        assert_eq!(matches.len(), 3, "{matches:?}");
        assert!(err.to_string().contains("FaceTime HD Camera (0x1100000046d0825)"), "{err}");

        let err = resolve_webcam("brio", &cams).expect_err("must fail");
        assert!(matches!(&err, Error::NotFound { available, .. } if available.len() == 4), "{err:?}");
        assert!(err.to_string().contains("available: FaceTime HD Camera, OBS Virtual Camera"), "{err}");
        assert!(resolve_webcam("9", &cams).is_err());
        let none = resolve_webcam("x", &[]).expect_err("must fail").to_string();
        assert!(none.ends_with("available: none"), "{none}");
    }
}
