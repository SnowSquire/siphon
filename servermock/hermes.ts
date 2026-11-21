/**
 * Minimal Hermes pubsub server, run in the same process as the HLS mock.
 *
 * The daemon's client is `crates/daemon/src/hermes.rs`; it reads every payload
 * nested under a key named after the message type (real Hermes' shape), and it
 * matches notifications to subscriptions by the id echoed from `subscribe`.
 * The `pubsub` field is a JSON *string*, parsed a second time by the daemon.
 */
import { createHash } from "node:crypto";
import { createServer, type IncomingMessage } from "node:http";
import type { Duplex } from "node:stream";

const GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
const OP_TEXT = 0x1;
const OP_CLOSE = 0x8;
const OP_PING = 0x9;
const OP_PONG = 0xa;

/** The daemon parses subscription ids as 21-char NanoIds. */
const ID_ALPHABET =
  "0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ_-";

interface Subscription {
  id: string;
  topic: string;
  video: boolean;
  upSent: boolean;
  downSent: boolean;
}

interface Client {
  socket: Duplex;
  buffer: Buffer;
  subs: Map<string, Subscription>;
  timers: Set<NodeJS.Timeout>;
  fragments: Buffer[];
  fragmentOpcode: number | undefined;
  closed: boolean;
}

function uniqueId(): string {
  let id = "";
  for (let i = 0; i < 21; i++) {
    id += ID_ALPHABET[Math.floor(Math.random() * ID_ALPHABET.length)];
  }
  return id;
}

/** Server frames are never masked; the client masks every frame it sends. */
function encodeFrame(opcode: number, payload: Buffer): Buffer {
  const len = payload.length;
  let header: Buffer;
  if (len < 126) {
    header = Buffer.from([0x80 | opcode, len]);
  } else if (len <= 0xffff) {
    header = Buffer.alloc(4);
    header[0] = 0x80 | opcode;
    header[1] = 126;
    header.writeUInt16BE(len, 2);
  } else {
    header = Buffer.alloc(10);
    header[0] = 0x80 | opcode;
    header[1] = 127;
    header.writeBigUInt64BE(BigInt(len), 2);
  }
  return Buffer.concat([header, payload]);
}

function sendFrame(client: Client, opcode: number, payload: Buffer): void {
  if (!client.closed) {
    client.socket.write(encodeFrame(opcode, payload));
  }
}

function sendText(client: Client, value: object): void {
  sendFrame(client, OP_TEXT, Buffer.from(JSON.stringify(value)));
}

function notify(client: Client, sub: Subscription, type: string): void {
  sendText(client, {
    type: "notification",
    notification: {
      subscription: { id: sub.id, topic: sub.topic },
      pubsub: JSON.stringify({ type }),
    },
  });
}

/** Sends one subscribeResponse per topic, echoing the id the client chose. */
function handleSubscribe(
  client: Client,
  message: Record<string, unknown>,
  ended: () => boolean,
): void {
  const subscribe = message.subscribe as Record<string, unknown> | undefined;
  const topics: { id: string; topic: string }[] = [];
  if (subscribe && typeof subscribe === "object") {
    // The daemon's wire format: subscribe.pubsub.topic plus subscribe.id.
    const pubsub = subscribe.pubsub as Record<string, unknown> | undefined;
    const topic = pubsub?.topic ?? subscribe.topic;
    if (typeof topic === "string") {
      topics.push({ topic, id: String(subscribe.id ?? uniqueId()) });
    }
  } else if (Array.isArray(message.topics)) {
    // The simplified {topics, nonce} shape, for hand-written test clients.
    for (const topic of message.topics) {
      if (typeof topic === "string") {
        topics.push({ topic, id: uniqueId() });
      }
    }
  }

  const nonce = message.nonce ?? message.id;
  for (const { id, topic } of topics) {
    const sub: Subscription = {
      id,
      topic,
      video: topic.startsWith("video-playback-by-id."),
      upSent: false,
      downSent: false,
    };
    client.subs.set(id, sub);
    console.log(`hermes: subscribe ${topic} (${id})`);
    // Twitch and simple clients may read either shape; the daemon reads the
    // nested one. Sending both keeps one server valid for both.
    const subscription = { id, topic };
    sendText(client, {
      type: "subscribeResponse",
      result: "ok",
      subscription,
      nonce,
      subscribeResponse: { result: "ok", subscription },
    });

    if (!sub.video) {
      continue;
    }
    // stream-up is not replayed by Twitch, so tests rely on this mock to
    // produce it once shortly after the subscription is confirmed. The end
    // check below only sends stream-down to subscriptions that already got
    // their stream-up; a late subscriber gets both here instead.
    const timer = setTimeout(() => {
      client.timers.delete(timer);
      if (client.closed) {
        return;
      }
      sub.upSent = true;
      notify(client, sub, "stream-up");
      if (ended() && !sub.downSent) {
        sub.downSent = true;
        notify(client, sub, "stream-down");
      }
    }, 1000);
    client.timers.add(timer);
  }
}

