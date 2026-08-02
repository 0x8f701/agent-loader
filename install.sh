#!/bin/sh
#
# al installer (macOS / Linux).
#
# Downloads the matching platform artifact from this repo's GitHub Releases,
# verifies its SHA-256 against the release's SHA256SUMS manifest, and installs
# the binary as ~/.agent-loader/bin/al (versioned binary in
# ~/.agent-loader/downloads/, atomic symlink in bin/).
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/0x8f701/agent-loader/main/install.sh | sh
#   sh install.sh --version v0.2.1      # pin a specific release
#
# Environment:
#   AL_HOME                install root (default: ~/.agent-loader)
#   AL_UPDATE_BASE_URL     GitHub-Releases-shaped API base (default:
#                          https://api.github.com/repos/0x8f701/agent-loader/releases)
#
# Fails fast on any error; never leaves a partial binary as the active al.

set -eu

REPO="0x8f701/agent-loader"
API_BASE="${AL_UPDATE_BASE_URL:-https://api.github.com/repos/${REPO}/releases}"
AL_HOME="${AL_HOME:-$HOME/.agent-loader}"

err() {
    printf 'install.sh: error: %s\n' "$*" >&2
    exit 1
}

usage() {
    sed -n '2,20p' "$0" 2>/dev/null | sed 's/^# \{0,1\}//'
}

is_semver() {
    printf '%s\n' "$1" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$'
}

# ── Arguments ────────────────────────────────────────────────────────────────
VERSION=""
while [ $# -gt 0 ]; do
    case "$1" in
        --version)
            [ $# -ge 2 ] || err "--version requires a value"
            VERSION="$2"
            shift 2
            ;;
        --version=*)
            VERSION="${1#*=}"
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            err "unknown argument: $1"
            ;;
    esac
done
VERSION="${VERSION#v}"
if [ -n "$VERSION" ] && ! is_semver "$VERSION"; then
    err "invalid version '$VERSION' (expected X.Y.Z or vX.Y.Z)"
fi

# ── Platform detection ───────────────────────────────────────────────────────
OS="$(uname -s)"
ARCH="$(uname -m)"
PLATFORM_OS=""
PLATFORM_ARCH=""
TRIPLE=""
case "$OS" in
    Darwin)
        PLATFORM_OS="macos"
        case "$ARCH" in
            arm64|aarch64)
                PLATFORM_ARCH="aarch64"
                TRIPLE="aarch64-apple-darwin"
                ;;
            x86_64)
                PLATFORM_ARCH="x86_64"
                TRIPLE="x86_64-apple-darwin"
                ;;
            *)
                err "unsupported macOS architecture: $ARCH"
                ;;
        esac
        ;;
    Linux)
        PLATFORM_OS="linux"
        case "$ARCH" in
            aarch64|arm64)
                PLATFORM_ARCH="aarch64"
                TRIPLE="aarch64-unknown-linux-gnu"
                ;;
            x86_64|amd64)
                PLATFORM_ARCH="x86_64"
                TRIPLE="x86_64-unknown-linux-gnu"
                ;;
            *)
                err "unsupported Linux architecture: $ARCH"
                ;;
        esac
        ;;
    *)
        err "unsupported OS: $OS (Windows: use install.ps1)"
        ;;
esac

# ── Downloader ───────────────────────────────────────────────────────────────
# Optional: set GITHUB_TOKEN to authenticate the fixed GitHub API endpoint and
# avoid the unauthenticated rate limit (60 req/hr per IP). Never forward the
# token to release-asset hosts or a custom test endpoint.
AUTH_HDR=""
if [ -n "${GITHUB_TOKEN:-}" ]; then
    AUTH_HDR="Authorization: Bearer $GITHUB_TOKEN"
fi

is_fixed_github_api_url() {
    case "$1" in
        "https://api.github.com/repos/${REPO}/releases"*)
            return 0
            ;;
        *)
            return 1
            ;;
    esac
}

if command -v curl >/dev/null 2>&1; then
    fetch() {
        if [ -n "$AUTH_HDR" ] && is_fixed_github_api_url "$1"; then
            curl -fsSL -H "$AUTH_HDR" -o "$2" "$1" || return 1
        else
            curl -fsSL -o "$2" "$1" || return 1
        fi
    }
    fetch_stdout() {
        if [ -n "$AUTH_HDR" ] && is_fixed_github_api_url "$1"; then
            curl -fsSL -H "$AUTH_HDR" "$1" || return 1
        else
            curl -fsSL "$1" || return 1
        fi
    }
