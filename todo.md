# TODO

- [ ] clean up logging and error handling
- [ ] parse com.amazon.ivs.unavailable-media in multivariant playlst, see if it's anything other than ertmpaudio1 (pretty sure this is the vod audio track u can send twitch to avoid mutes)
- [ ] test discontinuities on an fmp4 stream
- [ ] capture a portrait stream + public VOD (DVR variant set, PDT) for the disk recorder
- [ ] bring down binary size
- [ ] figure out how big the unbounded channels actually get, and bound them
- [ ] some protocol to monitor and control daemon mode. obvious is a rest api but that's not realtime, and also shitty
- [ ] move from clap to bpaf (evaluate first)
- [ ] Hermes WebSocket listener to automatically start recording
- [ ] include hermes updates in recorded index, possibly even chat and other stuff (redeems, bits, the horse animation)
- [ ] disk-backed RTSP seeking (read index/segs; stop the DVR refresh when disk is on)
- [ ] fMP4 / HEVC / AV1 RTP output
- [ ] Discord webhook sender behind notify()
- [ ] final options
  - [x] low latency
  - [x] auth token
  - [x] rtsp
  - [ ] file (disk output)
  - [ ] MoQ
  

