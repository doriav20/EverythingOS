#!/usr/bin/env python3
"""An ETP client written from the protocol, independent of EverythingOS's
own Rust client: the commands Everything sends when it connects to an ETP
server, read with Python's socket and ftplib modules.

    etp_client.py HOST PORT USER PASSWORD DISK

Prints PASS/FAIL lines.
"""

import ftplib
import io
import socket
import sys

failures = 0


def check(ok, what):
    global failures
    print(("PASS  " if ok else "FAIL  ") + "etp: " + what, flush=True)
    if not ok:
        failures += 1
    return ok


class Control:
    """The control connection, line by line."""

    def __init__(self, host, port):
        self.sock = socket.create_connection((host, port), timeout=15)
        self.file = self.sock.makefile("rb")

    def reply(self):
        lines = []
        while True:
            line = self.file.readline().decode("utf-8").rstrip("\r\n")
            lines.append(line)
            if len(line) >= 4 and line[3] == " " and line[:3].isdigit():
                return int(line[:3]), lines

    def cmd(self, line):
        self.sock.sendall(line.encode("utf-8") + b"\r\n")
        return self.reply()


def parse_results(lines):
    """Results of an EVERYTHING QUERY reply: a dict per FILE/FOLDER."""
    total, items, cur = None, [], {}
    for line in lines[1:-1]:
        key, _, value = line.strip().partition(" ")
        if key == "RESULT_COUNT":
            total = int(value)
        elif key in ("FILE", "FOLDER"):
            cur["name"], cur["folder"] = value, key == "FOLDER"
            items.append(cur)
            cur = {}
        else:
            cur[key] = value
    return total, items


def everything_client(host, port, user, password, disk):
    c = Control(host, port)
    code, banner = c.reply()
    check(code == 220, f"banner {banner}")
    check(c.cmd(f"USER {user}")[0] == 331, "USER")
    check(c.cmd(f"PASS {password}")[0] == 230, "PASS")
    check(c.cmd("OPTS UTF8 ON")[0] == 200, "OPTS UTF8 ON")
    code, feat = c.cmd("FEAT")
    check(any(l.strip() == "EVERYTHING" for l in feat), f"FEAT lists EVERYTHING {feat}")

    # The settings Everything sends before a query, each answered 200.
    for setting in ["CASE 0", "WHOLE_WORD 0", "PATH 0", "DIACRITICS 0", "REGEX 0",
                    "SORT NAME_ASCENDING", "OFFSET 0", "COUNT 100", "PATH_COLUMN 1",
                    "SIZE_COLUMN 1", "DATE_MODIFIED_COLUMN 1", "ATTRIBUTES_COLUMN 1",
                    "SEARCH report"]:
        code, lines = c.cmd(f"EVERYTHING {setting}")
        check(code == 200, f"EVERYTHING {setting} → {lines[-1]}")
    code, lines = c.cmd("EVERYTHING QUERY")
    check(code == 200 and lines[0].startswith("200-") and lines[-1] == "200 End.",
          f"EVERYTHING QUERY answers on the control connection {lines[:1]} … {lines[-1:]}")
    total, items = parse_results(lines)
    names = sorted(i["name"] for i in items)
    check(total == 2 and names == ["q1 report.pdf", "q2 report.pdf"],
          f"results {total} {names}")
    q1 = next((i for i in items if i["name"] == "q1 report.pdf"), {})
    check(q1.get("PATH") == f"{disk}/docs", f"the PATH column {q1.get('PATH')!r}")
    check(q1.get("SIZE") == "3000", f"the SIZE column {q1.get('SIZE')!r}")
    # Windows FILETIME: 100 ns intervals since 1601.
    filetime = int(q1.get("DATE_MODIFIED", "0"))
    check(filetime > 116444736000000000, f"DATE_MODIFIED is a FILETIME {filetime}")
    attrs = int(q1.get("ATTRIBUTES", "-1"))
    check(attrs >= 0 and attrs & 0x10 == 0, f"ATTRIBUTES are Windows attribute bits {attrs}")

    # SITE EVERYTHING works the same; a window of the sorted results.
    c.cmd("SITE EVERYTHING SORT SIZE_DESCENDING")
    c.cmd("SITE EVERYTHING COUNT 1")
    c.cmd("SITE EVERYTHING OFFSET 1")
    code, lines = c.cmd("SITE EVERYTHING QUERY")
    total, items = parse_results(lines)
    check(total == 2 and [i["name"] for i in items] == ["q2 report.pdf"],
          f"SITE EVERYTHING with SORT, OFFSET and COUNT {total} {[i['name'] for i in items]}")

    c.cmd("EVERYTHING OFFSET 0")
    c.cmd("EVERYTHING COUNT 10")
    c.cmd("EVERYTHING SEARCH שלום")
    total, items = parse_results(c.cmd("EVERYTHING QUERY")[1])
    check([i["name"] for i in items] == ["שָׁלוֹם.txt"], f"Hebrew search {[i['name'] for i in items]}")
    c.cmd("EVERYTHING SEARCH plans")
    total, _ = parse_results(c.cmd("EVERYTHING QUERY")[1])
    check(total == 0, "a folder the server's account cannot enter is not searched")
    check(c.cmd("EVERYTHING BOGUS 1")[0] >= 500, "unknown settings are refused")
    check(c.cmd("QUIT")[0] == 221, "QUIT")


