import {
  createMemo,
  createResource,
  createSignal,
  For,
  onCleanup,
  onMount,
  Show,
} from "solid-js";
import { createStore, reconcile } from "solid-js/store";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import "./App.css";

type ChannelConfig = {
  login: string;
  id: number | null;
  displayName: string | null;
};

type Config = {
  channels: ChannelConfig[];
  notifyTitleChanges: boolean;
  sound: boolean;
};

type SubStatus = "pending" | "connected" | "failed";

type ChannelStatus = {
  channelId: number;
  login: string;
  displayName: string;
  titleStatus: SubStatus;
  liveStatus: SubStatus;
};

type HermesStatus = {
  connected: boolean;
  error: string | null;
  channels: ChannelStatus[];
  unresolved: string[];
};

function normalizeLogin(raw: string): string {
  return raw.trim().toLowerCase();
}

export default function App() {
  const [config, { refetch }] = createResource(async () =>
    invoke<Config>("get_config"),
  );
  const [status, setStatus] = createStore<HermesStatus>({
    connected: false,
    error: null,
    channels: [],
    unresolved: [],
  });
  const [newLogin, setNewLogin] = createSignal("");
  const [error, setError] = createSignal("");

  onMount(async () => {
    try {
      const unlisten = await listen<HermesStatus>("hermes-status", (event) =>
        setStatus(reconcile(event.payload, { key: "channelId" })),
      );
      onCleanup(() => unlisten());
      const snapshot = await invoke<HermesStatus | null>("get_status");
      if (snapshot) setStatus(reconcile(snapshot, { key: "channelId" }));
    } catch (err) {
      setError(String(err));
    }
  });

  // the saved config in Rust is the source of truth; the resource refetches
  // after every mutation so the ui always mirrors it
  async function saveConfig(next: Config) {
    setError("");
    try {
      await invoke("save_config", { config: next });
      await refetch();
    } catch (err) {
      setError(String(err));
    }
  }

  function addChannel(event: SubmitEvent) {
    event.preventDefault();
    const current = config();
    const login = normalizeLogin(newLogin());
    setNewLogin("");
    if (!login || !current || current.channels.some((c) => c.login === login)) {
      return;
    }
    saveConfig({
      ...current,
      channels: [...current.channels, { login, id: null, displayName: null }],
    });
  }

  function removeChannel(login: string) {
    const current = config();
    if (!current) return;
    saveConfig({
      ...current,
      channels: current.channels.filter((channel) => channel.login !== login),
    });
  }

  function toggle(key: "notifyTitleChanges" | "sound", value: boolean) {
    const current = config();
    if (current) saveConfig({ ...current, [key]: value });
  }

  return (
    <main class="container">
      <header>
        <h1>Siphon</h1>
        <span
          class="connection"
          data-state={
            status.connected ? "ok" : (status.error ? "fail" : "pending")
          }
        >
          {status.connected ? "Connected" : (status.error ?? "Connecting\u2026")}
        </span>
      </header>

      <div class="channel-list">
        <div class="channel-row head">
          <span />
          <span class="col-label">Live Status</span>
          <span class="col-label">Title Status</span>
          <span />
        </div>
        <For each={config()?.channels ?? []}>
          {(channel) => <ChannelRow login={channel.login} onRemove={removeChannel} />}
        </For>
        <Show when={(config()?.channels.length ?? 0) === 0}>
          <p class="empty">No channels configured. Add a streamer below.</p>
        </Show>
      </div>

      <form class="add" onSubmit={addChannel}>
        <input
          placeholder="Add new streamer"
          value={newLogin()}
          spellcheck={false}
          onInput={(e) => setNewLogin(e.currentTarget.value)}
        />
        <button type="submit">Add</button>
      </form>

      <label class="check">
        <input
          type="checkbox"
          checked={config()?.notifyTitleChanges ?? true}
          onChange={(e) => toggle("notifyTitleChanges", e.currentTarget.checked)}
        />
        Notify on title changes while offline
      </label>
      <label class="check">
        <input
          type="checkbox"
          checked={config()?.sound ?? true}
          onChange={(e) => toggle("sound", e.currentTarget.checked)}
        />
        Notification sound
      </label>

      <Show when={error()}>
        <p class="error">{error()}</p>
      </Show>
    </main>
  );

  function ChannelRow(props: { login: string; onRemove: (login: string) => void }) {
    // display name priority: live status event > saved config > login
    const configName = createMemo(
      () =>
        config()?.channels.find((channel) => channel.login === props.login)
          ?.displayName ?? null,
    );
    const resolved = createMemo(
      () => status.channels.find((channel) => channel.login === props.login),
    );
    const notFound = createMemo(() => status.unresolved.includes(props.login));
    return (
      <div class="channel-row">
        <span class="name" classList={{ notfound: notFound() }}>
          {resolved()?.displayName || configName() || props.login}
        </span>
        <Badge
          state={notFound() ? "failed" : (resolved()?.liveStatus ?? "pending")}
          text={notFound() ? "Not found" : undefined}
        />
        <Badge
          state={notFound() ? "failed" : (resolved()?.titleStatus ?? "pending")}
          text={notFound() ? "Not found" : undefined}
        />
        <button
          class="remove"
          title={`Remove ${props.login}`}
          onClick={() => props.onRemove(props.login)}
        >
          &#215;
        </button>
      </div>
    );
  }
}

function Badge(props: { state: SubStatus; text?: string }) {
  const text = () => {
    if (props.text) return props.text;
    switch (props.state) {
      case "connected":
        return "Connected";
      case "failed":
        return "Failed";
      case "pending":
        return "Pending";
    }
  };
  return (
    <span class="badge" data-state={props.state}>
      {text()}
    </span>
  );
}
