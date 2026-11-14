import { execFileSync, spawn } from "node:child_process";
import { readFileSync } from "node:fs";
import { createRequire } from "node:module";
import { join } from "node:path";

const require = createRequire(import.meta.url);
// Built from notifier-rs/ (notify-rust wrapped with napi): see "build:notifier" in package.json.
const rawNotify = require("./notifier-rs/notifier_rs.node") as {
  notify: (
    summary: string,
    body: string,
    // napi-rs maps Rust snake_case fields to camelCase on the JS side.
    options?: {
      subtitle?: string | null;
      appId?: string | null;
      sound?: boolean | null;
      timeoutMs?: number | null;
    },
    onClick?: () => void,
  ) => void;
};

type Config = {
  channels: string[];
  channelIds: number[];
  notifyTitleChanges: boolean;
  sound: boolean;
};

const config: Config = {
  channels: [],
  channelIds: [],
  notifyTitleChanges: true,
  sound: true,
  ...JSON.parse(readFileSync(join(import.meta.dirname, "config.json"), "utf8")),
};

// Windows toasts must be attributed to a registered AUMID, otherwise the addon
// falls back to the PowerShell identity ("Windows PowerShell" on the toast).
// https://learn.microsoft.com/en-us/windows/apps/design/shell/tiles-and-notifications/send-local-toast-other-apps
const AUMID = "twitch-siphon";

function openStream(channel: string) {
  spawn("explorer", [`https://twitch.tv/${channel}`], { detached: true, stdio: "ignore" }).unref();
}

function notify(summary: string, body: string, channel?: string) {
  rawNotify.notify(
    summary,
    body,
    { appId: AUMID, sound: config.sound },
    channel === undefined ? undefined : () => openStream(channel),
  );
}

if (process.platform === "win32") {
  execFileSync(
    "reg",
    [
      "add",
      `HKCU\\Software\\Classes\\AppUserModelId\\${AUMID}`,
      "/v",
      "DisplayName",
      "/t",
      "REG_EXPAND_SZ",
      "/d",
      "Siphon",
      "/f",
    ],
    { stdio: "ignore" },
  );
}
const HERMES_URL = "wss://hermes.twitch.tv/v1?clientId=kimne78kx3ncx6brgo4mv6wki5h1ko";
const GQL_URL = "https://gql.twitch.tv/gql";
const CLIENT_ID = "kimne78kx3ncx6brgo4mv6wki5h1ko";

const USERS_BY_IDS_QUERY = `query UsersByIds($ids:[ID!]){users(ids:$ids){id login displayName broadcastSettings{id title game{id name displayName}}stream{id createdAt}}}`;

const USERS_BY_LOGINS_QUERY = `query UsersByLogins($logins:[String!]){users(logins:$logins){id login displayName broadcastSettings{id title game{id name displayName}}stream{id createdAt}}}`;

type SubscriptionId = string & { readonly __brand: unique symbol };

type BroadcastSettingsPayload = {
  channel_id: string;
  type: "broadcast_settings_update";
  channel: string;
  old_status: string;
  status: string;
  old_game: string;
  game: string;
  old_game_id: number;
  game_id: number;
};
type StreamUpPayload = { server_time: number; play_delay: number; type: "stream-up" };
type StreamDownPayload = { server_time: number; type: "stream-down" };
type VideoPlaybackPayload = StreamUpPayload | StreamDownPayload;

interface TopicPayloads {
  "broadcast-settings-update": BroadcastSettingsPayload;
  "video-playback-by-id": VideoPlaybackPayload;
}
type TopicArguments = { [N in keyof TopicPayloads]: number };

type Topic = {
  [N in keyof TopicPayloads]: `${N & string}.${TopicArguments[N]}`;
}[keyof TopicPayloads];

type TopicPayload<T extends Topic> =
  T extends `${infer N extends keyof TopicPayloads & string}.${number}` ? TopicPayloads[N] : never;

type PubSubNotification = {
  notification: {
    subscription: { id: string };
    type: "pubsub";
    pubsub: string;
  };
  id: string; //"c0fef4c1-97f9-5331-b263-97d8c051ed58-msg0-topic03k4mq61FRn3T5qeQ6PMsC"
  type: "notification";
  timestamp: string;
};

type ParsedNotification<T extends Topic = Topic> = Omit<PubSubNotification, "notification"> & {
  notification: {
    subscription: { id: string };
    type: "pubsub";
    pubsub: TopicPayload<T>;
  };
};

