# Paseo onboarding

Use Paseo to work with your Buzz agents from your Mac and phone.
You need the `buzz` CLI and Paseo; you do not need Kubernetes access or the Buzz desktop app.
Start after your operator confirms the service rollout and CLI download are ready.

## 1. Install the CLI on an Apple Silicon Mac

Install [GitHub CLI](https://cli.github.com/) if needed, then sign in to your GitHub account:

```sh
brew install gh
gh auth login
```

Download the newest CLI release, verify its checksum, and install it:

```sh
BUZZ_CLI_TAG="$(gh release list --repo voltagecloud/buzz --limit 100 \
  --json tagName,publishedAt \
  --jq 'map(select(.tagName | startswith("buzz-cli-"))) | sort_by(.publishedAt) | last | .tagName')"
BUZZ_DOWNLOAD_DIR="$(mktemp -d)"
gh release download "$BUZZ_CLI_TAG" --repo voltagecloud/buzz \
  --dir "$BUZZ_DOWNLOAD_DIR" \
  --pattern buzz-cli-macos-arm64.tar.gz --pattern SHA256SUMS
(cd "$BUZZ_DOWNLOAD_DIR" && shasum -a 256 -c SHA256SUMS && tar -xzf buzz-cli-macos-arm64.tar.gz)
mkdir -p "$HOME/.local/bin"
install -m 755 "$BUZZ_DOWNLOAD_DIR/buzz" "$HOME/.local/bin/buzz"
export PATH="$HOME/.local/bin:$PATH"
buzz paseo setup --help
```

The checksum command should report `OK`; the last command should show setup help.
Add `export PATH="$HOME/.local/bin:$PATH"` to `~/.zshrc` to keep the CLI available in new terminals.

For another macOS or Linux computer, build the CLI from source:

```sh
git clone --depth 1 https://github.com/voltagecloud/buzz.git buzz-cli-source
cd buzz-cli-source
. ./bin/activate-hermit
cargo install --locked --path crates/buzz-cli
```

## 2. Use your existing Buzz identity

Use the private key for your existing Buzz account. Do not generate a replacement identity.
Ask your operator for the relay URL and manager public key. Replace both example values below.
In Terminal's default zsh shell, paste your private key at this hidden prompt:

```sh
unset BUZZ_AUTH_TAG
read -rs 'BUZZ_PRIVATE_KEY?Buzz private key: '
printf '\n'
export BUZZ_PRIVATE_KEY
export BUZZ_RELAY_URL="https://your-community.example"
export BUZZ_MANAGER_PUBKEY="your-manager-public-key"
buzz paseo setup
```

The key stays on your computer. Do not put it in this README, a message, or a screenshot.

## 3. Complete setup

Setup reuses your stored agent profile and asks only for missing credentials.
Your existing agent GitHub token does not need replacing just to add Paseo.
If setup needs a credential, it explains how to obtain it and hides what you type.

The Paseo code browser uses a separate, optional GitHub token with read-only access.
Select only the repositories your operator configured for your daemon.
Give it read-only Contents and Pull requests permissions.
Private organization repositories may require token approval.
Public repositories need no additional private-repository grant.
If you want the native Paseo assistant, supply the Claude subscription token when requested.

If setup says the gateway needs relay admission, give your operator the printed **public** gateway key and admin command.
Then rerun setup. Never send the pairing password, private key, or credential tokens.

## 4. Pair Paseo

Install Paseo on your Mac or phone and choose **Add host → Scan QR**, or paste the printed link.
The QR code and link include the password; you should not need to type it separately.
Treat them as credentials. Anyone holding them can connect to your daemon.

To display them again:

```sh
buzz paseo pairing
```

## 5. Check your first agent

Open a remote-agent provider in Paseo and send a small task.
Confirm you see its reply and context usage.
If a conversation is missing, use **Import session → Show all**.
An agent's pushed file links should open its branch after the sandbox stops.

Existing agents should appear after setup refreshes the provider list.
If you need a new team, ask your operator for a team snapshot, then run:

```sh
buzz paseo import-team /path/to/team.json
```

## Later

Run `buzz paseo setup` at least every 90 days to renew access.
Use `buzz paseo setup --update` to change credentials; Enter keeps each stored value.
Setup shares your stream and forum channels with the daemon, including private ones.
Use `--exclude <channel>` on every setup run for a channel you want to keep out.
Direct messages are not shared.

When finished with setup in this terminal, run `unset BUZZ_PRIVATE_KEY`.
You can keep using the paired Paseo app without leaving the owner key in the shell.
