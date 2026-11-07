#!/bin/sh
set -e
# A previous container may have left Chrome's lock files in the profile; Chrome would
# then refuse to start ("profile in use on another computer").
rm -f /data/chrome-profile/SingletonLock /data/chrome-profile/SingletonSocket /data/chrome-profile/SingletonCookie

# -mcp-remote: requests from the host arrive through Docker's network, not from localhost.
# The Makefile publishes the port on 127.0.0.1 only, so the MCP endpoint stays local to this machine.
exec agent-browser-use \
  -chrome /usr/local/bin/chromium-docker \
  -profile /data/chrome-profile \
  -open=false \
  -mcp-remote \
  "$@"
