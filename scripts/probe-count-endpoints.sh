#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Andrew Stevens
#
# Probe what the providers' token-count endpoints actually accept and how they
# refuse, so `vertex_count_payload` and `count_refusal` in src/providers.rs are
# built from evidence rather than from a plausible reading of the docs.
#
# This is not a test you run in CI. It calls real provider APIs and needs real
# credentials. Run it when changing either function, or when a provider changes
# an endpoint. It has already found three bugs that review did not:
#
#   - Anthropic `count_tokens` rejects `max_tokens`, which every Messages
#     request carries, so posting the client's body unchanged returned
#     `400 max_tokens: Extra inputs are not permitted` on every request and
#     exact admission refused all Anthropic traffic.
#   - Vertex `:countTokens` has no `toolConfig` field, so an allowlist that
#     included it would have refused every request carrying one.
#   - Vertex counts a `responseSchema` inside `generationConfig` as prompt.
#     That field had been dropped on the reasoning that generationConfig
#     describes the output, which under-reserved every structured-output
#     request. Reasoning about which fields "cannot" affect an input count is
#     how two of these three got written. Measure instead.
#
# Usage:
#   PROJECT=<gcp-project> ./scripts/probe-count-endpoints.sh
#   ANTHROPIC_API_KEY=sk-ant-... ./scripts/probe-count-endpoints.sh
#   PROJECT=... ANTHROPIC_API_KEY=... LOCATION=us-central1 ./scripts/probe-count-endpoints.sh
#
# Each half is skipped if its credential is absent. Needs curl, jq, and for the
# Vertex half a gcloud login with access to the project.

set -uo pipefail

LOCATION="${LOCATION:-us-central1}"
VERTEX_MODEL="${VERTEX_MODEL:-gemini-2.5-flash}"
ANTHROPIC_MODEL="${ANTHROPIC_MODEL:-claude-sonnet-4-5}"

work="$(mktemp -d)"; trap 'rm -rf "$work"' EXIT

# Payloads go in files, never in argv: a realistic prompt exceeds ARG_MAX and
# the failure then looks like a network error rather than a shell limit.
post() { # url, auth-header, extra-header, file -> "CODE<TAB>summary"
  local out="$work/.out" code
  code="$(curl -s -o "$out" -w '%{http_code}' -X POST "$1" \
    -H "$2" -H "$3" -H 'content-type: application/json' --data-binary "@$4")"
  printf '%s\t%s' "$code" \
    "$(jq -c '.error.message // .error.type // {totalTokens, input_tokens}' < "$out" 2>/dev/null | head -c 130)"
}

vertex_url() {
  echo "https://${LOCATION}-aiplatform.googleapis.com/v1/projects/${PROJECT}/locations/${LOCATION}/publishers/google/models/$1:countTokens"
}

if [ -n "${PROJECT:-}" ] && command -v gcloud >/dev/null; then
  TOKEN="$(gcloud auth print-access-token 2>/dev/null)"
  if [ -z "$TOKEN" ]; then
    # Without this the token is empty, every Vertex line answers 401, and the
    # legend below only explains 400 and 404, so the run reads like a finding.
    echo "### Vertex: skipped (gcloud has no access token; run 'gcloud auth login')"
    echo
    PROJECT=""
  fi
fi

