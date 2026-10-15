# Searching only looks: indexing, rescans, content and property searches
# and previews must not change anything they touch, not even access
# times. The files live on a tmpfs mounted with strictatime, where every
# ordinary read would update an access time. Sourced by run.sh:
# setup_readonly before the daemon starts, check_readonly at the end.

RO_STATE=/tmp/e2e-local/ro-state

setup_readonly() {
    RO=/tmp/e2e-local/ro
    mkdir -p "$RO" "$RO_STATE"
    mount -t tmpfs -o strictatime e2e-ro "$RO" || return 1
    mkdir -p "$RO/dir/sub" "$RO/עברית"
    printf 'needle in a haystack\n' >"$RO/dir/note.txt"
    echo "preview me" >"$RO/dir/ro-preview.txt"
    head -c 5000 /dev/urandom >"$RO/dir/sub/blob.dat"
    echo "בדיקה" >"$RO/עברית/קובץ.txt"
    # Media for the property functions (distinct from the feature suite's).
    python3 - "$RO/dir" <<'PY'
import struct, sys, wave, zlib
d = sys.argv[1]
def chunk(kind, data):
    return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))
raw = b"".join(b"\0" + b"\x00\x80\xff" * 5 for _ in range(4))
png = (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", 5, 4, 8, 2, 0, 0, 0))
       + chunk(b"IDAT", zlib.compress(raw)) + chunk(b"IEND", b""))
open(f"{d}/ro-image.png", "wb").write(png)
with wave.open(f"{d}/ro-tone.wav", "wb") as w:
    w.setnchannels(1); w.setsampwidth(2); w.setframerate(8000)
    w.writeframes(b"\0\0" * 24000)
PY
    (cd "$RO" && find . -type f -print0 | sort -z | xargs -0 sha256sum) >"$RO_STATE/sums"
    find "$RO" -print0 | sort -z >"$RO_STATE/list"
    # Old times, so that even relatime would update them on a read.
    xargs -0 touch -h -d '2001-02-03 04:05:06' <"$RO_STATE/list"
    stat_snapshot "$RO_STATE/list" >"$RO_STATE/before"
}

check_readonly() {
    section "searching never changes what it searches"
    [ -s "$RO_STATE/before" ] || { fail "read-only fixture missing"; return; }
    evos --reindex
    sleep 2
    wait_for 30 phase_is "$RO" live || info "volume state: $(status_field "$RO" 3)"
    has_hits 'content:needle' && pass "content found on the strictatime volume" ||
        fail "content search on the strictatime volume"
    evos --count 'content:"no such text anywhere"' >/dev/null
    evos --count 'width:>0' >/dev/null
    evos --count 'length:>0' >/dev/null
    evos --count 'dupe: size:>0' >/dev/null
    evos -export-efu "$RO_STATE/all.efu" "under:$RO"
    stat_snapshot "$RO_STATE/list" >"$RO_STATE/after"
    if diff "$RO_STATE/before" "$RO_STATE/after" >"$RO_STATE/diff"; then
        pass "no access, modification or change time moved ($(wc -l <"$RO_STATE/before") entries)"
    else
        fail "times changed:"
        head -20 "$RO_STATE/diff" | sed 's/^/      /'
    fi
    # Only now list the folders again (that would itself touch them).
    find "$RO" -print0 | sort -z | cmp -s - "$RO_STATE/list" &&
        pass "no file was created or removed" || fail "the file set changed"
    (cd "$RO" && sha256sum --quiet -c "$RO_STATE/sums") &&
        pass "every file's content is unchanged" || fail "file content changed"
}
