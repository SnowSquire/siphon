// verify.js
// npm i @node-rs/xxhash
import net from "node:net";
import { xxh3 } from "@node-rs/xxhash";
import manifest from "./idk.json" with { type: "json" };

const HOST = "::1";
const PORT = 7417;
export const SEED = 0n; // must match the seed the producer hashed with

export const HASH_HEX_LENGTH = 32; // 128 bits

function normalizeHash(hash) {
  return hash.replace(/^0x/i, "").toLowerCase().padStart(HASH_HEX_LENGTH, "0");
}

/** Normalize and sort the manifest once, so the read loop stays simple. */
function loadSegments() {
  return manifest.records
    .map(({ segment }) => ({
      sequence: segment.sequence_number,
      offset: Number(segment.byte_offset),
      length: Number(segment.byte_length),
      hash: normalizeHash(segment.hash),
    }))
    .sort((a, b) => a.offset - b.offset);
}

/** Pull-based, exact-size reads over a socket's async iterator. */
class ChunkReader {
  #iter;
  #queue = [];
  #available = 0;

  constructor(stream) {
    this.#iter = stream[Symbol.asyncIterator]();
  }

  /** Read exactly `n` bytes; null if the stream ends first. */
  async read(n) {
    while (this.#available < n) {
      const { value, done } = await this.#iter.next();
      if (done) return null;
      this.#queue.push(value);
      this.#available += value.length;
    }
    const head = Buffer.concat(this.#queue);
    this.#queue = [head.subarray(n)];
    this.#available -= n;
    return head.subarray(0, n);
  }

  /** Consume and count whatever arrives after the last segment. */
  async drain() {
    let bytes = this.#available;
    this.#queue.length = 0;
    this.#available = 0;
    while (true) {
      const { value, done } = await this.#iter.next();
      if (done) return bytes;
      bytes += value.length;
    }
  }
}

async function verify() {
  const segments = loadSegments();
  const total = segments.reduce((n, s) => n + s.length, 0);
  console.log(`Expecting ${total} bytes across ${segments.length} segments`);

  const reader = new ChunkReader(net.connect(PORT, HOST));
  let received = 0;
  let failures = 0;

  for (const seg of segments) {
    const data = await reader.read(seg.length);
    if (data === null) {
      throw new Error(
        `Stream ended early: got ${received}/${total} bytes ` +
          `(segment ${seg.sequence} incomplete)`,
      );
    }

    const digest = xxh3.xxh128(data, SEED).toString(16).padStart(HASH_HEX_LENGTH, "0");

    await Deno.writeFile(`denosegs/${seg.sequence}.ts`, data);

    if (digest === seg.hash) {
      console.log(`seg ${seg.sequence}: OK       ${digest}`);
    } else {
      failures++;
      console.error(`seg ${seg.sequence}: MISMATCH expected ${seg.hash}, got ${digest}`);
    }
    received += seg.length;
  }

  const trailing = await reader.drain();
  if (trailing > 0) {
    console.warn(`Warning: ${trailing} trailing bytes after last segment`);
  }

  return failures;
}

try {
  const failures = await verify();
  if (failures === 0) {
    console.log("All segments verified.");
  } else {
    console.log(`Done with ${failures} hash mismatch(es).`);
    process.exitCode = 1;
  }
} catch (err) {
  console.error(`Verification failed: ${err.message ?? err}`);
  process.exitCode = 1;
}