function parsePubsub<T extends Topic>(notification: PubSubNotification): ParsedNotification<T> {
  return {
    ...notification,
    notification: {
      ...notification.notification,
      pubsub: JSON.parse(notification.notification.pubsub) as TopicPayload<T>,
    },
  };
}

type SubscriptionState = "pending" | "subscribed" | "pendingUnsubscribe";

interface Subscription<T extends Topic = Topic> {
  topic: T;
  state: SubscriptionState;
  callback: (e: ParsedNotification<T>) => void;
}
function NanoId(e = 21) {
  return crypto
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
}

type WelcomeMessage = {
  type: "welcome";
  welcome: { keepaliveSec: number; recoveryUrl: string; sessionId: string };
};

type HermesMessage =
  | {
      type: "subscribe";
      id: string;
      subscribe: {
        id: string;
        type: "pubsub";
        pubsub: { topic: Topic };
      };
      timestamp: string;
    }
  | {
      type: "unsubscribe";
      id: string;
      unsubscribe: { id: string };
      timestamp: string;
    }
  | {
      type: "subscribeResponse" | "unsubscribeResponse";
      id: string;
      parentId: string;
      subscribeResponse?: { subscription: { id: string }; result: "ok" };
      unsubscribeResponse?: { subscription: { id: string }; result: "ok" };
      timestamp: string;
    }
  | WelcomeMessage
  | { type: "keepalive" }
  | PubSubNotification;

class Hermes {
  subscriptions: Map<SubscriptionId, Subscription>;
  ws!: WebSocket;
  ready: Promise<void>;

  private resolveReady!: () => void;
  private recoveryUrl: string | null = null;
  private keepaliveSec = 15;
  private keepaliveTimer: ReturnType<typeof setTimeout> | null = null;
  private welcomeTimer: ReturnType<typeof setTimeout> | null = null;
  private currentUrl = HERMES_URL;
  private gotWelcome = false;
  private failedRecoveries = 0;
  private reconnectAttempt = 0;

  constructor() {
    this.subscriptions = new Map();
    this.ready = new Promise((resolve) => {
      this.resolveReady = resolve;
    });
    this.connect(HERMES_URL, false);
  }

  async subscribe<T extends Topic>(topic: T, callback: Subscription<T>["callback"]) {
    await this.ready;
    await this.whenOpen();
    const message_id = NanoId();
    const subscription_id = NanoId();
    this.subscriptions.set(subscription_id as SubscriptionId, {
      topic,
      state: "pending",
      callback: callback as Subscription["callback"],
    });

    this.ws.send(
      JSON.stringify({
        type: "subscribe",
        id: message_id,
        subscribe: {
          id: subscription_id,
          type: "pubsub",
          pubsub: { topic: topic },
        },
        timestamp: new Date().toISOString(),
      }),
    );
  }

  unsubscribe<T extends Topic>(topic: T) {
    for (const [id, subscription] of this.subscriptions) {
      if (subscription.state !== "subscribed" || subscription.topic !== topic) continue;
      subscription.state = "pendingUnsubscribe";
      this.ws.send(
        JSON.stringify({
          type: "unsubscribe",
          id: NanoId(),
          unsubscribe: { id: id },
          timestamp: new Date().toISOString(),
        }),
      );
    }
  }

  private connect(url: string, resubscribe: boolean) {
    this.currentUrl = url;
    this.gotWelcome = false;
    if (this.welcomeTimer !== null) clearTimeout(this.welcomeTimer);
    const ws = new WebSocket(url);
    this.ws = ws;
    // if the server never sends a welcome, treat the connection as dead
    this.welcomeTimer = setTimeout(() => ws.close(), 10_000);
    ws.addEventListener("message", (event) => this.onMessage(event));
    ws.addEventListener("close", () => this.onClose());
    if (resubscribe) ws.addEventListener("open", () => this.resubscribeAll());
  }

  private whenOpen(): Promise<void> {
    if (this.ws.readyState === WebSocket.OPEN) return Promise.resolve();
    return new Promise((resolve, reject) => {
      this.ws.addEventListener("open", () => resolve(), { once: true });
      this.ws.addEventListener("close", () => reject(new Error("hermes closed")), { once: true });
    });
  }

