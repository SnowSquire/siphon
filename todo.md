probably rip out libcurl and use own http client at least for segment fetch

clean up logging and error handling

bring down binary size

add vod support

use SCORE to pick variant from master playlist

rtsp/rtp/rtcp server (seemingly best protocol for this, might make my own)

use slotmap/arena whatever instead of generic allocator (instrusive ref counts)

// test discontinuities on an fmp4 stream

figure out how big the unbounded channels actually get, and bound them

https://github.com/compio-rs/compio

rip out tcp sender and just use rtsp

final options

low latency
auth token
file, rtsp, MoQ
encryption for segment fetching (everything else is always encrypted)

pass stream info to the file writer, (stream id & streamer name at least)
