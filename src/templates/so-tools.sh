#!/bin/bash
# so tools installer

set -e

export DEBIAN_FRONTEND=noninteractive

# use sudo if not root
SUDO=""
[ "$(id -u)" -ne 0 ] && SUDO="sudo"

echo "installing so tools"

CLAUDE_CODE_VERSION="2.1.22"
OPENCODE_VERSION="1.1.39"
CODEX_VERSION="0.92.0"

# install common tools
$SUDO apt-get update -qq
$SUDO apt-get install -y --no-install-recommends ripgrep jq git

# install node for opencode, codex, and claude code (as root)
echo "installing node..."
curl -fsSL https://deb.nodesource.com/setup_lts.x -o /tmp/node-setup.sh
$SUDO bash /tmp/node-setup.sh
rm -f /tmp/node-setup.sh
$SUDO apt-get install -y nodejs
$SUDO rm -rf /var/lib/apt/lists/*

echo "installing claude code..."
$SUDO npm i -g @anthropic-ai/claude-code@"$CLAUDE_CODE_VERSION"

echo "installing opencode..."
$SUDO npm i -g opencode-ai@"$OPENCODE_VERSION"

echo "installing codex..."
$SUDO npm i -g @openai/codex@"$CODEX_VERSION"

echo "installing jscpd..."
$SUDO npm i -g jscpd

# install so
echo "installing so..."
$SUDO curl -fsSL https://raw.githubusercontent.com/arvinmi/dotfiles/main/common/so/so.sh -o /usr/local/bin/so
$SUDO chmod +x /usr/local/bin/so

echo "done"
