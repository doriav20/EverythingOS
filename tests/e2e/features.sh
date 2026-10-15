# Search features end to end: the Everything 1.5 syntax, names in many
# scripts, content and media properties, run history, exports, live
# updates and per-user filtering, through the real daemon and CLI.
# Sourced by run.sh: make_tree before the daemon starts, run_features once
# it indexes $DISK.

# make_tree ROOT: the test data.
make_tree() {
    local d=$1
    mkdir -p "$d"/{docs,photos,music,עברית,عربي,Ελληνικά,日本語,emoji,dupes/a,dupes/b,empty,.hidden,bin,private,deep/a/b/c}
    head -c 3000 /dev/zero >"$d/docs/q1 report.pdf"
    head -c 1000 /dev/zero >"$d/docs/q2 report.pdf"
    echo "curriculum vitae" >"$d/docs/Résumé.txt"
    printf 'TODO: buy milk\nsecond line\n' >"$d/docs/notes.md"
    echo "שלום עולם" >"$d/עברית/שָׁלוֹם.txt"
    : >"$d/עברית/מסמך חשוב.docx"
    echo "مرحبا" >"$d/عربي/مرحبا.txt"
    : >"$d/Ελληνικά/αρχείο.txt"
    : >"$d/日本語/ファイル.txt"
    : >"$d/emoji/🎉 party.txt"
    echo same >"$d/dupes/a/same.txt"
    echo same >"$d/dupes/b/same.txt"
    echo secret >"$d/.hidden/secret.txt"
    printf '#!/bin/sh\necho hi\n' >"$d/bin/tool.sh"
    chmod +x "$d/bin/tool.sh"
    echo "read only" >"$d/docs/readonly.txt"
    chmod 444 "$d/docs/readonly.txt"
    : >"$d/deep/a/b/c/bottom.txt"
    ln -s docs "$d/link-to-docs"
    # Enough results to scroll through in the window.
    mkdir -p "$d/bulk"
    seq -f "$d/bulk/bulk-%05g.dat" 0 4999 | xargs touch
    echo "top secret plans" >"$d/private/plans.txt"
    chmod 700 "$d/private"
    echo "root only" >"$d/docs/root-only.txt"
    chmod 600 "$d/docs/root-only.txt"
    python3 - "$d" <<'PY'
import struct, sys, wave, zlib
root = sys.argv[1]
def chunk(kind, data):
    return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))
raw = b"".join(b"\0" + b"\xff\x00\x00" * 3 for _ in range(2))
png = (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", 3, 2, 8, 2, 0, 0, 0))
       + chunk(b"IDAT", zlib.compress(raw)) + chunk(b"IEND", b""))
open(f"{root}/photos/cat.png", "wb").write(png)
with wave.open(f"{root}/music/tone.wav", "wb") as w:
    w.setnchannels(1); w.setsampwidth(2); w.setframerate(8000)
    w.writeframes(b"\0\0" * 8000)
open(f"{root}/docs/utf16.txt", "wb").write("﻿hello unicode".encode("utf-16-le"))
PY
}