if [ -n "${PROJECT:-}" ]; then
  CONTENTS='"contents":[{"role":"user","parts":[{"text":"hello"}]}]'

  echo "### Vertex :countTokens, which fields are accepted"
  echo "# Field names are validated BEFORE the model is looked up, so against a"
  echo "# nonexistent model: 400 means the field is rejected, 404 means accepted."
  for field in \
    'systemInstruction:{"parts":[{"text":"be brief"}]}' \
    'system_instruction:{"parts":[{"text":"be brief"}]}' \
    'tools:[{"functionDeclarations":[{"name":"f","description":"d","parameters":{"type":"object","properties":{}}}]}]' \
    'toolConfig:{"functionCallingConfig":{"mode":"AUTO"}}' \
    'generationConfig:{"temperature":0.2}' \
    'generationConfig:{"maxOutputTokens":16}' \
    'generationConfig:{"maxOutputTokens":16,"responseMimeType":"application/json","responseSchema":{"type":"object","properties":{"a":{"type":"string"}}}}' \
    'generationConfig:{"thinkingConfig":{"thinkingBudget":128}}' \
    'generationConfig:{"responseModalities":["TEXT"]}' \
    'safetySettings:[{"category":"HARM_CATEGORY_HARASSMENT","threshold":"BLOCK_ONLY_HIGH"}]' \
    'labels:{"team":"x"}' \
    'cachedContent:"projects/p/locations/l/cachedContents/1"'
  do
    name="${field%%:*}"; value="${field#*:}"
    printf '{%s,"%s":%s}' "$CONTENTS" "$name" "$value" > "$work/f.json"
    printf '  %-20s %s\n' "$name" \
      "$(post "$(vertex_url no-such-model-probe)" "Authorization: Bearer $TOKEN" 'x-probe: 1' "$work/f.json")"
  done

  echo
  echo "### Vertex, do the carried fields CHANGE the count? (model: $VERTEX_MODEL)"
  echo "# If they do not, they are being ignored, and exact admission would"
  echo "# under-reserve every tool-heavy request."
  printf '{%s}' "$CONTENTS" > "$work/a.json"
  printf '{%s,"systemInstruction":{"parts":[{"text":"You are a careful assistant. Answer briefly and cite your reasoning when asked."}]}}' "$CONTENTS" > "$work/b.json"
  printf '{%s,"systemInstruction":{"parts":[{"text":"You are a careful assistant. Answer briefly and cite your reasoning when asked."}]},"tools":[{"functionDeclarations":[{"name":"get_weather","description":"Get the current weather for a city","parameters":{"type":"object","properties":{"city":{"type":"string","description":"City name"}},"required":["city"]}}]}]}' "$CONTENTS" > "$work/c.json"
  # generationConfig is CARRIED, against the obvious reading. It looks like it
  # describes only the output, and an earlier version dropped it for exactly
  # that reason. A responseSchema lives inside it and is counted as prompt.
  printf '{%s,"generationConfig":{"responseMimeType":"application/json","responseSchema":{"type":"object","properties":{"city":{"type":"string","description":"The city the weather was requested for, spelled in full"},"temperature_celsius":{"type":"number","description":"Current temperature in degrees celsius"},"conditions":{"type":"string","description":"A short human readable description of the current conditions"},"humidity_percent":{"type":"number","description":"Relative humidity as a percentage"}},"required":["city","temperature_celsius","conditions"]}}}' "$CONTENTS" > "$work/d.json"
  for f in a b c d; do
    printf '  %-20s %s\n' "$f" \
      "$(post "$(vertex_url "$VERTEX_MODEL")" "Authorization: Bearer $TOKEN" 'x-probe: 1' "$work/$f.json")"
  done
  echo "  a=bare  b=+systemInstruction  c=+tools  d=+generationConfig.responseSchema"
  echo "  Each must count HIGHER than the one before it. Measured on"
  echo "  gemini-2.5-flash: 1, 16, 32, 51. d being higher than a is why"
  echo "  generationConfig is carried: the schema is prompt material, and"
  echo "  generateContent bills it as prompt too. If d ever equals a on some"
  echo "  model, the schema is not counted there and carrying it is harmless."
  echo "  A field that does NOT move the count is not being counted, and"
  echo "  reserving without it under-reserves every request that uses it."

  echo
  echo "### Vertex, how does it refuse?"
  printf '{%s,"safetySettings":[]}' "$CONTENTS" > "$work/bad.json"
  printf '  %-20s %s\n' "unknown field" \
    "$(post "$(vertex_url "$VERTEX_MODEL")" "Authorization: Bearer $TOKEN" 'x-probe: 1' "$work/bad.json")"
  printf '{%s}' "$CONTENTS" > "$work/ok.json"
  printf '  %-20s %s\n' "bad credential" \
    "$(post "$(vertex_url "$VERTEX_MODEL")" "Authorization: Bearer not-a-real-token" 'x-probe: 1' "$work/ok.json")"
  echo
else
  echo "### Vertex: skipped (set PROJECT, and be logged in to gcloud)"; echo
fi

if [ -n "${ANTHROPIC_API_KEY:-}" ]; then
  URL="https://api.anthropic.com/v1/messages/count_tokens"
  echo "### Anthropic count_tokens, how does it refuse?"

  echo '{"model":"'"$ANTHROPIC_MODEL"'","messages":[]}' > "$work/bad.json"
  printf '  %-20s %s\n' "malformed body" \
    "$(post "$URL" "x-api-key: $ANTHROPIC_API_KEY" 'anthropic-version: 2023-06-01' "$work/bad.json")"

  echo '{"model":"'"$ANTHROPIC_MODEL"'","messages":[{"role":"user","content":"hi"}]}' > "$work/ok.json"
  printf '  %-20s %s\n' "bad credential" \
    "$(post "$URL" "x-api-key: sk-ant-not-a-real-key" 'anthropic-version: 2023-06-01' "$work/ok.json")"

  echo '{"model":"claude-nope-9","messages":[{"role":"user","content":"hi"}]}' > "$work/nf.json"
  printf '  %-20s %s\n' "unknown model" \
    "$(post "$URL" "x-api-key: $ANTHROPIC_API_KEY" 'anthropic-version: 2023-06-01' "$work/nf.json")"

  # The original bug: max_tokens is required on Messages and rejected here.
  echo '{"model":"'"$ANTHROPIC_MODEL"'","max_tokens":16,"messages":[{"role":"user","content":"hi"}]}' > "$work/mt.json"
  printf '  %-20s %s\n' "with max_tokens" \
    "$(post "$URL" "x-api-key: $ANTHROPIC_API_KEY" 'anthropic-version: 2023-06-01' "$work/mt.json")"
  echo
else
  echo "### Anthropic: skipped (set ANTHROPIC_API_KEY)"; echo
fi

cat <<'NOTE'
What must hold, or the code is wrong:

  Field acceptance   `vertex_count_payload` must carry exactly the fields shown
                     accepted here, and no others. A carried field that is
                     rejected fails EVERY request that includes it.
  Counts must move   A carried field whose presence does not change the count is
                     not being counted, and reserving without it under-reserves.
  Refusal statuses   `count_refusal` treats 400, 413 and 422 as the caller's
                     body and everything else as the provider. A credential
                     failure must appear here as 401 or 403, never as 400: if a
                     provider ever returns 400 for a bad key, that allowlist
                     would blame every caller for our own misconfiguration.
NOTE
