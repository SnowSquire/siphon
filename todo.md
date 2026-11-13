# TODO

- [ ] clean up logging and error handling
- [ ] test discontinuities on an fmp4 stream
- [ ] bring down binary size
- [ ] figure out how big the unbounded channels actually get, and bound them
- [ ] move from clap to bpaf (evaluate first)
- [ ] capture a portrait stream + public VOD (DVR variant set, PDT) for the disk recorder
- [ ] final options
  - [x] low latency
  - [x] auth token
  - [x] rtsp
  - [ ] file (disk output)
  - [ ] MoQ
  - [ ] encryption for segment fetching (everything else is always encrypted)

## Done

- [x] move to https://github.com/mcmah309/eros
- [x] add vod support
- [x] rtsp/rtp/rtcp server
- [x] compio runtime (https://github.com/compio-rs/compio)
- [x] rip out the tcp sender, RTSP is the only sink
- [x] arena/slotmap allocator instead of the generic allocator (intrusive refcounts)
- [x] miri as the unsafe verifier (`cargo +nightly miri test`)