run_features() {
    section "search syntax"
    wait_for 30 has_hits bottom.txt || fail "the local volume was not indexed"
    expect 2 report
    expect 0 case:REPORT
    expect 2 case:report
    expect 1 '"q1 report"'
    expect 2 '*.pdf'
    expect 0 'q?.pdf'
    expect 2 'q?*.pdf'
    expect 1 "$DISK/deep/*/b"
    expect 1 "**/c/bottom.txt"
    expect 0 "$DISK/*/bottom.txt"
    expect 1 "$DISK/**/bottom.txt"
    expect 3 'q1|q2|notes'
    expect 1 'report !q2'
    expect 2 '<q1|q2> pdf'
    expect 3 'ext:pdf;md'
    expect 2 "file: size:>2kb under:$DISK"
    expect 2 'size:500..4kb file:'
    expect 2 'stem:report'
    expect 1 'startwith:q1'
    expect 2 'endwith:.pdf'
    expect 2 'ww:report'
    expect 0 'ww:repo'
    expect 2 'prefix:rep'
    expect 1 'regex:^q1\s'
    expect 2 'type:"PDF Document"'
    expect 1 'parent:'"$DISK"'/docs ext:md'
    expect 1 'under:'"$DISK"'/deep file:'
    expect 1 "depth:$(($(echo "$DISK" | tr -cd / | wc -c) + 5)) under:$DISK/deep"
    expect 1 'empty:'
    expect 1 'exe:'
    expect 1 "pic: under:$DISK"
    expect 1 "audio: under:$DISK"
    expect 1 'attrib:R file: under:'"$DISK"'/docs'
    expect 1 'attrib:H folder:'
    expect 1 'attrib:L'
    expect 2 'child:same.txt'
    expect 2 'dupe: same.txt'
    expect 1 'count:1 ext:pdf'
    expect 2 'root:' # this volume and the read-only one
    expect 2 'dm:today ext:pdf'
    expect 2 'rc:today ext:pdf'
    expect 0 'dm:<2000 ext:pdf'

    section "names in other scripts and diacritics"
    expect 1 שלום
    expect 1 'שָׁלוֹם'
    expect 1 'מסמך חשוב'
    expect 1 'מִסְמָךְ'
    expect 1 مرحبا
    expect 1 αρχείο
    expect 1 ΑΡΧΕΊΟ
    expect 1 ファイル
    expect 1 🎉
    expect 1 resume
    expect 1 Résumé
    expect 0 diacritics:resume
    expect 1 diacritics:Résumé
    expect 0 -a resume
    expect_names "שָׁלוֹם.txt" עברית/ש

    section "content and properties"
    expect 1 'content:milk'
    expect 1 'content:"second line"'
    expect 1 'regex:content:^TODO'
    expect 1 'content:unicode'
    expect 1 'content:שלום'
    expect 0 'content:nothing-like-this'
    expect 1 'width:3 height:2'
    expect 1 'length:1sec'
    expect 0 'length:>1min'

    section "results, sorting and exports"
    expect_names "q1 report.pdf|q2 report.pdf" -sort size-descending ext:pdf
    expect_names "q2 report.pdf" -sort size -n 1 ext:pdf
    expect_names "q2 report.pdf|q1 report.pdf" /os ext:pdf
    csv=$(evos -csv -name -size ext:pdf -sort name)
    if [ "$csv" = $'Name,Size\r\n"q1 report.pdf",3000\r\n"q2 report.pdf",1000\r' ]; then
        pass "CSV export with chosen columns"
    else
        fail "CSV export: $csv"
    fi
    evos -export-efu /tmp/e2e-local/pdfs.efu ext:pdf
    expect 1 --filelist /tmp/e2e-local/pdfs.efu q1
    [ "$(evos --get-total-size ext:pdf)" = 4000 ] && pass "total size" || fail "total size: $(evos --get-total-size ext:pdf)"
    evos stats | grep -q '^TOTAL' && pass "statistics" || fail "statistics: $(evos stats)"

    section "run history"
    evos --inc-run-count "$DISK/docs/notes.md"
    evos --inc-run-count "$DISK/docs/notes.md"
    [ "$(evos --get-run-count "$DISK/docs/notes.md")" = 2 ] && pass "run count recorded" || fail "run count"
    expect 1 'runcount:2'
    expect 1 'dr:today'
    expect_names "notes.md" -sort run-count-descending -n 1 file:

    section "live updates"
    t=$(now)
    echo new >"$DISK/docs/live-new.txt"
    wait_for 10 has_hits live-new.txt && pass "new file found after $(since "$t")s" || fail "new file not found"
    mv "$DISK/docs/live-new.txt" "$DISK/docs/live-renamed.txt"
    wait_for 10 has_hits live-renamed.txt && wait_for 10 no_hits live-new.txt &&
        pass "rename followed" || fail "rename not followed"
    rm "$DISK/docs/live-renamed.txt"
    wait_for 10 no_hits live-renamed.txt && pass "deletion followed" || fail "deletion not followed"

    section "results are filtered per user"
    as_user() { setpriv --reuid 4242 --regid 4242 --clear-groups env EVOS_SOCKET="$EVOS_SOCKET" evos "$@"; }
    n=$(as_user --count plans 2>/dev/null)
    [ "${n:-0}" -eq 0 ] && pass "another user cannot see files in a 0700 folder" || fail "private file visible to another user"
    n=$(as_user --count 'content:"root only"' 2>/dev/null)
    [ "${n:-0}" -eq 0 ] && pass "content search skips files the user cannot read" || fail "content of a 0600 file leaked"
    n=$(as_user --count report 2>/dev/null)
    [ "${n:-0}" -eq 2 ] && pass "other files stay visible to that user" || fail "user sees $n reports"
}
