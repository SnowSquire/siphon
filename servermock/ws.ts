const EVENTSUB_URL = "wss://eventsub.wss.twitch.tv/ws";
const CLIENT_ID = process.env.TWITCH_CLIENT_ID ?? "";
import fs from "node:fs/promises";
// Device-code grant: https://dev.twitch.tv/docs/authentication/getting-tokens-oauth/#device-code-grant-flow
async function main() {
  const accessToken = await authorizeDevice();
  const ws = new WebSocket(EVENTSUB_URL);
  ws.addEventListener("open", (ev) => {
    console.log("connected");
  });
  let sessionId;
  ws.addEventListener("message", (data) => {
    const message = JSON.parse(data.data);
    const metadata = message.metadata;
    const payload = message.payload;

    if (metadata.message_type === "session_welcome") {
      sessionId = payload.session.id;
      console.log(`Received Session ID: ${sessionId}`);
      console.log("👉 Use this Session ID to create subscriptions via Twitch's API!");
      console.log(`Sending sub with ${accessToken.access_token}`);
      void fetch("https://api.twitch.tv/helix/eventsub/subscriptions", {
        headers: {
          "Client-ID": CLIENT_ID,
          Authorization: `Bearer ${accessToken.access_token}`,
          "Content-Type": "application/json",
        },
        method: "POST",
        body: JSON.stringify({
          type: "stream.online",
          version: "1",
          condition: {
            broadcaster_user_id: "112206400",
          },
          transport: {
            method: "websocket",
            session_id: sessionId,
          },
        }),
      });

      void fetch("https://api.twitch.tv/helix/eventsub/subscriptions", {
        headers: {
          "Client-ID": CLIENT_ID,
          Authorization: `Bearer ${accessToken.access_token}`,
          "Content-Type": "application/json",
        },
        method: "POST",
        body: JSON.stringify({
          type: "channel.update",
          version: "2",
          condition: {
            broadcaster_user_id: "112206400",
          },
          transport: {
            method: "websocket",
            session_id: sessionId,
          },
        }),
      });
      void fetch("https://api.twitch.tv/helix/eventsub/subscriptions", {
        headers: {
          "Client-ID": CLIENT_ID,
          Authorization: `Bearer ${accessToken.access_token}`,
          "Content-Type": "application/json",
        },
        method: "POST",
        body: JSON.stringify({
          type: "stream.offline",
          version: "1",
          condition: {
            broadcaster_user_id: "112206400",
          },
          transport: {
            method: "websocket",
            session_id: sessionId,
          },
        }),
      });
    }

    if (metadata.message_type === "notification") {
      console.log(`Received event type: ${payload.subscription.type} @ ${new Date().toString()}`);

      console.log("Event details:", payload.event);
    }
  });
}

async function authorizeDevice() {
  // -----------------------------------------------------------------
  // Step 1: Try reading existing tokens from disk
  // -----------------------------------------------------------------
  try {
    const fileData = await fs.readFile("./tokens.json", "utf8");
    const savedTokens = JSON.parse(fileData);

    if (savedTokens.access_token) {
      // Validate the saved access token with Twitch
      const validateResponse = await fetch("https://id.twitch.tv/oauth2/validate", {
        headers: { Authorization: `OAuth ${savedTokens.access_token}` },
      });

      if (validateResponse.ok) {
        console.log("Loaded valid access token from tokens.json!");
        return savedTokens;
      }

      // Token expired: Attempt to refresh using the stored refresh_token
      if (savedTokens.refresh_token) {
        console.log("Access token expired. Refreshing token using stored refresh_token...");
        const refreshResponse = await fetch("https://id.twitch.tv/oauth2/token", {
          method: "POST",
          headers: { "Content-Type": "application/x-www-form-urlencoded" },
          body: new URLSearchParams({
            client_id: CLIENT_ID,
            grant_type: "refresh_token",
            refresh_token: savedTokens.refresh_token,
          }),
        });

        const refreshedData = await refreshResponse.json();

        if (refreshResponse.ok) {
          const updatedTokens = {
            access_token: refreshedData.access_token,
            refresh_token: refreshedData.refresh_token,
          };

          await fs.writeFile("./tokens.json", JSON.stringify(updatedTokens, null, 2));
          console.log("Successfully refreshed token and updated tokens.json!");
          return updatedTokens;
        }

        console.log("Refresh token invalid or expired. Falling back to Device Code authorization.");
      }
    }
  } catch (err) {
    // File doesn't exist or JSON is invalid - proceed to initial device authorization
    console.log("No valid tokens.json found. Starting interactive Device Authorization...");
  }

  // -----------------------------------------------------------------
  // Step 2: Fallback to Device Code Flow (If disk reading/refresh fails)
  // -----------------------------------------------------------------
  const deviceResponse = await fetch("https://id.twitch.tv/oauth2/device", {
    method: "POST",
    headers: { "Content-Type": "application/x-www-form-urlencoded" },
    body: new URLSearchParams({
      client_id: CLIENT_ID,
      scopes: "", // Add scopes here if needed
    }),
  });

  const deviceData = await deviceResponse.json();

  if (!deviceResponse.ok) {
    throw new Error(`Device code request failed: ${JSON.stringify(deviceData)}`);
  }

  console.log("\n======================================================");
  console.log(`1. Go to: ${deviceData.verification_uri}`);
  console.log(`2. Enter Code: ${deviceData.user_code}`);
  console.log("======================================================\n");

  const interval = (deviceData.interval || 5) * 1000;

  while (true) {
    await new Promise((resolve) => setTimeout(resolve, interval));

    const tokenResponse = await fetch("https://id.twitch.tv/oauth2/token", {
      method: "POST",
      headers: { "Content-Type": "application/x-www-form-urlencoded" },
      body: new URLSearchParams({
        client_id: CLIENT_ID,
        device_code: deviceData.device_code,
        grant_type: "urn:ietf:params:oauth:grant-type:device_code",
      }),
    });

    const tokenData = await tokenResponse.json();

    if (tokenResponse.ok) {
      const tokensToSave = {
        access_token: tokenData.access_token,
        refresh_token: tokenData.refresh_token,
      };

      await fs.writeFile("./tokens.json", JSON.stringify(tokensToSave, null, 2));
      console.log("Successfully authorized and saved tokens.json!");
      return tokensToSave;
    }

    if (tokenData.message === "authorization_pending") {
      process.stdout.write(".");
      continue;
    }

    throw new Error(`Authorization failed: ${tokenData.message}`);
  }
}
if (import.meta.main) {
  main();
}
