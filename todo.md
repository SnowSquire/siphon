probably rip out libcurl and use own http client at least for segment fetch


add option to receiver prefetches or not (gets us on par with twitch-hls-client)

clean up logging and error handling

fix tcp corruption (have a hunch the downloading is sound but the listenre is not) nvm it's def the downloaders (actually might be that the chunks are getting dropped  too earlier)
// doesn't corrupt with curl, ???




add  atomic log of each segments byte position in the file so that it can be  split up arbitrarily, and concat files instead of 



bring down binary size


add vod support

optimzie manifest refetch algo (poll at ~50ms for phase of new segments and  use round trip latency and other stuff to get even closer)


optimize startup time (current ~2.5seconds) start - first bytes over tcp