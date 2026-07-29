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
# invocations to the system utilities. Paths must not contain whitespace
# because `$BB` is expanded unquoted, mirroring how pivot-init uses it.

set -u

TEST_DIR="$(dirname "$0")"
ENSURE_SCRIPT="${1:-$TEST_DIR/../ensure-localhost-hosts}"
if [ ! -f "$ENSURE_SCRIPT" ]; then
    echo "ensure-localhost-hosts not found at $ENSURE_SCRIPT" >&2
    exit 2
fi

WORK="${TMPDIR:-/tmp}/ensure-hosts-test.$$"
mkdir -p "$WORK" || exit 2
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
    $BB sh "$ENSURE_SCRIPT" "$1"
}

# Compare full file content including trailing newlines: command
# substitution strips them, so both sides carry an `x` sentinel.
file_content() {
    cat "$1" 2>/dev/null
    printf x
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
run_ensure "$d/hosts"; assert_eq "absent file: exit 0" 0 $?
assert_file "absent file: created with localhost mappings" "$d/hosts" "$BOTH"

# --- empty file (Docker/Kubernetes placeholder) gets both mappings --------
d="$WORK/empty"; mkdir -p "$d"
: > "$d/hosts"
run_ensure "$d/hosts"; assert_eq "empty file: exit 0" 0 $?
assert_file "empty file: localhost mappings appended" "$d/hosts" "$BOTH"

# --- unrelated entries are preserved, mappings appended -------------------
d="$WORK/unrelated"; mkdir -p "$d"
printf '10.0.0.7 db.internal\n192.168.1.4 cache cache.internal\n' > "$d/hosts"
run_ensure "$d/hosts"
assert_file "unrelated entries: preserved and localhost appended" "$d/hosts" \
"10.0.0.7 db.internal${NL}192.168.1.4 cache cache.internal${NL}${BOTH}"

# --- satisfied file stays byte-identical ----------------------------------
d="$WORK/satisfied"; mkdir -p "$d"
printf '127.0.0.1\tlocalhost\n::1\tlocalhost ip6-localhost ip6-loopback\n' > "$d/hosts"
before="$(file_content "$d/hosts")"
run_ensure "$d/hosts"; assert_eq "satisfied file: exit 0" 0 $?
assert_eq "satisfied file: byte-identical" "$before" "$(file_content "$d/hosts")"

# --- re-running never duplicates entries ----------------------------------
d="$WORK/rerun"; mkdir -p "$d"
: > "$d/hosts"
run_ensure "$d/hosts"
run_ensure "$d/hosts"
run_ensure "$d/hosts"
assert_file "re-run: entries appended exactly once" "$d/hosts" "$BOTH"

# --- names merely containing 'localhost' do not satisfy -------------------
d="$WORK/prefix"; mkdir -p "$d"
printf '127.0.0.1 localhost.localdomain\n::1 ip6-localhost\n' > "$d/hosts"
run_ensure "$d/hosts"
assert_file "prefix names: real localhost entries still appended" "$d/hosts" \
"127.0.0.1 localhost.localdomain${NL}::1 ip6-localhost${NL}${BOTH}"

# --- localhost among aliases satisfies its family -------------------------
d="$WORK/alias"; mkdir -p "$d"
printf '127.0.0.1 myapp localhost myapp.local\n' > "$d/hosts"
run_ensure "$d/hosts"
assert_file "aliases: only missing IPv6 mapping appended" "$d/hosts" \
"127.0.0.1 myapp localhost myapp.local${NL}::1 localhost${NL}"

# --- commented mappings do not satisfy ------------------------------------
d="$WORK/comment"; mkdir -p "$d"
printf '# 127.0.0.1 localhost\n#::1 localhost\n127.0.0.1 build # localhost\n' > "$d/hosts"
run_ensure "$d/hosts"
assert_file "comments: mappings appended" "$d/hosts" \
"# 127.0.0.1 localhost${NL}#::1 localhost${NL}127.0.0.1 build # localhost${NL}${BOTH}"

# --- missing trailing newline is repaired before appending ----------------
d="$WORK/nonewline"; mkdir -p "$d"
printf '10.0.0.7 svc' > "$d/hosts"
run_ensure "$d/hosts"
assert_file "no trailing newline: entries land on their own lines" "$d/hosts" \
"10.0.0.7 svc${NL}${BOTH}"

# --- IPv4-only file gains only the IPv6 mapping ---------------------------
d="$WORK/v4only"; mkdir -p "$d"
printf '127.0.0.1 localhost\n' > "$d/hosts"
run_ensure "$d/hosts"
assert_file "IPv4-only: IPv6 mapping appended" "$d/hosts" \
"127.0.0.1 localhost${NL}::1 localhost${NL}"

# --- whitespace variants satisfy detection --------------------------------
d="$WORK/whitespace"; mkdir -p "$d"
printf '  127.0.0.1\tlocalhost\n\t::1  \tlocalhost\n' > "$d/hosts"
before="$(file_content "$d/hosts")"
run_ensure "$d/hosts"
assert_eq "whitespace variants: byte-identical" "$before" "$(file_content "$d/hosts")"

# --- missing parent directory is created ----------------------------------
d="$WORK/noetc"
run_ensure "$d/etc/hosts"; assert_eq "missing parent dir: exit 0" 0 $?
assert_file "missing parent dir: file created" "$d/etc/hosts" "$BOTH"

# --- dangling symlink is replaced by a regular file -----------------------
d="$WORK/dangling"; mkdir -p "$d"
ln -s "$d/target-does-not-exist" "$d/hosts"
run_ensure "$d/hosts"
check "dangling symlink: replaced by regular file" test ! -L "$d/hosts"
assert_file "dangling symlink: mappings written" "$d/hosts" "$BOTH"

# --- valid symlink is preserved; appends follow it ------------------------
d="$WORK/symlink"; mkdir -p "$d/real"
: > "$d/real/hosts"
ln -s "$d/real/hosts" "$d/hosts"
run_ensure "$d/hosts"
check "valid symlink: link preserved" test -L "$d/hosts"
assert_file "valid symlink: target received mappings" "$d/real/hosts" "$BOTH"

# --- unwritable directory fails open with a warning -----------------------
if [ "$(id -u)" = "0" ]; then
    echo "skip: unwritable directory (running as root)"
else
    d="$WORK/readonly"; mkdir -p "$d"
    chmod 0555 "$d"
    run_ensure "$d/hosts"
    status=$?
    chmod 0755 "$d"
    assert_eq "unwritable dir: exit 1" 1 "$status"
    check "unwritable dir: no file created" test ! -e "$d/hosts"
fi

echo
echo "ensure-localhost-hosts tests: $PASS passed, $FAIL failed"
[ "$FAIL" = 0 ] || exit 1
exit 0