elif command -v wget >/dev/null 2>&1; then
    fetch() {
        if [ -n "$AUTH_HDR" ] && is_fixed_github_api_url "$1"; then
            wget -q --header="$AUTH_HDR" -O "$2" "$1" || return 1
        else
            wget -q -O "$2" "$1" || return 1
        fi
    }
    fetch_stdout() {
        if [ -n "$AUTH_HDR" ] && is_fixed_github_api_url "$1"; then
            wget -q --header="$AUTH_HDR" -O - "$1" || return 1
        else
            wget -q -O - "$1" || return 1
        fi
    }
else
    err "neither curl nor wget found"
fi

# ── SHA-256 tool ─────────────────────────────────────────────────────────────
if command -v sha256sum >/dev/null 2>&1; then
    sha256_of() { sha256sum -b "$1" | awk '{print $1}'; }
elif command -v shasum >/dev/null 2>&1; then
    sha256_of() { shasum -a 256 -b "$1" | awk '{print $1}'; }
else
    err "neither sha256sum nor shasum found"
fi

TMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/al-install.XXXXXX")"
STAGED=""
TMP_LINK=""
ROLLBACK_LINK=""
STATE_TMP=""
TRANSACTION_ACTIVE=0
ROLLBACK_RUNNING=0
cleanup() {
    trap - EXIT HUP INT TERM
    if [ "${TRANSACTION_ACTIVE:-0}" -eq 1 ] && [ "${ROLLBACK_RUNNING:-0}" -eq 0 ]; then
        ROLLBACK_RUNNING=1
        if rollback_install; then
            TRANSACTION_ACTIVE=0
        else
            printf 'install.sh: error: interrupted install rollback failed\n' >&2
        fi
    fi
    if [ -n "${STATE_TMP:-}" ] && [ -f "$STATE_TMP" ]; then rm -f "$STATE_TMP"; fi
    if [ -n "${ROLLBACK_LINK:-}" ] && { [ -e "$ROLLBACK_LINK" ] || [ -L "$ROLLBACK_LINK" ]; }; then rm -f "$ROLLBACK_LINK"; fi
    if [ -n "${TMP_LINK:-}" ] && { [ -e "$TMP_LINK" ] || [ -L "$TMP_LINK" ]; }; then rm -f "$TMP_LINK"; fi
    if [ -n "${STAGED:-}" ] && [ -f "$STAGED" ]; then rm -f "$STAGED"; fi
    if [ -d "$TMP_DIR" ]; then rm -rf "$TMP_DIR"; fi
}
trap cleanup EXIT
trap 'cleanup; exit 1' HUP INT TERM

# ── Resolve the release ──────────────────────────────────────────────────────
if [ -n "$VERSION" ]; then
    RELEASE_URL="$API_BASE/tags/v$VERSION"
else
    RELEASE_URL="$API_BASE/latest"
fi
printf 'Resolving release from %s\n' "$RELEASE_URL"
RELEASE_JSON="$(fetch_stdout "$RELEASE_URL")" \
    || err "could not fetch release metadata from $RELEASE_URL
         (GitHub may be rate-limiting this IP; set GITHUB_TOKEN to authenticate)"

TAG="$(printf '%s' "$RELEASE_JSON" \
    | sed 's/"tag_name"/\
"tag_name"/g' \
    | sed -n 's/^[[:space:]]*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
    | head -n 1)"
[ -n "$TAG" ] || err "release metadata has no tag_name (endpoint: $RELEASE_URL)"
case "$TAG" in
    v*)
        RESOLVED_VERSION="${TAG#v}"
        ;;
    *)
        err "release tag '$TAG' is invalid (expected vX.Y.Z)"
        ;;
esac
is_semver "$RESOLVED_VERSION" \
    || err "release tag '$TAG' is invalid (expected semantic version vX.Y.Z)"
if [ -n "$VERSION" ] && [ "$RESOLVED_VERSION" != "$VERSION" ]; then
    err "requested version $VERSION but release tag is $TAG"
fi

