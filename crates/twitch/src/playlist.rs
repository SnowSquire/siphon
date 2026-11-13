//! Single-pass media playlist lexer.
//!
//! Twitch playlists are line oriented: tags with an optional `:value`, and bare
//! URIs that immediately follow their `#EXTINF`. The lexer folds each
//! `EXTINF`/URI pair into one [`Event`], seeds the sequence and timestamp
//! counters from the header tags, and hands the scheduler fully numbered items
//! in a single traversal.
//!
//! Sequence numbers are positional: `EXT-X-MEDIA-SEQUENCE + index` over every
//! item, ads and prefetches included. That matched all captured manifests,
//! including the ad windows where Twitch's sparse `EXT-X-TWITCH-LIVE-SEQUENCE`
//! annotations lag behind by the number of filler pads. The annotations are
//! therefore only a canary, not a numbering source.

use std::num::ParseIntError;
use std::str::Lines;

use thiserror::Error;

use crate::utils::attribute;
use crate::utils::float_string_to_millis;

#[derive(Error, Debug)]
pub enum ManifestParseError {
    #[error("invalid integer: {0}")]
    Int(#[from] ParseIntError),
    #[error("unable to split as expected")]
    Split(),
}

pub struct MediaSegment<'a> {
    pub uri: &'a str,
    pub sequence: u64,
    pub timestamp_ms: u64,
    pub duration_ms: u64,
    pub is_ad: bool,
}

pub enum Event<'a> {
    MediaSequence(u64),
    Init(&'a str),
    Discontinuity,
    /// A sparse `EXT-X-TWITCH-LIVE-SEQUENCE` annotation together with the
    /// sequence the next item will be assigned.
    Checkpoint {
        annotated: u64,
        next: u64,
    },
    Media(MediaSegment<'a>),
    Prefetch {
        uri: &'a str,
        sequence: u64,
    },
    Ended,
}

pub struct Playlist<'a> {
    lines: Lines<'a>,
    /// Pending `EXTINF`: duration in milliseconds and whether it is an ad.
    extinf: Option<(u64, bool)>,
    sequence: u64,
    timestamp_ms: u64,
}

impl<'a> Playlist<'a> {
    pub fn new(text: &'a str) -> Self {
        Self {
            lines: text.lines(),
            extinf: None,
            sequence: 0,
            timestamp_ms: 0,
        }
    }
}

impl<'a> Iterator for Playlist<'a> {
    type Item = Result<Event<'a>, ManifestParseError>;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        for line in self.lines.by_ref() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }

            let Some(tag) = line.strip_prefix('#') else {
                let Some((duration_ms, is_ad)) = self.extinf.take() else {
                    continue; // stray URI
                };

                let media = MediaSegment {
                    uri: line,
                    sequence: self.sequence,
                    timestamp_ms: self.timestamp_ms,
                    duration_ms,
                    is_ad,
                };

                self.sequence += 1;
                self.timestamp_ms += duration_ms;
                return Some(Ok(Event::Media(media)));
            };

            let (tag, value) = tag.split_once(':').unwrap_or((tag, ""));

            let event = match tag {
                "EXT-X-MEDIA-SEQUENCE" => match value.parse() {
                    Ok(front) => {
                        self.sequence = front;
                        Event::MediaSequence(front)
                    }
                    Err(err) => return Some(Err(err.into())),
                },
                "EXT-X-TWITCH-ELAPSED-SECS" => {
                    let Some(ms) = float_string_to_millis(value) else {
                        return Some(Err(ManifestParseError::Split()));
                    };
                    self.timestamp_ms = ms;
                    continue;
                }
                "EXT-X-MAP" => match attribute(value, "URI") {
                    Some(uri) => Event::Init(uri),
                    None => return Some(Err(ManifestParseError::Split())),
                },
                "EXT-X-DISCONTINUITY" => Event::Discontinuity,
                "EXT-X-ENDLIST" => Event::Ended,
                "EXT-X-TWITCH-LIVE-SEQUENCE" => match value.parse() {
                    Ok(annotated) => Event::Checkpoint {
                        annotated,
                        next: self.sequence,
                    },
                    Err(err) => return Some(Err(err.into())),
                },
                "EXT-X-TWITCH-PREFETCH" | "EXT-X-PREFETCH" => {
                    let sequence = self.sequence;
                    self.sequence += 1;
                    Event::Prefetch {
                        uri: value,
                        sequence,
                    }
                }
                "EXTINF" => {
                    let (duration, title) = value.split_once(',').unwrap_or((value, ""));
                    let Some(ms) = float_string_to_millis(duration) else {
                        return Some(Err(ManifestParseError::Split()));
                    };
                    self.extinf = Some((ms, title.contains('|')));
                    continue;
                }
                _ => continue,
            };