def plain_ftp(host, port, user, password, disk):
    ftp = ftplib.FTP()
    ftp.encoding = "utf-8"
    ftp.connect(host, port, timeout=15)
    try:
        ftp.login(user, "wrong " + password)
        check(False, "a wrong password is refused")
    except ftplib.error_perm:
        check(True, "a wrong password is refused")
    ftp.close()

    ftp = ftplib.FTP()
    ftp.encoding = "utf-8"
    ftp.connect(host, port, timeout=15)
    ftp.login(user, password)
    ftp.cwd(f"{disk}/עברית")
    check(ftp.pwd() == f"{disk}/עברית", "CWD into a Hebrew folder")
    names = ftp.nlst()
    check("שָׁלוֹם.txt" in names, f"NLST {names}")
    data = io.BytesIO()
    ftp.retrbinary("RETR שָׁלוֹם.txt", data.write)
    check(data.getvalue().decode() == "שלום עולם\n", "RETR of a Hebrew file")
    check(ftp.size(f"{disk}/docs/q1 report.pdf") == 3000, "SIZE")
    entries = list(ftp.mlsd(f"{disk}/docs"))
    check(any(n == "q1 report.pdf" and f.get("size") == "3000" for n, f in entries), "MLSD")
    for what, call in [
        ("STOR", lambda: ftp.storbinary(f"STOR {disk}/docs/uploaded.txt", io.BytesIO(b"x"))),
        ("DELE", lambda: ftp.delete(f"{disk}/docs/notes.md")),
        ("RNFR", lambda: ftp.rename(f"{disk}/docs/notes.md", f"{disk}/docs/moved.md")),
        ("MKD", lambda: ftp.mkd(f"{disk}/docs/newdir")),
        ("RMD", lambda: ftp.rmd(f"{disk}/empty")),
    ]:
        try:
            call()
            check(False, f"{what} is refused")
        except ftplib.error_perm as e:
            check(str(e).startswith("5"), f"{what} is refused ({e})")
    try:
        ftp.retrbinary(f"RETR {disk}/private/plans.txt", lambda b: None)
        check(False, "files the server's account may not read are not sent")
    except ftplib.error_perm:
        check(True, "files the server's account may not read are not sent")
    ftp.quit()


def main():
    host, port, user, password, disk = sys.argv[1], int(sys.argv[2]), *sys.argv[3:6]
    for test in (everything_client, plain_ftp):
        try:
            test(host, port, user, password, disk)
        except Exception as e:  # a crash fails, it does not skip
            check(False, f"{test.__name__} crashed: {e!r}")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