URLS="$(printf '%s' "$RELEASE_JSON" \
    | sed 's/"browser_download_url"/\
"browser_download_url"/g' \
    | sed -n 's/^[[:space:]]*"browser_download_url"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p')"

find_asset_url() {
    suffix="$1"
    found=""
    count=0
    for u in $URLS; do
        case "$u" in
            */"$suffix")
                if [ "$count" -ne 0 ]; then
                    err "release $TAG contains duplicate $suffix assets"
                fi
                found="$u"
                count=1
                ;;
        esac
    done
    if [ "$count" -ne 1 ]; then
        return 1
    fi
    printf '%s\n' "$found"
}

if ! SUMS_URL="$(find_asset_url "SHA256SUMS")"; then
    err "release $TAG must contain exactly one SHA256SUMS asset"
fi

ASSET="al-${RESOLVED_VERSION}-${TRIPLE}.tar.gz"
if ! ARCHIVE_URL="$(find_asset_url "$ASSET")"; then
    err "release $TAG does not contain asset $ASSET"
fi

# ── Download + verify ────────────────────────────────────────────────────────
printf 'Downloading al v%s (%s)...\n' "$RESOLVED_VERSION" "$TRIPLE"
fetch "$ARCHIVE_URL" "$TMP_DIR/$ASSET" || err "download failed: $ARCHIVE_URL"
fetch "$SUMS_URL" "$TMP_DIR/SHA256SUMS" || err "download failed: $SUMS_URL"

MANIFEST_SIZE="$(wc -c < "$TMP_DIR/SHA256SUMS" | tr -d '[:space:]')"
ARCHIVE_SIZE="$(wc -c < "$TMP_DIR/$ASSET" | tr -d '[:space:]')"
[ "$MANIFEST_SIZE" -le 1048576 ] || err "SHA256SUMS is unexpectedly large"
[ "$ARCHIVE_SIZE" -le 1073741824 ] || err "$ASSET exceeds the 1 GiB safety limit"

EXPECTED=""
EXPECTED_COUNT=0
while IFS=' ' read -r hash name; do
    [ -n "$hash" ] || continue
    case "$name" in
        "$ASSET"|"*$ASSET")
            EXPECTED="$hash"
            EXPECTED_COUNT=$((EXPECTED_COUNT + 1))
            ;;
    esac
done < "$TMP_DIR/SHA256SUMS"
[ "$EXPECTED_COUNT" -eq 1 ] \
    || err "SHA256SUMS must contain exactly one entry for $ASSET"
case "$EXPECTED" in
    *[!0-9A-Fa-f]*|'')
        err "SHA256SUMS contains an invalid digest for $ASSET"
        ;;
esac
[ "${#EXPECTED}" -eq 64 ] || err "SHA256SUMS contains an invalid digest for $ASSET"
EXPECTED="$(printf '%s' "$EXPECTED" | tr 'A-F' 'a-f')"
ACTUAL="$(sha256_of "$TMP_DIR/$ASSET" | tr 'A-F' 'a-f')"
if [ "$ACTUAL" != "$EXPECTED" ]; then
    err "SHA256 mismatch for $ASSET: expected $EXPECTED, got $ACTUAL"
fi
printf 'Checksum verified.\n'

# ── Extract + install ────────────────────────────────────────────────────────
tar -tzf "$TMP_DIR/$ASSET" > "$TMP_DIR/archive.list" \
    || err "failed to inspect $ASSET"

BINARY_MEMBER=""
BINARY_COUNT=0
while IFS= read -r member; do
    normalized="${member#./}"
    [ "$normalized" = "al" ] || continue
    case "$member" in
        */)
            err "archive $ASSET contains a directory binary entry: $member"
            ;;
    esac
    if [ "$BINARY_COUNT" -ne 0 ]; then
        err "archive $ASSET contains more than one root-level al binary"
    fi
    BINARY_MEMBER="$member"
    BINARY_COUNT=1
done < "$TMP_DIR/archive.list"
[ "$BINARY_COUNT" -eq 1 ] \
    || err "archive $ASSET must contain exactly one root-level al binary"

tar -xOzf "$TMP_DIR/$ASSET" "$BINARY_MEMBER" > "$TMP_DIR/al" \
    || err "failed to extract al from $ASSET"
