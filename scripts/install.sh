#!/bin/bash
# Public release entry point: compatible with macOS's built-in Bash 3.2.
set -euo pipefail
gopher_install_tmp=''

fail() { printf 'Gopher installer: %s\n' "$*" >&2; exit 1; }

main() {
    local no_launch=0
    case "${1:-}" in
        --no-launch) no_launch=1; shift ;;
        --help|-h) printf 'Usage: install.sh [--no-launch]\nInstall or update Gopher in ~/Applications. Requires macOS 13+, Apple Silicon, and authenticated gh.\n'; return ;;
        '') ;;
        *) fail "Unknown argument: $1" ;;
    esac
    [ "$#" -eq 0 ] || fail 'Too many arguments'
    [ "$EUID" -ne 0 ] || fail 'Run as your normal user, without sudo'
    [ "$(/usr/bin/uname -s)" = Darwin ] || fail 'macOS is required'
    # hw.optional.arm64 also works when the invoking Terminal runs under Rosetta.
    [ "$(/usr/sbin/sysctl -n hw.optional.arm64 2>/dev/null || true)" = 1 ] || fail 'Apple Silicon is required'
    local os_version
    os_version=$(/usr/bin/sw_vers -productVersion)
    [ "${os_version%%.*}" -ge 13 ] || fail 'macOS 13 or later is required'
    local gh
    gh=$(command -v gh || true)
    if [ -z "$gh" ]; then
        for gh in /opt/homebrew/bin/gh /usr/local/bin/gh; do
            [ ! -x "$gh" ] || break
        done
    fi
    [ -x "$gh" ] || fail 'Install GitHub CLI (https://cli.github.com), run gh auth login, then rerun this installer'
    unset GH_DEBUG
    export GH_PROMPT_DISABLED=1 GH_PAGER=cat
    "$gh" auth status --hostname github.com >/dev/null 2>&1 || fail 'Authenticate GitHub CLI with gh auth login --hostname github.com, then rerun this installer'
    local tag version
    tag=$("$gh" api --hostname github.com repos/mbuvarp/gopher/releases/latest --jq .tag_name) || fail 'Cannot find the latest release. Check connectivity and API limits; a release may not have been published yet'
    [[ "$tag" =~ ^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]] || fail 'The latest release has an unsupported version tag'
    version=${tag#v}
    local archive base expected actual member
    gopher_install_tmp=$(/usr/bin/mktemp -d)
    # The function runs in a subshell, so this cleanup also covers early failures.
    trap '/bin/rm -rf -- "$gopher_install_tmp"' EXIT
    archive="Gopher-$version-macos-arm64.zip"
    base="https://github.com/mbuvarp/gopher/releases/download/$tag"
    printf 'Downloading Gopher %s…\n' "$version"
    /usr/bin/curl --proto '=https' --proto-redir '=https' --fail --location --silent --show-error --connect-timeout 10 --max-time 120 --retry 2 "$base/$archive" --output "$gopher_install_tmp/$archive"
    /usr/bin/curl --proto '=https' --proto-redir '=https' --fail --location --silent --show-error --connect-timeout 10 --max-time 30 --retry 2 "$base/$archive.sha256" --output "$gopher_install_tmp/checksum"
    # Validate the complete checksum record rather than interpreting downloaded paths.
    expected=$(/bin/cat "$gopher_install_tmp/checksum")
    [[ "$expected" =~ ^[a-f0-9]{64}\ \ Gopher-[0-9]+\.[0-9]+\.[0-9]+-macos-arm64\.zip$ ]] || fail 'Invalid checksum file'
    [ "${expected#*  }" = "$archive" ] || fail 'Checksum names a different archive'
    actual=$(/usr/bin/shasum -a 256 "$gopher_install_tmp/$archive")
    [ "${actual%% *}" = "${expected%% *}" ] || fail 'Archive checksum mismatch; installation unchanged'
    /usr/bin/tar -tf "$gopher_install_tmp/$archive" > "$gopher_install_tmp/members"
    while IFS= read -r member; do
        case "$member" in
            /*|..|../*|*/../*|*/..|*\\*) fail 'Unsafe archive path' ;;
            Gopher.app|Gopher.app/*|__MACOSX|__MACOSX/|__MACOSX/Gopher.app|__MACOSX/Gopher.app/*|__MACOSX/._Gopher.app) ;;
            *) fail 'Unexpected archive contents' ;;
        esac
    done < "$gopher_install_tmp/members"
    /bin/mkdir "$gopher_install_tmp/extracted"
    # bsdtar's default extraction protections reject traversal through symlinks.
    # Do not use -P, which disables these protections.
    /usr/bin/tar -xf "$gopher_install_tmp/$archive" -C "$gopher_install_tmp/extracted" --no-same-owner
    local app="$gopher_install_tmp/extracted/Gopher.app"
    [ -d "$app" ] && [ ! -L "$app" ] || fail 'Archive does not contain Gopher.app'
    /usr/bin/codesign --verify --deep --strict -R '=anchor apple generic and identifier "dev.mbuvarp.gopher" and certificate leaf[subject.OU] = "DZ4XZQXHZ7"' "$app" || fail 'Gopher signature verification failed'
    [ "$(/usr/bin/plutil -extract CFBundleShortVersionString raw -o - "$app/Contents/Info.plist")" = "$version" ] || fail 'App version does not match the release'
    [ "$(/usr/bin/plutil -extract GopherInstallerProtocol raw -o - "$app/Contents/Info.plist")" = 1 ] || fail 'Release does not support this installer'
    case "$(/usr/bin/file -b "$app/Contents/MacOS/gopher")" in
        'Mach-O 64-bit executable arm64') ;;
        *) fail 'Release is not an Apple Silicon executable' ;;
    esac
    if [ "$no_launch" = 1 ]; then
        "$app/Contents/MacOS/gopher" install --no-launch
    else
        "$app/Contents/MacOS/gopher" install
    fi
    printf 'This build is unnotarized. If macOS blocks launch, use System Settings → Privacy & Security → Open Anyway.\n'
}

# Define everything before running, including when invoked through curl | bash.
( main "$@" )
