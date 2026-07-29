#!/bin/sh
# Regression tests for tools-image/ensure-localhost-hosts.
#
# The tools-image Docker build runs this harness with the exact BusyBox
# binary that ships in the drive:
#
#   BB=<busybox> <busybox> sh test-ensure-localhost-hosts.sh <script>
#
# It also runs under any POSIX shell on a dev host: it prefers a `busybox`
# from PATH and otherwise falls back to a shim that forwards `$BB <applet>`
# invocations to the system utilities.

set -u

TEST_DIR="$(dirname "$0")"
ENSURE_SCRIPT="${1:-$TEST_DIR/../ensure-localhost-hosts}"
case "$ENSURE_SCRIPT" in
    /*) ;;
    *) ENSURE_SCRIPT="$(pwd)/$ENSURE_SCRIPT" ;;
esac
if [ ! -f "$ENSURE_SCRIPT" ]; then
    echo "ensure-localhost-hosts not found at $ENSURE_SCRIPT" >&2
    exit 2
fi

WORK="$(mktemp -d "${TMPDIR:-/tmp}/ensure-hosts-test.XXXXXX")" || exit 2
trap 'chmod -R u+w "$WORK" 2>/dev/null; rm -rf "$WORK"' EXIT

if [ -z "${BB:-}" ]; then
    if command -v busybox >/dev/null 2>&1; then
        BB=busybox
    else
        printf '#!/bin/sh\nexec "$@"\n' > "$WORK/bb-shim"
        chmod 0755 "$WORK/bb-shim"
        BB="$WORK/bb-shim"
    fi
fi
export BB

NL='
'
BOTH="127.0.0.1 localhost${NL}::1 localhost${NL}"

PASS=0
FAIL=0

run_ensure() {
    "$BB" sh "$ENSURE_SCRIPT" "$1"
}

# Invoked indirectly by check().
# shellcheck disable=SC2329
run_ensure_with_umask() (
    umask "$1"
    run_ensure "$2"
)

# Invoked indirectly by check().
# shellcheck disable=SC2329
run_ensure_from() (
    cd "$1" && run_ensure "$2"
)

# Invoked indirectly by check().
# shellcheck disable=SC2329
run_ensure_from_with_umask() (
    cd "$1" && umask "$2" && run_ensure "$3"
)

# Compare full file content including trailing newlines: command
# substitution strips them, so both sides carry an `x` sentinel.
file_content() {
    cat "$1" 2>/dev/null
    printf x
}

# GNU/BusyBox stat uses -c while BSD stat uses -f. The fallback shim exposes
# the host implementation, so support both forms.
file_mode() {
    if "$BB" stat -c '%a' -- "$1" 2>/dev/null; then
        return 0
    fi
    stat -f '%Lp' "$1" 2>/dev/null
}

assert_file() { # $1 case name, $2 path, $3 expected content
    if [ "$(file_content "$2")" = "${3}x" ]; then
        PASS=$((PASS + 1))
        echo "ok: $1"
    else
        FAIL=$((FAIL + 1))
        {
            echo "FAIL: $1"
            echo "----- expected -----"
            printf '%s' "$3"
            echo "----- actual -------"
            cat "$2" 2>/dev/null
            echo "--------------------"
        } >&2
    fi
}

assert_eq() { # $1 case name, $2 expected, $3 actual
    if [ "$2" = "$3" ]; then
        PASS=$((PASS + 1))
        echo "ok: $1"
    else
        FAIL=$((FAIL + 1))
        echo "FAIL: $1 (expected '$2', got '$3')" >&2
    fi
}

check() { # $1 case name, then a command to run
    name="$1"
    shift
    if "$@"; then
        PASS=$((PASS + 1))
        echo "ok: $name"
    else
        FAIL=$((FAIL + 1))
        echo "FAIL: $name" >&2
    fi
}

# --- absent file is created with both mappings ----------------------------
d="$WORK/absent"; mkdir -p "$d"
check "absent file: exit 0" run_ensure_with_umask 000 "$d/hosts"
assert_file "absent file: created with localhost mappings" "$d/hosts" "$BOTH"
assert_eq "absent file: mode normalized" 644 "$(file_mode "$d/hosts")"

# --- existing empty file gets mappings without changing its mode ----------
d="$WORK/empty"; mkdir -p "$d"
: > "$d/hosts"
chmod 0600 "$d/hosts"
check "empty file: exit 0" run_ensure "$d/hosts"
assert_file "empty file: localhost mappings appended" "$d/hosts" "$BOTH"
assert_eq "empty file: existing mode preserved" 600 "$(file_mode "$d/hosts")"

# --- unrelated entries are preserved, mappings appended -------------------
d="$WORK/unrelated"; mkdir -p "$d"
printf '10.0.0.7 db.internal\n192.168.1.4 cache cache.internal\n' > "$d/hosts"
check "unrelated entries: exit 0" run_ensure "$d/hosts"
assert_file "unrelated entries: preserved and localhost appended" "$d/hosts" \
"10.0.0.7 db.internal${NL}192.168.1.4 cache cache.internal${NL}${BOTH}"

# --- satisfied file stays byte-identical ----------------------------------
d="$WORK/satisfied"; mkdir -p "$d"
printf '127.0.0.1\tlocalhost\n::1\tlocalhost ip6-localhost ip6-loopback\n' > "$d/hosts"
chmod 0640 "$d/hosts"
before="$(file_content "$d/hosts")"
check "satisfied file: exit 0" run_ensure "$d/hosts"
assert_eq "satisfied file: byte-identical" "$before" "$(file_content "$d/hosts")"
assert_eq "satisfied file: mode preserved" 640 "$(file_mode "$d/hosts")"

# --- malformed IPv4 loopback does not suppress the canonical mapping -----
d="$WORK/malformed-v4"; mkdir -p "$d"
printf '127.999.999.999 localhost\n::1 localhost\n' > "$d/hosts"
check "malformed IPv4: first run exit 0" run_ensure "$d/hosts"
assert_file "malformed IPv4: canonical mapping appended" "$d/hosts" \
"127.999.999.999 localhost${NL}::1 localhost${NL}127.0.0.1 localhost${NL}"
before="$(file_content "$d/hosts")"
check "malformed IPv4: second run exit 0" run_ensure "$d/hosts"
assert_eq "malformed IPv4: re-run is byte-identical" "$before" "$(file_content "$d/hosts")"

# --- a non-canonical 127/8 mapping does not replace the canonical entry ----
d="$WORK/valid-v4"; mkdir -p "$d"
printf '127.12.34.56 localhost\n::1 localhost\n' > "$d/hosts"
check "non-canonical IPv4: exit 0" run_ensure "$d/hosts"
assert_file "non-canonical IPv4: canonical mapping appended" "$d/hosts" \
"127.12.34.56 localhost${NL}::1 localhost${NL}127.0.0.1 localhost${NL}"

# --- hostname matching is case-insensitive --------------------------------
d="$WORK/case"; mkdir -p "$d"
printf '127.0.0.1 LOCALHOST\n::1 LocalHost\n' > "$d/hosts"
before="$(file_content "$d/hosts")"
check "hostname case: exit 0" run_ensure "$d/hosts"
assert_eq "hostname case: byte-identical" "$before" "$(file_content "$d/hosts")"

# --- re-running never duplicates entries ----------------------------------
d="$WORK/rerun"; mkdir -p "$d"
: > "$d/hosts"
check "re-run: first exit 0" run_ensure "$d/hosts"
check "re-run: second exit 0" run_ensure "$d/hosts"
check "re-run: third exit 0" run_ensure "$d/hosts"
assert_file "re-run: entries appended exactly once" "$d/hosts" "$BOTH"

# --- names merely containing 'localhost' do not satisfy -------------------
d="$WORK/prefix"; mkdir -p "$d"
printf '127.0.0.1 localhost.localdomain\n::1 ip6-localhost\n' > "$d/hosts"
check "prefix names: exit 0" run_ensure "$d/hosts"
assert_file "prefix names: real localhost entries still appended" "$d/hosts" \
"127.0.0.1 localhost.localdomain${NL}::1 ip6-localhost${NL}${BOTH}"

# --- localhost among aliases satisfies its family -------------------------
d="$WORK/alias"; mkdir -p "$d"
printf '127.0.0.1 myapp localhost myapp.local\n' > "$d/hosts"
check "aliases: exit 0" run_ensure "$d/hosts"
assert_file "aliases: only missing IPv6 mapping appended" "$d/hosts" \
"127.0.0.1 myapp localhost myapp.local${NL}::1 localhost${NL}"

# --- commented mappings do not satisfy ------------------------------------
d="$WORK/comment"; mkdir -p "$d"
printf '# 127.0.0.1 localhost\n#::1 localhost\n127.0.0.1 build # localhost\n' > "$d/hosts"
check "comments: exit 0" run_ensure "$d/hosts"
assert_file "comments: mappings appended" "$d/hosts" \
"# 127.0.0.1 localhost${NL}#::1 localhost${NL}127.0.0.1 build # localhost${NL}${BOTH}"

# --- missing trailing newline is repaired before appending ----------------
d="$WORK/nonewline"; mkdir -p "$d"
printf '10.0.0.7 svc' > "$d/hosts"
check "no trailing newline: exit 0" run_ensure "$d/hosts"
assert_file "no trailing newline: entries land on their own lines" "$d/hosts" \
"10.0.0.7 svc${NL}${BOTH}"

# --- IPv4-only file gains only the IPv6 mapping ---------------------------
d="$WORK/v4only"; mkdir -p "$d"
printf '127.0.0.1 localhost\n' > "$d/hosts"
check "IPv4-only: exit 0" run_ensure "$d/hosts"
assert_file "IPv4-only: IPv6 mapping appended" "$d/hosts" \
"127.0.0.1 localhost${NL}::1 localhost${NL}"

# --- whitespace variants satisfy detection --------------------------------
d="$WORK/whitespace"; mkdir -p "$d"
printf '  127.0.0.1\tlocalhost\n\t::1  \tlocalhost\n' > "$d/hosts"
before="$(file_content "$d/hosts")"
check "whitespace variants: exit 0" run_ensure "$d/hosts"
assert_eq "whitespace variants: byte-identical" "$before" "$(file_content "$d/hosts")"

# --- missing parents and file have deterministic modes under umask 077 ----
d="$WORK/noetc"
check "missing parent dir: exit 0" run_ensure_with_umask 077 "$d/etc/hosts"
assert_file "missing parent dir: file created" "$d/etc/hosts" "$BOTH"
assert_eq "missing parent dir: mode normalized" 755 "$(file_mode "$d/etc")"
assert_eq "missing parent dir: file mode normalized" 644 "$(file_mode "$d/etc/hosts")"

# --- paths containing whitespace are supported ----------------------------
d="$WORK/path with spaces"
check "space path: exit 0" run_ensure "$d/etc/hosts"
assert_file "space path: file created" "$d/etc/hosts" "$BOTH"

# --- dangling symlink is replaced by a regular file -----------------------
d="$WORK/dangling"; mkdir -p "$d"
ln -s "$d/target-does-not-exist" "$d/hosts"
check "dangling symlink: exit 0" run_ensure_with_umask 077 "$d/hosts"
check "dangling symlink: replaced by regular file" test ! -L "$d/hosts"
assert_file "dangling symlink: mappings written" "$d/hosts" "$BOTH"
assert_eq "dangling symlink: mode normalized" 644 "$(file_mode "$d/hosts")"

# --- valid symlink is preserved; appends follow it ------------------------
d="$WORK/symlink"; mkdir -p "$d/real"
: > "$d/real/hosts"
chmod 0600 "$d/real/hosts"
ln -s "$d/real/hosts" "$d/hosts"
check "valid symlink: exit 0" run_ensure "$d/hosts"
check "valid symlink: link preserved" test -L "$d/hosts"
assert_file "valid symlink: target received mappings" "$d/real/hosts" "$BOTH"
assert_eq "valid symlink: target mode preserved" 600 "$(file_mode "$d/real/hosts")"

# --- option-like existing path is detected and remains idempotent ---------
d="$WORK/option-file"; mkdir -p "$d"
printf '%s' "$BOTH" > "$d/-hosts"
before="$(file_content "$d/-hosts")"
check "option-like file: first exit 0" run_ensure_from "$d" -hosts
check "option-like file: second exit 0" run_ensure_from "$d" -hosts
assert_eq "option-like file: re-runs are byte-identical" "$before" "$(file_content "$d/-hosts")"

# --- option-like dangling path is safely replaced -------------------------
d="$WORK/option-dangling"; mkdir -p "$d"
ln -s target-does-not-exist "$d/-hosts"
check "option-like dangling path: exit 0" run_ensure_from_with_umask "$d" 077 -hosts
check "option-like dangling path: link replaced" test ! -L "$d/-hosts"
assert_file "option-like dangling path: mappings written" "$d/-hosts" "$BOTH"
assert_eq "option-like dangling path: mode normalized" 644 "$(file_mode "$d/-hosts")"

# --- option-like missing parent is safely created -------------------------
d="$WORK/option-parent"; mkdir -p "$d"
check "option-like parent: exit 0" run_ensure_from_with_umask "$d" 077 -etc/hosts
assert_file "option-like parent: mappings written" "$d/-etc/hosts" "$BOTH"
assert_eq "option-like parent: directory mode normalized" 755 "$(file_mode "$d/-etc")"
assert_eq "option-like parent: file mode normalized" 644 "$(file_mode "$d/-etc/hosts")"

# --- symlink to a non-regular target fails open without writing -----------
d="$WORK/device-symlink"; mkdir -p "$d"
ln -s /dev/null "$d/hosts"
stderr="$d/stderr"
run_ensure "$d/hosts" 2> "$stderr"
status=$?
assert_eq "device symlink: exit 1" 1 "$status"
check "device symlink: link preserved" test -L "$d/hosts"
check "device symlink: warning emitted" grep -qF \
    "AGENTENV PIVOT INIT WARN: failed to write localhost entries to $d/hosts" \
    "$stderr"

# --- unusable path fails open with a warning, even when run as root -------
d="$WORK/unusable"; mkdir -p "$d"
: > "$d/not-a-directory"
stderr="$d/stderr"
run_ensure "$d/not-a-directory/hosts" 2> "$stderr"
status=$?
assert_eq "unusable path: exit 1" 1 "$status"
check "unusable path: no file created" test ! -e "$d/not-a-directory/hosts"
check "unusable path: warning emitted" grep -qF \
    "AGENTENV PIVOT INIT WARN: failed to write localhost entries to $d/not-a-directory/hosts" \
    "$stderr"

echo
echo "ensure-localhost-hosts tests: $PASS passed, $FAIL failed"
[ "$FAIL" = 0 ] || exit 1
exit 0
