import toasted from "toasted-notifier";
const HERMES_URL = "wss://hermes.twitch.tv/v1?clientId=kimne78kx3ncx6brgo4mv6wki5h1ko";
const GQL_URL = "https://gql.twitch.tv/gql";
const CLIENT_ID = "kimne78kx3ncx6brgo4mv6wki5h1ko";

toasted.notify(
  {
    title: "Notification Title",
    message: "This is a message",

    sound: true, // Only Notification Center or Windows Toasters
    wait: true, // Wait on callback until user interacts, does not apply to Windows Toasters as they always wait or notify-send as it does not support the wait option
  },
  (error, response, metadata) => {
    // Response is response from notification
    // Metadata contains activationType, activationAt, deliveredAt
  },
);

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
  | PubSubNotification;

class Hermes {
  subscriptions: Map<SubscriptionId, Subscription>;
  ws: WebSocket;
  ready: Promise<void>;

  constructor() {
    this.subscriptions = new Map();
    this.ws = new WebSocket(HERMES_URL);
    this.ready = new Promise((resolve) => this.ws.addEventListener("open", () => resolve()));
    this.ws.addEventListener("message", (event) => this.onMessage(event));
  }

  async subscribe<T extends Topic>(topic: T, callback: Subscription<T>["callback"]) {
    console.log("awaiting ready");
    await this.ready;
    const message_id = NanoId();
    const subscription_id = NanoId();
    this.subscriptions.set(subscription_id as SubscriptionId, {
      topic,
      state: "pending",
      callback: callback as Subscription["callback"],
    });
    console.log("sent");
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

  private onMessage(event: MessageEvent) {
    const message = JSON.parse(String(event.data)) as HermesMessage;
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

interface ChannelArg {
  id: null | number;
  name: null | string;
}

function parseChannelArgs(args: string[]): ChannelArg[] {
  const channels: ChannelArg[] = [];
  for (const arg of args) {
    const separator = arg.lastIndexOf(":");
    if (separator !== -1) {
      const name = arg.slice(0, separator);
      const id = Number(arg.slice(separator + 1));
      if (!name || !Number.isInteger(id)) {
        throw new Error(`bad channel arg "${arg}", expected name:id`);
      }
      channels.push({ id, name });
    } else if (/^\d+$/.test(arg)) {
      channels.push({ id: Number(arg), name: null });
    } else if (arg) {
      channels.push({ id: null, name: arg });
    } else {
      throw new Error(`bad channel arg "${arg}", expected name:id, name, or id`);
    }
  }
  return channels;
}

async function main() {
  const channels = parseChannelArgs(process.argv.slice(2));
  if (channels.length === 0) {
    console.error("usage: node hermesnotifier.ts (name:id | name | id) ...");
    return;
  }

  const users = await fetchUsers(
    channels.filter((c): c is ChannelArg & { id: number } => c.id !== null).map((c) => c.id),
    channels.filter((c): c is ChannelArg & { name: string } => c.name !== null).map((c) => c.name),
  );

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
        idToStreamInfo.set(info.channelId, toStreamInfo(user));
        console.log(`[${info.channelName}] stream-up | ${info.streamTitle} (${info.gameName})`);
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
