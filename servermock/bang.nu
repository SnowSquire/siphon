open meta.json
| get 0.Playlist.MediaParts.0.MediaSegments
| reduce -f {rows: [], time: 0.0} {|seg, acc|
  let row = {
    filename: $'($seg.Index | fill -w 3 -c 0 -a r).ts'
    length_ms: ($seg.Duration * 1000)
    present_time_ms: $acc.time
  }
  {
    rows: ($acc.rows | append $row)
    time: ($acc.time + $seg.Duration * 1000)
  }
} | get rows