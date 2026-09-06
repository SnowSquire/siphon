#pragma description Stream Index File Format
#pragma endian little

import type.base;
import std.mem;

enum RecordType : u8 {
    MpegTs        = 0,
    FMp4          = 1,
    Discontinuity = 2,
    Init          = 3,
};

struct Segment {
    type::Hex<u128> hash;
    u64 sequence_number;
    u64 byte_offset;
    u32 byte_length;
    u32 timestamp_ms;
    bool muted;
} [[packed]];

struct Discontinuity {
    u16 width;
    u16 height;
    u16 rate_num;
    u16 rate_denom;
} [[packed]];

struct Init {
    u64 counter;
    u128 hash;
    u64 byte_offset;
    u32 byte_length;
} [[packed]];

struct Header {
    char magic[8];
    u64 version;
    u64 stream_id;
} [[packed]];

struct Record {
    RecordType type [[hidden]]; 

    match (type) {
        (RecordType::MpegTs|RecordType::FMp4): Segment segment;
        (RecordType::Discontinuity):           Discontinuity discontinuity;
        (RecordType::Init):                    Init init;
    }
} [[inline]];
Header header @ 0x00;
Record records[while(!std::mem::eof())] @ $;