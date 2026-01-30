#!/bin/bash
set -e
export DEBIAN_FRONTEND=noninteractive

# use sudo if not root
run() { [ "$(id -u)" -eq 0 ] && "$@" || sudo "$@"; }

echo "installing so tools"

# install system packages
run apt-get update -qq
run apt-get install -y --no-install-recommends ripgrep jq git

# install node for opencode, codex, and claude code
curl -fsSL https://deb.nodesource.com/setup_lts.x -o /tmp/node-setup.sh
run bash /tmp/node-setup.sh
rm -f /tmp/node-setup.sh
run apt-get install -y nodejs
run rm -rf /var/lib/apt/lists/*

# install npm packages
run npm i -g \
  @anthropic-ai/claude-code@2.1.22 \
  opencode-ai@1.1.39 \
  @openai/codex@0.92.0 \
  jscpd

echo "done"
