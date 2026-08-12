fn attributes(tag: &str) -> impl Iterator<Item = (&str, &str)> {
    let mut rest = tag.split_once(':').map_or("", |(_, attrs)| attrs);
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }

        let eq: usize = rest.find('=')?;
        let key = rest[..eq].trim();
        let after_eq = &rest[eq + 1..];

        let (value, tail) = if let Some(quoted) = after_eq.strip_prefix('"') {
            let close = quoted.find('"')?;
            let value = &quoted[..close];
            let tail = quoted[close + 1..].strip_prefix(',').unwrap_or("");
            (value, tail)
        } else {
            after_eq
                .split_once(',')
                .map_or((after_eq.trim(), ""), |(v, t)| (v.trim(), t))
        };

        rest = tail;
        Some((key, value))
    })
}

fn get_attr<'a>(tag: &'a str, key: &str) -> Option<&'a str> {
    attributes(tag).find(|(k, _)| *k == key).map(|(_, v)| v)
}

fn parse_name<'a>(media: Option<&'a str>, stream_inf: &'a str) -> Option<&'a str> {
    let name = media
        .and_then(|m| get_attr(m, "NAME"))
        .or_else(|| get_attr(stream_inf, "IVS-NAME"))?;
    Some(name.strip_suffix(" (source)").unwrap_or(name))
}

fn parse_resolution(stream_inf: &str) -> Option<(u16, u16)> {
    let (w, h) = get_attr(stream_inf, "RESOLUTION")?.split_once('x')?;
    Some((w.parse().ok()?, h.parse().ok()?))
}

struct Candidate<'a> {
    url_range: &'a str,
    area: u32,
}

// "track" isn't the best naming, but "stream" is already used in this project
// to mean a stream of bytes (from an HTTP source), and this combo has the best
// overall fit I can think of for now.
pub fn select_track(buffer: &str, preferred_prefix: Option<&str>) -> Option<Box<str>> {
    let mut media: Option<&str> = None;
    let mut stream_inf: Option<&str> = None;
    let mut best: Option<Candidate> = None;

    for line in buffer.lines() {
        if line.starts_with("#EXT-X-MEDIA") {
            media = Some(line);
        } else if line.starts_with("#EXT-X-STREAM-INF") {
            stream_inf = Some(line);
        } else if line.starts_with("http") {
            let Some(inf) = stream_inf.take() else {
                continue;
            };

            let Some(name) = parse_name(media.take(), inf) else {
                continue;
            };

            if let Some(prefix) = preferred_prefix {
                if name.starts_with(prefix) {
                    return Some(line.into());
                }
            }
            let area = parse_resolution(inf).map_or(0, |(w, h)| w as u32 * h as u32);
            if best.as_ref().is_none_or(|b| area > b.area) {
                best = Some(Candidate {
                    url_range: line,
                    area,
                });
            }
        }
    }

    best.map(|b| b.url_range.into())
}
