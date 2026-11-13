# TODO

- [ ] clean up logging and error handling
- [ ] parse com.amazon.ivs.unavailable-media in multivariant playlst, see if it's anything other than ertmpaudio1 (pretty sure this is the vod audio track u can send twitch to avoid mutes)
- [ ] test discontinuities on an fmp4 stream
- [ ] capture a portrait stream + public VOD (DVR variant set, PDT) for the disk recorder
- [ ] bring down binary size
- [ ] figure out how big the unbounded channels actually get, and bound them
- [ ] move from clap to bpaf (evaluate first)
- [ ] Hermes WebSocket listener to automatically start recording
- [ ] include hermes updates in index, possibly even chat and other stuff (redeems, bits, the horse animation)
- [ ] get back on the gui grind https://blog.zortax.de/posts/a-critique-of-gpui this guy's framework looks cool and he has all the same problems I have with the state of rust gui frameworks
- [ ] disk-backed RTSP seeking (read index/segs; stop the DVR refresh when disk is on)
- [ ] fMP4 / HEVC / AV1 RTP output
- [ ] Discord webhook sender behind notify()
- [ ] final options
  - [x] low latency
  - [x] auth token
  - [x] rtsp
  - [ ] file (disk output)
  - [ ] MoQ
  
## Done

- [x] move to https://github.com/mcmah309/eros
- [x] add vod support
- [x] rtsp/rtp/rtcp server
- [x] compio runtime (https://github.com/compio-rs/compio)
- [x] rip out the tcp sender, RTSP is the only sink
- [x] arena/slotmap allocator instead of the generic allocator (intrusive refcounts)
- [x] miri as the unsafe verifier (`cargo +nightly miri test`)