[ -s "$TMP_DIR/al" ] || err "archive $ASSET contains an empty al binary"
BINARY_SIZE="$(wc -c < "$TMP_DIR/al" | tr -d '[:space:]')"
[ "$BINARY_SIZE" -le 1073741824 ] || err "extracted al exceeds the 1 GiB safety limit"
chmod 0755 "$TMP_DIR/al"

ensure_directory() {
    path="$1"
    label="$2"
    [ ! -L "$path" ] || err "refusing to use symlinked $label: $path"
    if [ -e "$path" ]; then
        [ -d "$path" ] || err "$label is not a directory: $path"
    else
        mkdir -p "$path" || err "could not create $label: $path"
    fi
}

DOWNLOADS_DIR="$AL_HOME/downloads"
BIN_DIR="$AL_HOME/bin"
ensure_directory "$AL_HOME" "al install root"
ensure_directory "$DOWNLOADS_DIR" "al downloads directory"
ensure_directory "$BIN_DIR" "al bin directory"

# The archive digest is part of the deployment identity. A deliberately
# republished tag therefore gets a new path and cannot overwrite the active
# same-semver binary before its smoke test succeeds.
VERSIONED="al-${RESOLVED_VERSION}-${PLATFORM_OS}-${PLATFORM_ARCH}-sha256-${EXPECTED}"
DEST="$DOWNLOADS_DIR/$VERSIONED"
STAGED="$(mktemp "$DOWNLOADS_DIR/.al-stage.XXXXXX")" \
    || err "could not create a staged binary under $DOWNLOADS_DIR"
cp "$TMP_DIR/al" "$STAGED" || err "could not stage downloaded al"
chmod 0755 "$STAGED"
# Smoke-test the staged bytes before touching either live component.
"$STAGED" --version >/dev/null 2>&1 \
    || err "downloaded binary failed smoke test; existing install left untouched"

TMP_LINK="$BIN_DIR/al.install.$$"
[ ! -e "$TMP_LINK" ] && [ ! -L "$TMP_LINK" ] \
    || { rm -f "$STAGED"; err "temporary activation path already exists: $TMP_LINK"; }
ln -s "../downloads/$VERSIONED" "$TMP_LINK" \
    || { rm -f "$STAGED"; err "failed to stage active al link"; }

if [ ! -L "$BIN_DIR/al" ] && [ -e "$BIN_DIR/al" ]; then
    rm -f "$TMP_LINK" "$STAGED"
    err "$BIN_DIR/al is not a managed symlink; refusing to overwrite it"
fi

# Capture the prior active symlink target so rollback restores exactly what
# was live before this install, not the newly staged version.
HAD_ACTIVE=0
OLD_LINK_TARGET=""
if [ -L "$BIN_DIR/al" ]; then
    OLD_LINK_TARGET="$(readlink "$BIN_DIR/al")" \
        || { rm -f "$TMP_LINK" "$STAGED"; err "failed to read prior active al symlink"; }
    HAD_ACTIVE=1
fi

# Preserve an existing same-identity binary until both activation and the
# update-state commit succeed. Rollback moves these exact bytes back into
# place before restoring a link that may point at them.
HAD_DEST=0
VERSIONED_ASIDE="$DOWNLOADS_DIR/$VERSIONED.old.$$"

