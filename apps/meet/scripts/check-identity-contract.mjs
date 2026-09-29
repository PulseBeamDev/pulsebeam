import { readFileSync } from "node:fs";

const page = readFileSync("app/page.tsx", "utf8");
const lobby = readFileSync("components/Lobby.tsx", "utf8");
const room = readFileSync("components/Room.tsx", "utf8");
const config = readFileSync("lib/config.ts", "utf8");
const sources = [page, lobby, room, config].join("\n");
const identityInputs = [page, lobby, config].join("\n");

function requireMatch(source, pattern, message) {
  if (!pattern.test(source)) throw new Error(message);
}

function rejectMatch(source, pattern, message) {
  if (pattern.test(source)) throw new Error(message);
}

requireMatch(
  lobby,
  /const \[token, setToken\] = useState\(""\)/,
  "Lobby must keep the opaque token only in component state",
);
requireMatch(
  lobby,
  /const \[serverURL, setServerURL\] = useState\(defaultServerUrl\)/,
  "Lobby must expose server URL state",
);
requireMatch(
  lobby,
  /type="password"[\s\S]*?value=\{token\}/,
  "Token must use a password control",
);
requireMatch(
  lobby,
  /onJoin\(token, endpoint, isCamOn, isMicOn\)/,
  "Lobby must pass the token opaquely to the room",
);
requireMatch(
  room,
  /useAgent\(\{[\s\S]*?endpoint,[\s\S]*?token,[\s\S]*?topology:/,
  "Room must own an agent with server endpoint and token",
);
requireMatch(
  room,
  /agent\.participantExternalId \?\? "Joining…"/,
  "Room must display the authoritative external participant identity",
);
requireMatch(
  room,
  /agent\.roomExternalId/,
  "Room must display the authoritative external room identity",
);
rejectMatch(
  sources,
  /agent\.(?:participantId|roomId)\b/,
  "Meet must not display canonical agent identity",
);
rejectMatch(
  identityInputs,
  /\b(?:roomId|setRoomId|participantId|setParticipantId)\b/,
  "Meet must not construct room or participant identity inputs",
);
rejectMatch(
  sources,
  /localStorage|sessionStorage|indexedDB|document\.cookie|URLSearchParams/,
  "Meet must not persist or encode the token in a URL",
);
rejectMatch(
  sources,
  /console\.(?:debug|info|log|warn|error|trace)/,
  "Meet must not log client configuration",
);
rejectMatch(
  config,
  /\/api\/v1/,
  "Meet defaults must be server URLs, not signaling URLs",
);