  private onClose() {
    if (this.keepaliveTimer !== null) {
      clearTimeout(this.keepaliveTimer);
      this.keepaliveTimer = null;
    }
    if (this.gotWelcome) {
      // The server keeps the session's subscription set alive, so reconnecting
      // through the recovery URL restores subscriptions without resubscribing.
      // A fresh welcome (with a new recovery URL) arrives after recovery.
      this.failedRecoveries = 0;
      this.scheduleReconnect(this.recoveryUrl, false);
      return;
    }
    if (this.currentUrl !== HERMES_URL) {
      // recovery attempt failed before welcome; try the recovery URL once more
      // before giving up on the session entirely
      this.failedRecoveries++;
      if (this.failedRecoveries < 2) {
        this.scheduleReconnect(this.recoveryUrl, false);
        return;
      }
    }
    // no session to recover: start fresh and replay all subscriptions
    this.scheduleReconnect(HERMES_URL, true);
  }

  private scheduleReconnect(url: string | null, resubscribe: boolean) {
    this.reconnectAttempt++;
    const delay = Math.min(500 * 2 ** (this.reconnectAttempt - 1), 15_000);
    console.log(
      `[hermes] disconnected, ${resubscribe ? "reconnecting fresh" : "recovering session"} in ${delay}ms`,
    );
    setTimeout(() => this.connect(url ?? HERMES_URL, resubscribe), delay);
  }

  private resubscribeAll() {
    for (const [id, subscription] of this.subscriptions) {
      if (subscription.state === "pendingUnsubscribe") {
        this.subscriptions.delete(id);
        continue;
      }
      subscription.state = "pending";
      this.ws.send(
        JSON.stringify({
          type: "subscribe",
          id: NanoId(),
          subscribe: {
            id: id,
            type: "pubsub",
            pubsub: { topic: subscription.topic },
          },
          timestamp: new Date().toISOString(),
        }),
      );
    }
  }

  private bumpKeepalive() {
    if (this.keepaliveTimer !== null) clearTimeout(this.keepaliveTimer);
    // consider the connection dead after two missed keepalives
    this.keepaliveTimer = setTimeout(() => this.ws.close(), this.keepaliveSec * 2_000);
  }

  private onMessage(event: MessageEvent) {
    const message = JSON.parse(String(event.data)) as HermesMessage;
    if (message.type === "welcome") {
      if (this.welcomeTimer !== null) {
        clearTimeout(this.welcomeTimer);
        this.welcomeTimer = null;
      }
      this.gotWelcome = true;
      this.reconnectAttempt = 0;
      this.keepaliveSec = message.welcome.keepaliveSec;
      this.recoveryUrl = message.welcome.recoveryUrl;
      this.bumpKeepalive();
      this.resolveReady();
      return;
    }
    if (message.type === "keepalive") {
      this.bumpKeepalive();
      return;
    }
    if (message.type === "subscribeResponse") {
      const id = message.subscribeResponse?.subscription.id as SubscriptionId | undefined;
      if (id === undefined) return;
      const subscription = this.subscriptions.get(id);
      if (subscription?.state !== "pending") return;
      if (message.subscribeResponse?.result !== "ok") {
        this.subscriptions.delete(id);
        return;
      }
      subscription.state = "subscribed";
      return;
    }
    if (message.type === "unsubscribeResponse") {
      const id = message.unsubscribeResponse?.subscription.id as SubscriptionId | undefined;
      if (id === undefined) return;
      const subscription = this.subscriptions.get(id);
      if (subscription?.state !== "pendingUnsubscribe") return;
      if (message.unsubscribeResponse?.result !== "ok") {
        subscription.state = "subscribed";
        return;
      }
      this.subscriptions.delete(id);
      return;
    }
    if (message.type !== "notification") return;
    const subscription = this.subscriptions.get(
      message.notification.subscription.id as SubscriptionId,
    );
    if (subscription?.state === "subscribed") subscription.callback(parsePubsub(message));
  }
}

type StreamInfo = {
  gameId: number | null;
  gameName: string | null;
  gameDisplayName: string | null;
  streamId: number;
  streamTitle: string;
  streamStart: Date | null;
  channelId: number;
  channelName: string;
  channelDisplayName: string;
  live: boolean;
};

interface RawUser {
  id: string;
  login: string;
  displayName: string;
  broadcastSettings: null | {
    id: string;
    title: string;
    game: null | { id: string; name: string; displayName: string };
  };
  stream: null | { id: string; createdAt: string };
}

