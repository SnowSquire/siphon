const hermesURL = "wss://hermes.twitch.tv/v1?clientId=kimne78kx3ncx6brgo4mv6wki5h1ko";
fetch("wss://hermes.twitch.tv/v1?clientId=kimne78kx3ncx6brgo4mv6wki5h1ko", {
  headers: {
    "accept-language": "en-US,en;q=0.9",
    "cache-control": "no-cache",
    pragma: "no-cache",
    "sec-websocket-extensions": "permessage-deflate; client_max_window_bits",
    "sec-websocket-key": "IEaNry7766b6DUZ36KTU1A==",
    "sec-websocket-version": "13",
  },
  body: null,
  method: "GET",
});

export const NanoId = (e = 21) =>
  crypto
    .getRandomValues(new Uint8Array(e))
    .reduce(
      (acc, rand) =>
        (acc +=
          (rand &= 63) < 36
            ? rand.toString(36)
            : rand < 62
              ? (rand - 26).toString(36).toUpperCase()
              : rand > 62
                ? "-"
                : "_"),
      "",
    );
new WebSocket(hermesURL);

const lol = [
  {
    type: "subscribe",
    id: "QKAGl2_UcvexRQWjeGf6-",
    subscribe: {
      id: "XtCaMSn-2RJ7He-OE7Uw0",
      type: "pubsub",
      pubsub: { topic: "broadcast-settings-update.112206400" },
    },
    timestamp: "2026-09-05T08:51:19.121Z",
  },
  {
    type: "subscribe",
    id: "kJHJwRXVrNAaIbBOXiRRO", // request id
    subscribe: {
      id: "RmiP0jG-U-e-7-hgCpaST", // subscription id
      type: "pubsub",
      pubsub: { topic: "video-playback-by-id.112206400" }, //
    },
    timestamp: "2026-09-05T05:39:33.324Z",
  },
  {
    subscribeResponse: { subscription: { id: "RmiP0jG-U-e-7-hgCpaST" }, result: "ok" },
    id: "d68fb462-6a71-4e5e-bdf6-e6d4c797a6d9",
    parentId: "kJHJwRXVrNAaIbBOXiRRO",
    type: "subscribeResponse",
    timestamp: "2026-09-05T05:39:33.35198013Z",
  },

  {
    notification: {
      subscription: { id: "RmiP0jG-U-e-7-hgCpaST" },
      type: "pubsub",
      pubsub: '{"server_time":1788586783,"play_delay":0,"type":"stream-up"}',
    },
    id: "d9243ca0-e5a3-5dd0-8652-5fb548fc7797-msg0-topic0RmiP0jG-U-e-7-hgCpaST",
    type: "notification",
    timestamp: "2026-09-05T05:39:43.451501446Z",
  },

  {
    notification: {
      subscription: { id: "RmiP0jG-U-e-7-hgCpaST" },
      type: "pubsub",
      pubsub: '{"server_time":1788586791,"type":"stream-down"}',
    },
    id: "afa17178-9655-50a9-9df5-126d3b329286-msg0-topic0RmiP0jG-U-e-7-hgCpaST",
    type: "notification",
    timestamp: "2026-09-05T05:39:51.957875035Z",
  },

  {
    type: "unsubscribe",
    id: "my72CCZaBBD0gsGtCjLdc",
    unsubscribe: { id: "RmiP0jG-U-e-7-hgCpaST" },
    timestamp: "2026-09-05T05:44:32.927Z",
  },
  {
    unsubscribeResponse: { subscription: { id: "RmiP0jG-U-e-7-hgCpaST" }, result: "ok" },
    id: "16035e25-d690-4cec-8905-8396988e0232",
    parentId: "my72CCZaBBD0gsGtCjLdc",
    type: "unsubscribeResponse",
    timestamp: "2026-09-05T05:44:32.959946885Z",
  },
];
