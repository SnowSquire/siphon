//! DVR/VOD playlist discovery and parsing.
//!
//! The live playlist only contains the recent window. Twitch exposes the whole
//! broadcast (from sequence 0) as `index-dvr.m3u8` next to it; the mock serves
//! the same path. The RTSP output uses that timeline for seeking and the live
//! scheduler spools for the edge.

use crate::playlist::Event;
use crate::playlist::Playlist;

/// One segment of the DVR playlist, positioned on the broadcast timeline.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VodSegment {
    pub start_ms: u64,
    pub duration_ms: u64,
    pub url: String,
}

#[derive(Clone, Debug, Default)]
pub struct VodTimeline {
    /// Sorted by `start_ms`.
    pub segments: Vec<VodSegment>,
    /// End of the newest segment, in broadcast milliseconds.
    pub end_ms: u64,
}

impl VodTimeline {
    /// Parse a media playlist. `base_url` resolves relative segment URIs.
    #[must_use]
    pub fn parse(base_url: &str, text: &str) -> Self {
        let mut segments = Vec::new();
        let mut end_ms = 0;

        for event in Playlist::new(text).flatten() {
            if let Event::Media(media) = event {
                let url = resolve_url(base_url, media.uri);
                end_ms = media.timestamp_ms.saturating_add(media.duration_ms);
                segments.push(VodSegment {
                    start_ms: media.timestamp_ms,
                    duration_ms: media.duration_ms,
                    url,
                });
            }
        }

        Self { segments, end_ms }
    }

    /// Index of the segment containing `position_ms`.
    #[must_use]
    pub fn find(&self, position_ms: u64) -> Option<usize> {
        let index = self
            .segments
            .partition_point(|segment| segment.start_ms <= position_ms);
        let index = index.checked_sub(1)?;
        let segment = &self.segments[index];
        (position_ms < segment.start_ms.saturating_add(segment.duration_ms)).then_some(index)
    }
}

/// Derive the DVR playlist URL from the live media playlist URL.
///
/// Twitch and the mock both keep `index-dvr.m3u8` next to the live
/// `index*.m3u8`, so replacing the final component is enough.
#[must_use]
pub fn dvr_url(live_url: &str) -> Option<String> {
    let (path, query) = live_url
        .split_once('?')
        .map_or((live_url, None), |(path, query)| (path, Some(query)));
    let slash = path.rfind('/')?;
    let (directory, file) = path.split_at(slash + 1);

    if !file.to_ascii_lowercase().ends_with(".m3u8") {
        return None;
    }
    if file.starts_with("index-dvr") {
        return Some(live_url.to_owned());
    }
    if !file.starts_with("index") {
        return None;
    }

    let mut url = format!("{directory}index-dvr.m3u8");
    if let Some(query) = query {
        url.push('?');
        url.push_str(query);
    }
    Some(url)
}

/// Resolve a segment URI against the playlist URL.
#[must_use]
pub fn resolve_url(base_url: &str, uri: &str) -> String {
    if uri.starts_with("http://") || uri.starts_with("https://") {
        return uri.to_owned();
    }

    let origin_len = base_url.find("://").map_or(0, |index| index + 3);
    let path_start = base_url[origin_len..]
        .find('/')
        .map_or(base_url.len(), |index| origin_len + index);
    let origin = &base_url[..path_start];

    if let Some(absolute) = uri.strip_prefix('/') {
        return format!("{origin}/{absolute}");
    }

    let path_end = base_url.find('?').unwrap_or(base_url.len());
    let directory = base_url[..path_end]
        .rfind('/')
        .map_or(origin, |slash| &base_url[..slash]);
    format!("{directory}/{uri}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const DVR: &str = "\
#EXTM3U
#EXT-X-VERSION:3
#EXT-X-TARGETDURATION:12
#EXT-X-PLAYLIST-TYPE:EVENT
#EXT-X-MEDIA-SEQUENCE:0
#EXT-X-TWITCH-ELAPSED-SECS:0.000
#EXT-X-TWITCH-TOTAL-SECS:30.000
#EXTINF:10.000,
dvr/0.ts
#EXTINF:10.000,
dvr/1.ts
#EXTINF:10.000,
dvr/2.ts
";

    #[test]
    fn derives_the_dvr_url() {
        assert_eq!(
            dvr_url("http://127.0.0.2:8000/index.m3u8"),
            Some("http://127.0.0.2:8000/index-dvr.m3u8".to_owned())
        );
        assert_eq!(
            dvr_url("https://cdn/720p60/index-muted-ABC.m3u8?sig=x"),
            Some("https://cdn/720p60/index-dvr.m3u8?sig=x".to_owned())
        );
        assert_eq!(
            dvr_url("https://cdn/chunked/index-dvr.m3u8"),
            Some("https://cdn/chunked/index-dvr.m3u8".to_owned())
        );
        assert_eq!(dvr_url("http://host/foo.m3u8"), None);
    }

    #[test]
    fn parses_positions_and_resolves_urls() {
        let timeline = VodTimeline::parse("http://127.0.0.2:8000/index-dvr.m3u8", DVR);
        assert_eq!(timeline.segments.len(), 3);
        assert_eq!(timeline.end_ms, 30_000);
        assert_eq!(timeline.segments[1].start_ms, 10_000);
        assert_eq!(timeline.segments[1].url, "http://127.0.0.2:8000/dvr/1.ts");
    }

    #[test]
    fn finds_the_containing_and_next_segment() {
        let timeline = VodTimeline::parse("http://host/index-dvr.m3u8", DVR);
        assert_eq!(timeline.find(0), Some(0));
        assert_eq!(timeline.find(15_000), Some(1));
        assert_eq!(timeline.find(30_000), None);
    }

    #[test]
    fn resolves_absolute_and_root_relative_urls() {
        assert_eq!(
            resolve_url("http://host/a/index.m3u8", "https://cdn/x.ts"),
            "https://cdn/x.ts"
        );
        assert_eq!(
            resolve_url("http://host/a/index.m3u8", "/x.ts"),
            "http://host/x.ts"
        );
    }
}