            return Some(Ok(event));
        }

        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn media(text: &str) -> Vec<(u64, u64, bool)> {
        Playlist::new(text)
            .filter_map(|event| match event.unwrap() {
                Event::Media(media) => Some((media.sequence, media.timestamp_ms, media.is_ad)),
                _ => None,
            })
            .collect()
    }

    fn checkpoints(text: &str) -> Vec<(u64, u64)> {
        Playlist::new(text)
            .filter_map(|event| match event.unwrap() {
                Event::Checkpoint { annotated, next } => Some((annotated, next)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn numbers_live_segments_and_prefetches() {
        let text = "\
#EXT-X-MEDIA-SEQUENCE:10
#EXT-X-TWITCH-LIVE-SEQUENCE:10
#EXT-X-TWITCH-ELAPSED-SECS:20.500
#EXTINF:2.000,live
https://a/1.ts
#EXTINF:2.000,live
https://a/2.ts
#EXT-X-TWITCH-PREFETCH:https://a/3.ts
";
        assert_eq!(media(text), [(10, 20_500, false), (11, 22_500, false)]);
        assert_eq!(checkpoints(text), [(10, 10)]);

        let prefetch: Vec<_> = Playlist::new(text)
            .filter_map(|event| match event.unwrap() {
                Event::Prefetch { sequence, .. } => Some(sequence),
                _ => None,
            })
            .collect();
        assert_eq!(prefetch, [12]);
    }

    #[test]
    fn ad_filler_advances_sequence_but_is_not_content() {
        let text = "\
#EXT-X-MEDIA-SEQUENCE:10224
#EXT-X-TWITCH-LIVE-SEQUENCE:10224
#EXTINF:2.000,live
https://a/1.ts
#EXTINF:2.000,Amazon|1
https://a/ad1.ts
#EXTINF:2.000,Amazon|1
https://a/ad2.ts
#EXTINF:0.235,Amazon|1
https://a/ad3.ts
#EXT-X-TWITCH-LIVE-SEQUENCE:10226
#EXTINF:2.000,live
https://a/2.ts
";
        assert_eq!(
            media(text),
            [
                (10224, 0, false),
                (10225, 2000, true),
                (10226, 4000, true),
                (10227, 6000, true),
                (10228, 6235, false),
            ]
        );
        // The post-ad annotation is the last ad segment, not the next live one.
        assert_eq!(checkpoints(text), [(10224, 10224), (10226, 10228)]);
    }

    #[test]
    fn dropout_checkpoint_matches_positional_sequence() {
        let text = "\
#EXT-X-MEDIA-SEQUENCE:50
#EXT-X-TWITCH-LIVE-SEQUENCE:50
#EXTINF:2.000,live
https://a/1.ts
#EXT-X-DISCONTINUITY
#EXT-X-TWITCH-LIVE-SEQUENCE:51
#EXTINF:5.029,live
https://a/2.ts
";
        assert_eq!(checkpoints(text), [(50, 50), (51, 51)]);
        assert_eq!(media(text), [(50, 0, false), (51, 2000, false)]);
    }

    #[test]
    fn parses_map_attributes() {
        let text = "\
#EXT-X-MAP:URI=\"main.mp4\",BYTERANGE=\"560@0\"
#EXT-X-MEDIA-SEQUENCE:0
#EXTINF:2.000,live
https://a/1.m4s
";
        let init: Vec<_> = Playlist::new(text)
            .filter_map(|event| match event.unwrap() {
                Event::Init(uri) => Some(uri),
                _ => None,
            })
            .collect();
        assert_eq!(init, ["main.mp4"]);
    }

    #[test]
    fn emits_discontinuity_and_endlist() {
        let text = "\
#EXT-X-MEDIA-SEQUENCE:0
#EXT-X-DISCONTINUITY
#EXTINF:2.000,live
https://a/1.ts
#EXT-X-ENDLIST
";
        let events: Vec<_> = Playlist::new(text)
            .filter_map(|event| match event.unwrap() {
                Event::Discontinuity => Some("discontinuity"),
                Event::Ended => Some("ended"),
                _ => None,
            })
            .collect();
        assert_eq!(events, ["discontinuity", "ended"]);
    }
}
