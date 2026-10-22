#!/bin/sh
set -eu
script=$(cd "$(dirname "$0")" && pwd)/install-macos.sh
test_dir=$(mktemp -d)
trap 'rm -rf "$test_dir"' EXIT HUP INT TERM
mkdir -p "$test_dir/home/.cargo/bin" "$test_dir/repo & checkout/target/release" "$test_dir/repo & checkout/data"
printf '#!/bin/sh\nexit 0\n' > "$test_dir/home/.cargo/bin/cargo"
printf '#!/bin/sh\nprintf "%%s\\n" "$*" >> "$TEST_NPM_CALLS"\n' > "$test_dir/home/.cargo/bin/npm"
printf '#!/bin/sh\necho "Python must not be required" >&2\nexit 1\n' > "$test_dir/home/.cargo/bin/python3"
printf '#!/bin/sh\nexit 0\n' > "$test_dir/repo & checkout/target/release/kodrik"
printf '#!/bin/sh\nprintf "%%s\\n" "$*" >> "$TEST_CALLS"\n[ "$1" != print ]\n' > "$test_dir/home/.cargo/bin/launchctl"
printf '#!/bin/sh\nprintf "%%s\\n" "$*" >> "$TEST_CODESIGN_CALLS"\n[ "$1" != "${TEST_CODESIGN_FAIL:-}" ]\n' > "$test_dir/home/.cargo/bin/codesign"
chmod +x "$test_dir/home/.cargo/bin/"* "$test_dir/repo & checkout/target/release/kodrik"
printf 'old log\n' > "$test_dir/repo & checkout/data/kodrik-gateway.log"
export TEST_CALLS="$test_dir/calls"
export TEST_CODESIGN_CALLS="$test_dir/codesign-calls"
export TEST_NPM_CALLS="$test_dir/npm-calls"
env HOME="$test_dir/home" /bin/sh "$script" "$test_dir/repo & checkout"
grep -q '^ci --prefix .*repo & checkout/web$' "$TEST_NPM_CALLS"
grep -q '^run build --prefix .*repo & checkout/web$' "$TEST_NPM_CALLS"
grep -q '^--force --sign Kodrik Local --identifier com.kodrik.gateway --timestamp=none ' "$TEST_CODESIGN_CALLS"
grep -q '^--verify --strict ' "$TEST_CODESIGN_CALLS"
test -x "$test_dir/home/.local/bin/kodrik"
test -f "$test_dir/home/.kodrik/config.toml"
test "$(stat -f %Lp "$test_dir/home/.kodrik/config.toml")" = 600
test ! -d "$test_dir/repo & checkout/data"
test "$(cat "$test_dir/home/.kodrik/logs/kodrik-gateway.log")" = 'old log'
plist="$test_dir/home/Library/LaunchAgents/com.kodrik.gateway.plist"
test "$(plutil -extract WorkingDirectory raw -o - "$plist")" = "$test_dir/repo & checkout"
test "$(plutil -extract ProgramArguments.0 raw -o - "$plist")" = "$test_dir/home/.local/bin/kodrik"
test "$(plutil -extract KeepAlive raw -o - "$plist")" = true
if grep -q bootstrap "$TEST_CALLS"; then exit 1; fi
printf 'keep config\n' > "$test_dir/home/.kodrik/config.toml"
env HOME="$test_dir/home" /bin/sh "$script" "$test_dir/repo & checkout"
test "$(cat "$test_dir/home/.kodrik/config.toml")" = 'keep config'
test -f "$test_dir/home/.kodrik/gateway.plist.backup"
grep -q '^bootstrap gui/' "$TEST_CALLS"
cp "$test_dir/home/.local/bin/kodrik" "$test_dir/previous-binary"
cp "$plist" "$test_dir/previous-plist"
for failure in --force --verify; do
    : > "$TEST_CALLS"
    if env HOME="$test_dir/home" TEST_CODESIGN_FAIL="$failure" /bin/sh "$script" "$test_dir/repo & checkout"; then
        echo "Installer must stop on codesign $failure failure" >&2
        exit 1
    fi
    test ! -s "$TEST_CALLS"
    cmp "$test_dir/previous-binary" "$test_dir/home/.local/bin/kodrik"
    cmp "$test_dir/previous-plist" "$plist"
done
echo 'Installer checks passed.'