# Restore the prior managed symlink with an atomic rename, then verify the
# live link contains the exact target captured before activation.
rollback_active_link() {
    if [ "$HAD_ACTIVE" -eq 1 ]; then
        ROLLBACK_LINK="$BIN_DIR/al.rollback.$$"
        if [ -e "$ROLLBACK_LINK" ] || [ -L "$ROLLBACK_LINK" ]; then
            printf 'install.sh: error: rollback path already exists: %s\n' "$ROLLBACK_LINK" >&2
            return 1
        fi
        if ! ln -s "$OLD_LINK_TARGET" "$ROLLBACK_LINK"; then
            printf 'install.sh: error: rollback failed to stage the prior active al symlink\n' >&2
            return 1
        fi
        if ! mv -f "$ROLLBACK_LINK" "$BIN_DIR/al"; then
            printf 'install.sh: error: rollback failed to restore the prior active al symlink\n' >&2
            if ! rm -f "$ROLLBACK_LINK"; then
                printf 'install.sh: error: rollback also failed to remove temporary symlink: %s\n' "$ROLLBACK_LINK" >&2
            fi
            return 1
        fi
        if [ ! -L "$BIN_DIR/al" ]; then
            printf 'install.sh: error: rollback verification found no active al symlink\n' >&2
            return 1
        fi
        if ! RESTORED_LINK_TARGET="$(readlink "$BIN_DIR/al")"; then
            printf 'install.sh: error: rollback could not read the restored active al symlink\n' >&2
            return 1
        fi
        if [ "$RESTORED_LINK_TARGET" != "$OLD_LINK_TARGET" ]; then
            printf 'install.sh: error: rollback restored the wrong active al target\n' >&2
            return 1
        fi
    else
        if ! rm -f "$BIN_DIR/al"; then
            printf 'install.sh: error: rollback failed to remove the newly activated al symlink\n' >&2
            return 1
        fi
        if [ -e "$BIN_DIR/al" ] || [ -L "$BIN_DIR/al" ]; then
            printf 'install.sh: error: rollback verification found an unexpected active al path\n' >&2
            return 1
        fi
    fi
    return 0
}

rollback_install() {
    ROLLBACK_FAILED=0
    if [ "$HAD_DEST" -eq 1 ]; then
        if [ -e "$VERSIONED_ASIDE" ] || [ -L "$VERSIONED_ASIDE" ]; then
            if ! mv -f "$VERSIONED_ASIDE" "$DEST"; then
                printf 'install.sh: error: rollback failed to restore prior versioned binary: %s\n' "$DEST" >&2
                ROLLBACK_FAILED=1
            fi
        elif [ ! -e "$DEST" ] && [ ! -L "$DEST" ]; then
            printf 'install.sh: error: rollback found neither prior destination nor its aside: %s\n' "$DEST" >&2
            ROLLBACK_FAILED=1
        fi
        if [ ! -e "$DEST" ] && [ ! -L "$DEST" ]; then
            printf 'install.sh: error: rollback verification found no restored versioned binary: %s\n' "$DEST" >&2
            ROLLBACK_FAILED=1
        fi
    else
        if ! rm -f "$DEST"; then
            printf 'install.sh: error: rollback failed to remove new versioned binary: %s\n' "$DEST" >&2
            ROLLBACK_FAILED=1
        elif [ -e "$DEST" ] || [ -L "$DEST" ]; then
            printf 'install.sh: error: rollback verification found an unexpected versioned binary: %s\n' "$DEST" >&2
            ROLLBACK_FAILED=1
        fi
    fi

    if ! rollback_active_link; then
        ROLLBACK_FAILED=1
    fi
    [ "$ROLLBACK_FAILED" -eq 0 ]
}

fail_after_rollback() {
    ROLLBACK_CAUSE="$1"
    ROLLBACK_RUNNING=1
    if rollback_install; then
        TRANSACTION_ACTIVE=0
        err "$ROLLBACK_CAUSE; previous install restored"
    fi
    err "$ROLLBACK_CAUSE; rollback failed"
}

if [ -e "$DEST" ] || [ -L "$DEST" ]; then
    [ ! -e "$VERSIONED_ASIDE" ] && [ ! -L "$VERSIONED_ASIDE" ] \
        || { rm -f "$TMP_LINK" "$STAGED"; err "versioned rollback path already exists: $VERSIONED_ASIDE"; }
    HAD_DEST=1
    TRANSACTION_ACTIVE=1
    mv "$DEST" "$VERSIONED_ASIDE" \
        || { rm -f "$TMP_LINK" "$STAGED"; err "failed to preserve existing versioned binary"; }
else
    TRANSACTION_ACTIVE=1
fi

if ! mv -f "$STAGED" "$DEST"; then
    fail_after_rollback "failed to activate versioned binary"
fi
STAGED=""

if ! mv -f "$TMP_LINK" "$BIN_DIR/al"; then
    fail_after_rollback "failed to activate al binary"
fi
TMP_LINK=""

# Record the exact release-archive identity used by any in-place updater so
# republished tags are detected by checksum.
STATE_FILE="$AL_HOME/update-state.json"
if [ -d "$STATE_FILE" ]; then
    fail_after_rollback "update-state path is a directory: $STATE_FILE"
fi