function handleMessage(client: Client, text: string, ended: () => boolean): void {
  let message: Record<string, unknown>;
  try {
    const parsed: unknown = JSON.parse(text);
    if (!parsed || typeof parsed !== "object") {
      return;
    }
    message = parsed as Record<string, unknown>;
  } catch {
    return;
  }
  switch (message.type) {
    case "ping":
      sendText(client, { type: "pong" });
      return;
    case "subscribe":
      handleSubscribe(client, message, ended);
      return;
    default:
      return;
  }
}

function handleFrame(
  client: Client,
  fin: boolean,
  opcode: number,
  payload: Buffer,
  ended: () => boolean,
): void {
  if (opcode === 0 || opcode === OP_TEXT || opcode === 0x2) {
    if (opcode !== 0) {
      client.fragmentOpcode = opcode;
    }
    client.fragments.push(payload);
    if (!fin) {
      return;
    }
    const kind = client.fragmentOpcode;
    const text = Buffer.concat(client.fragments).toString("utf8");
    client.fragments = [];
    client.fragmentOpcode = undefined;
    if (kind === OP_TEXT) {
      handleMessage(client, text, ended);
    }
    return;
  }
  if (opcode === OP_PING) {
    sendFrame(client, OP_PONG, payload);
  } else if (opcode === OP_CLOSE) {
    sendFrame(client, OP_CLOSE, payload.subarray(0, 2));
    client.closed = true;
    client.socket.end();
  }
}

/** Unmask and dispatch as many frames as have arrived. */
function drain(client: Client, ended: () => boolean): void {
  for (;;) {
    const buffer = client.buffer;
    if (buffer.length < 2) {
      return;
    }
    const first = buffer[0]!;
    const second = buffer[1]!;
    if ((second & 0x80) === 0) {
      // Every client frame must be masked; there is nothing to recover here.
      client.socket.destroy();
      return;
    }
    let len = second & 0x7f;
    let offset = 2;
    if (len === 126) {
      if (buffer.length < 4) {
        return;
      }
      len = buffer.readUInt16BE(2);
      offset = 4;
    } else if (len === 127) {
      if (buffer.length < 10) {
        return;
      }
      len = Number(buffer.readBigUInt64BE(2));
      offset = 10;
    }
    const payloadOffset = offset + 4;
    if (buffer.length < payloadOffset + len) {
      return;
    }
    const mask = buffer.subarray(offset, payloadOffset);
    const payload = Buffer.from(buffer.subarray(payloadOffset, payloadOffset + len));
    for (let i = 0; i < payload.length; i++) {
      payload[i]! ^= mask[i % 4]!;
    }
    client.buffer = buffer.subarray(payloadOffset + len);
    handleFrame(client, (first & 0x80) !== 0, first & 0x0f, payload, ended);
    if (client.closed) {
      return;
    }
  }
}

/** Starts the websocket server on 127.0.0.1:8001/v1. */
export function startHermes(ended: () => boolean): void {
  const clients = new Set<Client>();
  const server = createServer();

  server.on("upgrade", (req: IncomingMessage, socket: Duplex, head: Buffer) => {
    const path = new URL(req.url ?? "/", "http://localhost").pathname;
    if (path !== "/v1" || typeof req.headers["sec-websocket-key"] !== "string") {
      socket.destroy();
      return;
    }
    const accept = createHash("sha1")
      .update(req.headers["sec-websocket-key"] + GUID)
      .digest("base64");
    socket.write(
      "HTTP/1.1 101 Switching Protocols\r\n" +
        "Upgrade: websocket\r\n" +
        "Connection: Upgrade\r\n" +
        `Sec-WebSocket-Accept: ${accept}\r\n\r\n`,
    );

    const client: Client = {
      socket,
      buffer: head,
      subs: new Map(),
      timers: new Set(),
      fragments: [],
      fragmentOpcode: undefined,
      closed: false,
    };
    clients.add(client);
    socket.on("data", (chunk: Buffer) => {
      client.buffer = Buffer.concat([client.buffer, chunk]);
      drain(client, ended);
    });
    socket.on("close", () => {
      client.closed = true;
      for (const timer of client.timers) {
        clearTimeout(timer);
      }
      client.timers.clear();
      clients.delete(client);
    });
    socket.on("error", () => {
      socket.destroy();
    });

    // Twitch announces its keepalive interval in the welcome. The daemon drops
    // a socket after two missed intervals, so periodic keepalives keep long
    // (slow-speed) test runs connected.
    sendText(client, {
      type: "welcome",
      welcome: { keepaliveSec: 15 },
      keepaliveSec: 15,
    });
    const keepalive = setInterval(() => sendText(client, { type: "keepalive" }), 10_000);
    client.timers.add(keepalive);
  });

  // The playlist generators declare ENDLIST at the same instant: both use the
  // simulated `content_now >= streamDuration` clock, not a subscribed channel.
  // Only confirmed (stream-up) subscriptions get stream-down here; otherwise a
  // subscription accepted right at the end would be notified twice.
  const endCheck = setInterval(() => {
    if (!ended()) {
      return;
    }
    clearInterval(endCheck);
    for (const client of clients) {
      for (const sub of client.subs.values()) {
        if (sub.video && sub.upSent && !sub.downSent) {
          sub.downSent = true;
          notify(client, sub, "stream-down");
        }
      }
    }
  }, 100);

  server.listen(8001, "127.0.0.1", () => {
    console.log("hermes: ws://127.0.0.1:8001/v1");
  });
}
