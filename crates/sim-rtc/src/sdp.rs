//! What the publisher reads from SDP itself (str0m parses and applies the rest): the H.264
//! `profile-level-id` an answer settled on, and trickle ICE fragments (RFC 8840) for PATCH.

use crate::h264::ProfileLevelId;

/// The `profile-level-id` of the first H.264 format in the answer's first video section, in the
/// order of its `m=` line. `None` when there is no video section, no H.264 in it, or the section
/// is rejected (port 0).
pub fn answered_h264_profile(answer: &str) -> Option<ProfileLevelId> {
    let lines: Vec<&str> = answer.lines().map(str::trim).collect();
    let start = lines.iter().position(|l| l.starts_with("m=video "))?;
    let end = lines[start + 1..].iter().position(|l| l.starts_with("m=")).map_or(lines.len(), |i| start + 1 + i);
    let section = &lines[start..end];
    let mut fields = section[0].split_whitespace().skip(1);
    if fields.next()? == "0" {
        return None;
    }
    let pts: Vec<&str> = fields.skip(1).collect();
    let attr = |prefix: &str, pt: &str| -> Option<&str> {
        let key = format!("a={prefix}:{pt} ");
        section.iter().find_map(|l| l.strip_prefix(key.as_str()))
    };
    for pt in pts {
        let Some(rtpmap) = attr("rtpmap", pt) else { continue };
        if !rtpmap.to_ascii_uppercase().starts_with("H264/") {
            continue;
        }
        let fmtp = attr("fmtp", pt).unwrap_or_default();
        // RFC 6184 §8.1: no profile-level-id means 420010 (Baseline, level 1).
        let plid = fmtp
            .split(';')
            .filter_map(|kv| kv.trim().split_once('='))
            .find(|(k, _)| k.eq_ignore_ascii_case("profile-level-id"))
            .map_or(Some(ProfileLevelId::new(0x42, 0x00, 0x0a)), |(_, v)| ProfileLevelId::parse(v.trim()));
        return plid;
    }
    None
}

/// The `a=ice-ufrag` and `a=ice-pwd` of an SDP (session or first media level).
pub fn ice_credentials(sdp: &str) -> Option<(String, String)> {
    let find = |key: &str| sdp.lines().find_map(|l| l.trim().strip_prefix(key).map(str::to_owned));
    Some((find("a=ice-ufrag:")?, find("a=ice-pwd:")?))
}

/// An `application/trickle-ice-sdpfrag` body (RFC 8840 §9, as RFC 9725 §4.3.1 uses it):
/// the session's ICE credentials, then one media section per `mid` with its candidates
/// (`candidate:…` strings, without `a=`), and `a=end-of-candidates` when gathering is done.
pub fn trickle_fragment(ufrag: &str, pwd: &str, mid: &str, candidates: &[String], end: bool) -> String {
    let mut s = format!("a=ice-ufrag:{ufrag}\r\na=ice-pwd:{pwd}\r\nm=video 9 UDP/TLS/RTP/SAVPF 0\r\na=mid:{mid}\r\n");
    for c in candidates {
        let c = c.trim().trim_start_matches("a=");
        s.push_str(&format!("a={c}\r\n"));
    }
    if end {
        s.push_str("a=end-of-candidates\r\n");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    const ANSWER: &str = "v=0\r\no=- 1 1 IN IP4 0.0.0.0\r\ns=-\r\nt=0 0\r\na=ice-ufrag:abcd\r\na=ice-pwd:secretsecret\r\n\
        m=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=rtpmap:111 opus/48000/2\r\n\
        m=video 9 UDP/TLS/RTP/SAVPF 97 96\r\na=mid:0\r\na=rtpmap:96 H264/90000\r\n\
        a=fmtp:96 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f\r\n\
        a=rtpmap:97 rtx/90000\r\na=fmtp:97 apt=96\r\n";

    #[test]
    fn reads_the_answered_profile() {
        assert_eq!(answered_h264_profile(ANSWER), ProfileLevelId::parse("42e01f"));
        let main = ANSWER.replace("42e01f", "4D001F");
        assert_eq!(answered_h264_profile(&main), Some(ProfileLevelId::MAIN_3_1));
        let bare = ANSWER.replace("level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f", "packetization-mode=1");
        assert_eq!(answered_h264_profile(&bare).map(|p| p.to_string()), Some("42000a".into()));
        assert_eq!(answered_h264_profile(&ANSWER.replace("m=video 9", "m=video 0")), None, "rejected");
        assert_eq!(answered_h264_profile(&ANSWER.replace("H264/90000", "VP8/90000")), None);
        assert_eq!(answered_h264_profile("v=0\r\n"), None);
    }

    #[test]
    fn builds_trickle_fragments() {
        let (ufrag, pwd) = ice_credentials(ANSWER).expect("creds");
        assert_eq!((ufrag.as_str(), pwd.as_str()), ("abcd", "secretsecret"));
        let frag = trickle_fragment(&ufrag, &pwd, "0", &["a=candidate:1 1 udp 2130706431 192.168.1.2 50000 typ host".into()], true);
        assert_eq!(
            frag,
            "a=ice-ufrag:abcd\r\na=ice-pwd:secretsecret\r\nm=video 9 UDP/TLS/RTP/SAVPF 0\r\na=mid:0\r\n\
             a=candidate:1 1 udp 2130706431 192.168.1.2 50000 typ host\r\na=end-of-candidates\r\n"
        );
    }
}
