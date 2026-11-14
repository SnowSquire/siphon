#pragma description Stream Index File Format v4
#pragma endian little

import type.base;
import std.mem;

enum RecordType : u8 {
    Segment       = 0,
    Init          = 1,
    Discontinuity = 2,
    Run           = 3,
};

enum Source : u8 {
    Live = 0,
    Dvr  = 1,
};

// One media chunk. `elapsed_ms` is the playlist's broadcast-relative time (what
// NPT uses); `pdt_ms` is the absolute EXT-X-PROGRAM-DATE-TIME (Unix epoch ms) and
// is the only key the live and DVR sources are aligned by.
struct Segment {
    Source source;
    u64 sequence;
    u32 elapsed_ms;
    u64 pdt_ms;
    u32 duration_ms;
    u64 byte_offset;
    u32 byte_length;
    u128 hash;
    u8 flags; // bit 0: muted
} [[packed]];

struct Init {
    Source source;
    u64 counter;
    u64 byte_offset;
    u32 byte_length;
    u128 hash;
} [[packed]];

struct Discontinuity {
    u16 width;
    u16 height;
    u16 rate_num;
    u16 rate_denom;
} [[packed]];

// Sits at the start of the index area reserved for one live run. The marker is
// followed by `record_len` bytes of rebased run records (zero while pending).
// `run` names live.NNN so a resume can find the source pair; it is one field
// more than the original design table, which is why this file is the authority.
struct Run {
    u32 run;
    u64 dst_segs_offset;
    u64 segs_len;
    u64 source_segs_offset;
    u64 source_index_offset;
    u32 record_len;
    u8 complete; // 0 pending, 1 done (or deliberately abandoned)
} [[packed]];

struct Header {
    char magic[8]; // "OZENIKEN"
    u32 version;   // 4
    u64 stream_id;
} [[packed]];

struct Record {
    RecordType type [[hidden]];

    match (type) {
        (RecordType::Segment):       Segment segment;
        (RecordType::Init):          Init init;
        (RecordType::Discontinuity): Discontinuity discontinuity;
        (RecordType::Run):           Run run;
    }
} [[inline]];

Header header @ 0x00;
Record records[while(!std::mem::eof())] @ $;
