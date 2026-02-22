#!/usr/bin/env bash
set -euo pipefail

# Where to write the generated config
CONFIG_PATH="${CONFIG_PATH:-/app/.credentials.toml}"

# --- helpers ---
die() { echo "ERROR: $*" >&2; exit 1; }

require_env() {
  local name="$1"
  if [[ -z "${!name:-}" ]]; then
    die "$name not set"
  fi
}

# --- required ---
require_env DISCORD_TOKEN
require_env TEAMSPEAK_SERVER
require_env TEAMSPEAK_IDENTITY

# --- optional defaults ---
TEAMSPEAK_NAME="${TEAMSPEAK_NAME:-VoiceBridge Bot}"
VERBOSE="${VERBOSE:-0}"     # 0-3
VOLUME="${VOLUME:-1.0}"     # 0.0-2.0

# --- generate base config ---
mkdir -p "$(dirname "$CONFIG_PATH")"

cat > "$CONFIG_PATH" <<EOF
discord_token = "$(printf '%s' "$DISCORD_TOKEN" | sed 's/"/\\"/g')"
teamspeak_server = "$(printf '%s' "$TEAMSPEAK_SERVER" | sed 's/"/\\"/g')"
teamspeak_identity = "$(printf '%s' "$TEAMSPEAK_IDENTITY" | sed 's/"/\\"/g')"
EOF

# Channel selection (name OR id). If both are set, prefer ID.
if [[ -n "${TEAMSPEAK_CHANNEL_ID:-}" ]]; then
  echo "teamspeak_channel_id = ${TEAMSPEAK_CHANNEL_ID}" >> "$CONFIG_PATH"
elif [[ -n "${TEAMSPEAK_CHANNEL_NAME:-}" ]]; then
  echo "teamspeak_channel_name = \"$(printf '%s' "$TEAMSPEAK_CHANNEL_NAME" | sed 's/"/\\"/g')\"" >> "$CONFIG_PATH"
fi

# Optional settings header
cat >> "$CONFIG_PATH" <<EOF

# Optional settings
teamspeak_name = "$(printf '%s' "$TEAMSPEAK_NAME" | sed 's/"/\\"/g')"
verbose = ${VERBOSE}  # 0-3, higher = more logs
volume = ${VOLUME}  # Default volume (0.0-2.0)
EOF

# Optional passwords
if [[ -n "${TEAMSPEAK_SERVER_PASSWORD:-}" ]]; then
  echo "teamspeak_server_password = \"$(printf '%s' "$TEAMSPEAK_SERVER_PASSWORD" | sed 's/"/\\"/g')\"" >> "$CONFIG_PATH"
fi
if [[ -n "${TEAMSPEAK_CHANNEL_PASSWORD:-}" ]]; then
  echo "teamspeak_channel_password = \"$(printf '%s' "$TEAMSPEAK_CHANNEL_PASSWORD" | sed 's/"/\\"/g')\"" >> "$CONFIG_PATH"
fi

echo "Generated config at: $CONFIG_PATH"
echo "----"
sed 's/discord_token = ".*"/discord_token = "***REDACTED***"/' "$CONFIG_PATH"

# run app
/app/voice_bridge