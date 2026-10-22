#!/bin/sh
set -eu
export PATH="$HOME/.cargo/bin:$PATH:/opt/homebrew/bin:/usr/local/bin"
[ "$(uname -s)" = Darwin ] || { echo 'This installer requires macOS.' >&2; exit 1; }
repo=$(cd "${1:-$(dirname "$0")/..}" && pwd)
state="$HOME/.kodrik"
logs="$state/logs"
binary="$HOME/.local/bin/kodrik"
plist="$HOME/Library/LaunchAgents/com.kodrik.gateway.plist"
domain="gui/$(id -u)"
service="$domain/com.kodrik.gateway"
npm ci --prefix "$repo/web"
npm run build --prefix "$repo/web"
cargo build --release --locked --manifest-path "$repo/Cargo.toml"
"$repo/target/release/kodrik" --version
mkdir -p "$logs" "$(dirname "$binary")" "$(dirname "$plist")"
fresh=false
if [ ! -e "$state/config.toml" ]; then
    (umask 077; set -C; printf '[gateway]\n\n[provider.primary]\ntype = "openai"\nmodel = "your-model"\napi_key = "your-key"\n' > "$state/config.toml")
    fresh=true
fi
staging=$(mktemp -d "$state/install.XXXXXX")
staged_binary=$(mktemp "$(dirname "$binary")/.kodrik-install.XXXXXX")
trap 'rm -f "$staged_binary"; rm -rf "$staging"' EXIT HUP INT TERM
staged_plist="$staging/gateway.plist"
plutil -create xml1 "$staged_plist"
plutil -insert Label -string com.kodrik.gateway "$staged_plist"
plutil -insert ProgramArguments -xml '<array/>' "$staged_plist"
plutil -insert ProgramArguments.0 -string "$binary" "$staged_plist"
plutil -insert ProgramArguments.1 -string gateway "$staged_plist"
plutil -insert WorkingDirectory -string "$repo" "$staged_plist"
plutil -insert KeepAlive -bool true "$staged_plist"
plutil -insert RunAtLoad -bool true "$staged_plist"
plutil -insert ThrottleInterval -integer 30 "$staged_plist"
plutil -insert EnvironmentVariables -xml '<dict/>' "$staged_plist"
plutil -insert EnvironmentVariables.PATH -string "$PATH" "$staged_plist"
plutil -insert StandardOutPath -string "$logs/kodrik-gateway.log" "$staged_plist"
plutil -insert StandardErrorPath -string "$logs/kodrik-gateway.err.log" "$staged_plist"
plutil -lint "$staged_plist"
cp -p "$repo/target/release/kodrik" "$staged_binary"
codesign --force --sign "Kodrik Local" --identifier com.kodrik.gateway --timestamp=none "$staged_binary"
codesign --verify --strict "$staged_binary"
if launchctl print "$service" >/dev/null 2>&1; then
    launchctl bootout "$service"
fi
if [ -f "$plist" ]; then cp -p "$plist" "$state/gateway.plist.backup"; fi
for name in kodrik-gateway.log kodrik-gateway.err.log self-update-check.log; do
    if [ -f "$repo/data/$name" ]; then
        destination="$logs/$name"
        if [ -e "$destination" ]; then destination="$destination.migrated-$$"; fi
        mv "$repo/data/$name" "$destination"
    fi
done
if [ -d "$repo/data" ]; then rmdir "$repo/data" 2>/dev/null || :; fi
mv -f "$staged_binary" "$binary"
mv -f "$staged_plist" "$plist"
if [ "$fresh" = true ]; then
    echo "Installed $binary. Fill in $state/config.toml, configure Telegram, then run this installer again."
else
    launchctl bootstrap "$domain" "$plist"
    echo "Gateway installed. Logs: $logs"
fi
