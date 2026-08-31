probably rip out libcurl and use own http client at least for segment fetch

clean up logging and error handling

fix tcp corruption (have a hunch the downloading is sound but the listenre is not) nvm it's def the downloaders (actually might be that the chunks are getting dropped too earlier)
// doesn't corrupt with curl, ???
// no still corrupts with curl, just less. maybe some kind of atomics issues?

bring down binary size

add vod support

rtsp/rtp/rtcp server (seemingly best protocol for this, might make my own)

use slotmap/arena whatever instead of generic allocator (instrusive ref counts)

find a real live fMP4 stream, and download every segment and manifest update for testing, hopefully with a discontinuity as well

create twitch hls mock to debug errors, can generate data with seeded rng, and have the urls be the seed,
the segments vec is somehow loadbearing, need to go over all the unsafe code with a fine toothed comb

figure out how big the unbounded channels actually get, and bound them


https://github.com/compio-rs/compio