interface RawUsersResponse {
  data: { users: RawUser[] };
}

async function fetchUsers(ids: number[], logins: string[]): Promise<RawUser[]> {
  if (ids.length === 0 && logins.length === 0) return [];

  const operations = [];
  if (ids.length > 0) {
    operations.push({
      query: USERS_BY_IDS_QUERY,
      variables: { ids: ids.map(String) },
      operationName: "UsersByIds",
    });
  }
  if (logins.length > 0) {
    operations.push({
      query: USERS_BY_LOGINS_QUERY,
      variables: { logins },
      operationName: "UsersByLogins",
    });
  }

  const res = await fetch(GQL_URL, {
    method: "POST",
    headers: { "Client-ID": CLIENT_ID },
    body: JSON.stringify(operations),
  });
  if (!res.ok) throw new Error(`gql request failed: ${res.status}`);
  const responses = (await res.json()) as RawUsersResponse[];

  return responses.flatMap((response) => response.data.users);
}

function toStreamInfo(user: RawUser): StreamInfo {
  return {
    gameId: Number(user.broadcastSettings?.game?.id ?? null),
    gameName: user.broadcastSettings?.game?.name ?? null,
    gameDisplayName: user.broadcastSettings?.game?.displayName ?? null,
    streamId: Number(user.stream?.id ?? user.broadcastSettings?.id),
    streamTitle: user.broadcastSettings?.title ?? "",
    channelId: Number(user.id),
    channelName: user.login,
    channelDisplayName: user.displayName,
    streamStart: user.stream?.createdAt ? new Date(user.stream.createdAt) : null,
    live: user.stream !== null,
  };
}

async function main() {
  const users = await fetchUsers(config.channelIds, config.channels);

  const idToStreamInfo = new Map<number, StreamInfo>();
  for (const rawuser of users) {
    const info = toStreamInfo(rawuser);
    idToStreamInfo.set(info.channelId, info);

    console.log(
      `[${info.channelName}] ${info.live ? "LIVE" : "offline"} | ${info.streamTitle} (${info.gameName})`,
    );
  }

  const hermes = new Hermes();

  for (const info of idToStreamInfo.values()) {
    const name = info.channelName;
    await hermes.subscribe(`broadcast-settings-update.${info.channelId}`, (e) => {
      const pubsub = e.notification.pubsub;
      const info2 = idToStreamInfo.get(info.channelId);
      if (info2) {
        const titleChanged = pubsub.status !== info2.streamTitle;
        const gameChanged = (pubsub.game || null) !== info2.gameName;
        if (config.notifyTitleChanges && !info2.live && (titleChanged || gameChanged)) {
          const lines = [];
          if (titleChanged) lines.push(`${pubsub.old_status} → ${pubsub.status}`);
          if (gameChanged) lines.push(`${pubsub.old_game} → ${pubsub.game}`);
          notify(
            `[${info2.channelDisplayName}] ${[titleChanged && "title", gameChanged && "game"]
              .filter(Boolean)
              .join(" & ")} changed`,
            lines.join("\n"),
            info2.channelName,
          );
        }
        info2.streamTitle = pubsub.status;
        info2.gameName = pubsub.game || null;
        info2.gameId = pubsub.game_id || null;
      }
      console.log(
        `[${name}] ${pubsub.old_status} -> ${pubsub.status} | ${pubsub.old_game} -> ${pubsub.game}`,
      );
    });
    await hermes.subscribe(`video-playback-by-id.${info.channelId}`, async (e) => {
      if (e.notification.pubsub.type === "stream-up") {
        const [user] = await fetchUsers([info.channelId], []);
        const fresh = toStreamInfo(user);
        idToStreamInfo.set(info.channelId, fresh);
        notify(
          `[${fresh.channelDisplayName}] is LIVE`,
          `${fresh.streamTitle}${fresh.gameName ? ` — ${fresh.gameName}` : ""}`,
          fresh.channelName,
        );
        console.log(`[${info.channelName}] stream-up | ${fresh.streamTitle} (${fresh.gameName})`);
      } else if (e.notification.pubsub.type === "stream-down") {
        const info2 = idToStreamInfo.get(info.channelId);
        if (info2) info2.live = false;
        console.log(`[${name}] stream-down`);
      }
    });
  }
}

if (process.argv[1] === import.meta.filename) {
  main();
}
