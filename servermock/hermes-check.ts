/**
 * Smoke-check the Hermes mock's wire contract with Node's built-in WebSocket:
 *
 *   pnpm dev                 # in one terminal
 *   pnpm exec node hermes-check.ts
 */
import assert from "node:assert/strict";

const ws = new WebSocket("ws://127.0.0.1:8001/v1");
const messages: Record<string, unknown>[] = [];
let subId: string | undefined;

const deadline = setTimeout(() => {
  console.error("hermes-check: timed out", messages);
  process.exit(1);
}, 5000);

ws.addEventListener("open", () => {
  ws.send(
    JSON.stringify({ type: "subscribe", topics: ["video-playback-by-id.1"], nonce: "n1" }),
  );
  ws.send(JSON.stringify({ type: "ping" }));
});

ws.addEventListener("message", (event) => {
  const message = JSON.parse(String(event.data)) as Record<string, any>;
  messages.push(message);
  switch (message.type) {
    case "welcome":
      assert.equal(message.welcome.keepaliveSec, 15);
      break;
    case "pong":
      break;
    case "subscribeResponse":
      assert.equal(message.result, "ok");
      assert.equal(message.nonce, "n1");
      assert.equal(message.subscription.topic, "video-playback-by-id.1");
      subId = String(message.subscription.id);
      assert.equal(subId.length, 21);
      break;
    case "notification": {
      const pubsub = JSON.parse(message.notification.pubsub) as Record<string, unknown>;
      assert.equal(pubsub.type, "stream-up");
      assert.ok(subId);
      assert.equal(message.notification.subscription.id, subId);
      clearTimeout(deadline);
      console.log("hermes-check: ok");
      ws.close();
      process.exit(0);
    }
    default:
      break;
  }
});

ws.addEventListener("error", (error) => {
  clearTimeout(deadline);
  console.error("hermes-check: socket error", error);
  process.exit(1);
});
