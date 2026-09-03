clean up logging and error handling

add vod support

rtsp/rtp/rtcp server (seemingly best protocol for this, might make my own)

// test discontinuities on an fmp4 stream

https://github.com/compio-rs/compio

rip out tcp sender and just use rtsp

final options

low latency
auth token
file, rtsp, MoQ
encryption for segment fetching (everything else is always encrypted)

use slotmap/arena whatever instead of generic allocator (instrusive ref counts)

bring down binary size

figure out how big the unbounded channels actually get, and bound them

miri? or some other unsafe verifier? i use unsafe a lot so....

move from clap to bpaf (evaluate first)
