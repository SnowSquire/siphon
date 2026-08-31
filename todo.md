probably rip out libcurl and use own http client at least for segment fetch


clean up logging and error handling

fix tcp corruption (have a hunch the downloading is sound but the listenre is not) nvm it's def the downloaders (actually might be that the chunks are getting dropped  too earlier)
// doesn't corrupt with curl, ???
// no still corrupts with curl, just less. maybe some kind of atomics issues?




bring down binary size


add vod support



rtsp/rtp/rtcp server (seemingly best protocol for this, might make my own)



use slotmap/arena whatever instead of generic allocator (instrusive ref counts)