if ! STATE_TMP="$(mktemp "$AL_HOME/.update-state.XXXXXX")"; then
    STATE_TMP=""
    fail_after_rollback "could not create temporary update state under $AL_HOME"
fi

if ! CHECKED_AT="$(date -u +%s)"; then
    fail_after_rollback "could not determine the current Unix timestamp"
fi
case "$CHECKED_AT" in
    *[!0-9]*|'')
        fail_after_rollback "could not determine the current Unix timestamp"
        ;;
esac
printf '{\n  "installed_version": "%s",\n  "installed_asset": "%s",\n  "installed_sha256": "%s",\n  "installed_binary": "%s",\n  "checked_at_unix": %s\n}\n' \
    "$RESOLVED_VERSION" "$ASSET" "$EXPECTED" "$VERSIONED" "$CHECKED_AT" > "$STATE_TMP" || {
    fail_after_rollback "could not write update state"
}
if ! mv -f "$STATE_TMP" "$STATE_FILE"; then
    fail_after_rollback "could not record al update state"
fi
STATE_TMP=""
TRANSACTION_ACTIVE=0

# State is committed; safe to discard the old versioned binary aside.
rm -f "$VERSIONED_ASIDE"

printf '\nal v%s installed to %s\n' "$RESOLVED_VERSION" "$BIN_DIR/al"

case ":$PATH:" in
    *":$BIN_DIR:"*)
        printf 'Run `al` to get started.\n'
        ;;
    *)
        # Persist BIN_DIR on PATH in the login shell's rc file.  The install
        # itself is already committed; a failure here only means the user must
        # add the line manually.
        persist_line() {
            rc="$1"
            line="$2"
            if [ -f "$rc" ] && grep -qF "$line" "$rc"; then
                printf '\n%s is already configured in %s.\n' "$line" "$rc"
                return 0
            fi
            if printf '\n# Added by the al installer\n%s\n' "$line" >> "$rc"; then
                printf '\nAdded %s to your PATH in %s.\n' "$line" "$rc"
            else
                printf '\ninstall.sh: warning: could not write %s.\n' "$rc" >&2
                printf 'Add this line to your shell profile manually:\n  %s\n' "$line"
            fi
        }
        # POSIX single-quote escape: backslashes are literal; a single quote
        # is represented by closing the quote, adding an escaped quote, then
        # reopening. This keeps the generated rc line safe for any BIN_DIR.
        sh_quote() {
            printf '%s\n' "$1" | sed "s/'/'\\\\''/g"
        }
        SH_BIN_DIR="$(sh_quote "$BIN_DIR")"
        EXPORT_LINE="export PATH='$SH_BIN_DIR':\$PATH"
        # Fish single-quoted string: escape backslashes first, then single
        # quotes. In a single-quoted Fish string only \ and \' are special.
        fish_quote() {
            printf '%s' "$1" | sed 's/\\/\\\\/g; s/'"'"'/\\'"'"'/g'
        }
        FISH_DIR="$(fish_quote "$BIN_DIR")"
        case "${SHELL:-}" in
            */zsh)
                persist_line "${ZDOTDIR:-$HOME}/.zshrc" "$EXPORT_LINE"
                ;;
            */bash)
                if [ "$PLATFORM_OS" = "macos" ]; then
                    persist_line "$HOME/.bash_profile" "$EXPORT_LINE"
                else
                    persist_line "$HOME/.bashrc" "$EXPORT_LINE"
                fi
                ;;
            */fish)
                FISH_CONF_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/fish"
                if mkdir -p "$FISH_CONF_DIR" 2>/dev/null; then
                    persist_line "$FISH_CONF_DIR/config.fish" "fish_add_path -- '$FISH_DIR'"
                else
                    printf '\ninstall.sh: warning: could not create %s.\n' "$FISH_CONF_DIR" >&2
                    printf 'Add this line to your fish config manually:\n  fish_add_path -- '\''%s'\''\n' "$FISH_DIR"
                fi
                ;;
            *)
                persist_line "$HOME/.profile" "$EXPORT_LINE"
                ;;
        esac
        printf 'Open a new terminal, then run `al` to get started.\n'
        ;;
esac

# Final cleanup: by now the install is committed.
[ ! -e "$VERSIONED_ASIDE" ] || rm -f "$VERSIONED_ASIDE"
