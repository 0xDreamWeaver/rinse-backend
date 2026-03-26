#!/usr/bin/env bash
#
# Discogs OAuth 1.0a Token Generator
#
# Walks through the 3-legged OAuth flow to obtain an access token
# for the Discogs API. Uses PLAINTEXT signature method over HTTPS.
#
# Prerequisites:
#   1. Create an app at https://www.discogs.com/settings/developers
#   2. Note your Consumer Key and Consumer Secret
#
# Usage:
#   ./scripts/discogs_token.sh
#   DISCOGS_CONSUMER_KEY=xxx DISCOGS_CONSUMER_SECRET=yyy ./scripts/discogs_token.sh

set -euo pipefail

USER_AGENT="Rinse/1.0.0"

REQUEST_TOKEN_URL="https://api.discogs.com/oauth/request_token"
AUTHORIZE_URL="https://www.discogs.com/oauth/authorize"
ACCESS_TOKEN_URL="https://api.discogs.com/oauth/access_token"

# --- Get consumer credentials ---

CONSUMER_KEY="${DISCOGS_CONSUMER_KEY:-}"
CONSUMER_SECRET="${DISCOGS_CONSUMER_SECRET:-}"

if [ -z "$CONSUMER_KEY" ]; then
    read -rp "Consumer Key: " CONSUMER_KEY
fi
if [ -z "$CONSUMER_SECRET" ]; then
    read -rp "Consumer Secret: " CONSUMER_SECRET
fi

if [ -z "$CONSUMER_KEY" ] || [ -z "$CONSUMER_SECRET" ]; then
    echo "Error: Consumer Key and Consumer Secret are required."
    echo "Create an app at https://www.discogs.com/settings/developers"
    exit 1
fi

# --- Step 1: Request Token ---

echo ""
echo "Step 1/3: Requesting token..."

TIMESTAMP=$(date +%s)
NONCE="${TIMESTAMP}$(openssl rand -hex 4)"

RESPONSE=$(curl -s -X POST "$REQUEST_TOKEN_URL" \
    -H "Content-Type: application/x-www-form-urlencoded" \
    -H "User-Agent: $USER_AGENT" \
    -H "Authorization: OAuth \
oauth_consumer_key=\"${CONSUMER_KEY}\", \
oauth_nonce=\"${NONCE}\", \
oauth_signature=\"${CONSUMER_SECRET}&\", \
oauth_signature_method=\"PLAINTEXT\", \
oauth_timestamp=\"${TIMESTAMP}\", \
oauth_callback=\"oob\"")

# Parse response (format: oauth_token=xxx&oauth_token_secret=yyy&oauth_callback_confirmed=true)
if echo "$RESPONSE" | grep -q "oauth_token="; then
    REQUEST_TOKEN=$(echo "$RESPONSE" | tr '&' '\n' | grep "oauth_token=" | head -1 | cut -d= -f2)
    REQUEST_TOKEN_SECRET=$(echo "$RESPONSE" | tr '&' '\n' | grep "oauth_token_secret=" | cut -d= -f2)
    echo "  Got request token."
else
    echo "Error: Failed to get request token."
    echo "Response: $RESPONSE"
    exit 1
fi

# --- Step 2: User Authorization ---

AUTH_URL="${AUTHORIZE_URL}?oauth_token=${REQUEST_TOKEN}"

echo ""
echo "Step 2/3: Authorize the app."
echo ""
echo "  Open this URL in your browser:"
echo ""
echo "  $AUTH_URL"
echo ""

# Try to open the URL automatically
if command -v open &>/dev/null; then
    open "$AUTH_URL"
    echo "  (Opened in your default browser)"
    echo ""
fi

read -rp "Enter the verification code from Discogs: " VERIFIER

if [ -z "$VERIFIER" ]; then
    echo "Error: Verification code is required."
    exit 1
fi

# --- Step 3: Access Token ---

echo ""
echo "Step 3/3: Exchanging for access token..."

TIMESTAMP=$(date +%s)
NONCE="${TIMESTAMP}$(openssl rand -hex 4)"

RESPONSE=$(curl -s -X POST "$ACCESS_TOKEN_URL" \
    -H "Content-Type: application/x-www-form-urlencoded" \
    -H "User-Agent: $USER_AGENT" \
    -H "Authorization: OAuth \
oauth_consumer_key=\"${CONSUMER_KEY}\", \
oauth_nonce=\"${NONCE}\", \
oauth_token=\"${REQUEST_TOKEN}\", \
oauth_signature=\"${CONSUMER_SECRET}&${REQUEST_TOKEN_SECRET}\", \
oauth_signature_method=\"PLAINTEXT\", \
oauth_timestamp=\"${TIMESTAMP}\", \
oauth_verifier=\"${VERIFIER}\"")

if echo "$RESPONSE" | grep -q "oauth_token="; then
    ACCESS_TOKEN=$(echo "$RESPONSE" | tr '&' '\n' | grep "oauth_token=" | head -1 | cut -d= -f2)
    ACCESS_TOKEN_SECRET=$(echo "$RESPONSE" | tr '&' '\n' | grep "oauth_token_secret=" | cut -d= -f2)

    echo ""
    echo "Success! Add these to your .env file:"
    echo ""
    echo "  DISCOGS_CONSUMER_KEY=${CONSUMER_KEY}"
    echo "  DISCOGS_CONSUMER_SECRET=${CONSUMER_SECRET}"
    echo "  DISCOGS_TOKEN=${ACCESS_TOKEN}"
    echo "  DISCOGS_TOKEN_SECRET=${ACCESS_TOKEN_SECRET}"
    echo ""
else
    echo "Error: Failed to get access token."
    echo "Response: $RESPONSE"
    exit 1
fi
