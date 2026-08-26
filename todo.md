probably rip out libcurl and use own http client at least for segment fetch


add option to receiver prefetches or not (gets us on par with twitch-hls-client)

clean up logging and error handling

fix tcp corruption (have a hunch the downloading is sound but the listenre is not) nvm it's def the downloaders (actually might be that the chunks are getting dropped  too earlier)
// doesn't corrupt with curl, ???
// no still corrupts with curl, just less. maybe some kind of atomics issues?




add  atomic log of each segments byte position in the file so that it can be  split up arbitrarily, and concat files instead of 



bring down binary size


add vod support

<!-- optimzie manifest refetch algo (poll at ~50ms for phase of new segments and  use round trip latency and other stuff to get even closer) -->
not needed since we're already grabbing the prefetch before they start transmitting




store timestamp within stream in boundaries file

http server to reserve from disk?


use slotmap/arena whatever instead of generic allocator (instrusive ref counts)


change segment record structure to include resolution (2 u16s) (framerate rational, 2 u16), mute status (1 bit, bool)

add discontinuity record

add init record