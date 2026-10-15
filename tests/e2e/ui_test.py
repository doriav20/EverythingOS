#!/usr/bin/env python3
"""End-to-end test of the evos-gtk window.

The real window runs on Xvfb. It is driven through its command line
(forwarded to the running instance), its D-Bus actions (the same ones its
menus use) and mouse clicks, and read through the accessibility bus
(AT-SPI), so the checks see what a user sees: rows, column headers, the
status bar, the preview pane and dialogs.

    ui_test.py local DISK RO     the full suite on the local test volume
    ui_test.py share ROOT NAME   a file on a network share shows up
    ui_test.py share-mount SOURCE USER PASSWORD NAME
                                 an SMB share mounted from Preferences
"""

import os
import re
import subprocess
import sys
import time

import gi

gi.require_version("Atspi", "2.0")
from gi.repository import Atspi  # noqa: E402

WINDOW = "/org/evos/Search/window/1"
failures = 0


def check(ok, what):
    global failures
    print(("PASS  " if ok else "FAIL  ") + "ui: " + what, flush=True)
    if not ok:
        failures += 1
    return ok


def wait(predicate, timeout=15):
    """Polls until predicate() is truthy; returns its last value."""
    end = time.time() + timeout
    value = None
    while time.time() < end:
        try:
            value = predicate()
        except Exception:  # the tree changes under us while we read it
            value = None
        if value:
            return value
        time.sleep(0.2)
    return value


def app():
    desktop = Atspi.get_desktop(0)
    for i in range(desktop.get_child_count()):
        a = desktop.get_child_at_index(i)
        if a is not None and a.get_name() == "evos-gtk":
            return a
    return None


def walk(node, depth=0):
    if node is None or depth > 40:
        return
    yield node
    for i in range(node.get_child_count()):
        yield from walk(node.get_child_at_index(i), depth + 1)


def showing(node):
    return node.get_state_set().contains(Atspi.StateType.SHOWING)


def find_all(role=None, name=None, root=None, visible=True):
    out = []
    for n in walk(root or app()):
        if role and n.get_role_name() != role:
            continue
        if name is not None and n.get_name() != name:
            continue
        if visible and not showing(n):
            continue
        out.append(n)
    return out


def results_table():
    tables = find_all("tree table", "Results")
    return tables[0] if tables else None


def rows():
    """Visible result rows as lists of cell texts."""
    table = results_table()
    out = []
    for lst in find_all("list", root=table):
        for i in range(lst.get_child_count()):
            row = lst.get_child_at_index(i)
            if row.get_role_name() != "table row" or not showing(row):
                continue
            cells = [
                row.get_child_at_index(j).get_name()
                for j in range(row.get_child_count())
            ]
            if any(cells):
                out.append(cells)
    return out


def names():
    return [r[0] for r in rows()]


def header():
    table = results_table()
    for i in range(table.get_child_count()):
        child = table.get_child_at_index(i)
        if child.get_role_name() == "table row":
            return child.get_name()
    return ""


def count_text():
    for label in find_all("label"):
        name = label.get_name() or ""
        if re.match(r"^[\d,]+ objects?\b", name):
            return name
    return ""


def total():
    m = re.match(r"^([\d,]+)", count_text())
    return int(m.group(1).replace(",", "")) if m else -1


def text_of(node):
    try:
        return Atspi.Text.get_text(node, 0, -1) or ""
    except Exception:
        return ""


def bold_text(node):
    """The bold parts of a label: the highlighted search terms."""
    # Character by character: GTK reports a plain run as reaching the end
    # of the text, so runs cannot be skipped over.
    text = text_of(node)
    out = ""
    for offset, char in enumerate(text):
        attrs, _, _ = Atspi.Text.get_attribute_run(node, offset, False)
        if (attrs or {}).get("weight") == "700":
            out += char
    return out


def first_name_label():
    rows_ = visible_rows()
    for n in walk(rows_[0]) if rows_ else []:
        if n.get_role_name() == "label":
            return n
    return None


def remote(*args):
    """Runs evos-gtk; the arguments go to the running window."""
    subprocess.run(["evos-gtk", *args], check=True, timeout=30)


def action(name, param=None):
    """Activates a window action over D-Bus, as a menu item would."""
    params = "[]" if param is None else "[<%s>]" % gvariant_string(param)
    subprocess.run(
        ["gdbus", "call", "--session", "--dest", "org.evos.Search",
         "--object-path", WINDOW, "--method", "org.gtk.Actions.Activate",
         name, params, "{}"],
        check=True, stdout=subprocess.DEVNULL, timeout=30,
    )


def gvariant_string(s):
    return "'" + s.replace("\\", "\\\\").replace("'", "\\'") + "'"


def window_origin():
    """Screen position of the first search window."""
    out = subprocess.run(
        ["xdotool", "search", "--name", "^EverythingOS$", "getwindowgeometry"],
        capture_output=True, text=True,
    ).stdout
    m = re.search(r"Position: (-?\d+),(-?\d+)", out)
    return (int(m.group(1)), int(m.group(2))) if m else (0, 0)


def xdo(*args):
    subprocess.run(["xdotool", *args], check=True, timeout=30)


def click_node(node, button=1, mods=(), dx=None):
    """Clicks a widget of the first search window (or a dialog in it)."""
    # GTK reports positions relative to its window only; add the window's
    # place on the screen.
    rect = Atspi.Component.get_extents(node, Atspi.CoordType.WINDOW)
    wx, wy = window_origin()
    x = wx + rect.x + (rect.width // 2 if dx is None else dx)
    y = wy + rect.y + rect.height // 2
    for m in mods:
        xdo("keydown", m)
    xdo("mousemove", "--sync", str(x), str(y), "click", str(button))
    for m in mods:
        xdo("keyup", m)
    time.sleep(0.2)


def key(*keys):
    xdo("key", "--clearmodifiers", *keys)
    time.sleep(0.2)


def type_text(text):
    xdo("type", "--delay", "30", text)


def visible_rows():
    table = results_table()
    for lst in find_all("list", root=table):
        out = [lst.get_child_at_index(i) for i in range(lst.get_child_count())]
        return [r for r in out if r.get_role_name() == "table row" and showing(r)]
    return []


def click_row(index, button=1, mods=()):
    """Clicks a result row, which selects it."""
    row = wait(lambda: (visible_rows() + [None] * (index + 1))[index])
    if row is None:
        return False
    click_node(row, button, mods, dx=60)
    return True


def header_cell(title):
    table = results_table()
    for i in range(table.get_child_count()):
        row = table.get_child_at_index(i)
        if row.get_role_name() == "table row":
            for n in walk(row):
                if n.get_name() == title and n.get_role_name() != "label":
                    return n
    return None


def search_entry():
    entries = find_all("entry", "Search")
    return entries[0] if entries else None


def clipboard(target=None):
    cmd = ["xclip", "-o", "-selection", "clipboard"]
    if target:
        cmd += ["-t", target]
    r = subprocess.run(cmd, capture_output=True, text=True, timeout=10)
    return r.stdout if r.returncode == 0 else ""


def accessible_name(node):
    """The node's name, or the name of the label it is labelled by."""
    if node.get_name():
        return node.get_name()
    relations = node.get_relation_set() or []
    for rel in relations:
        if rel.get_relation_type() == Atspi.RelationType.LABELLED_BY and rel.get_n_targets():
            target = rel.get_target(0)
            return target.get_name() or text_of(target)
    return ""


def menu_item(name):
    for n in find_all("menu item"):
        if accessible_name(n).replace("_", "") == name:
            return n
    return None


def press(node):
    """Activates a button or menu item the way assistive technology does."""
    try:
        return Atspi.Action.do_action(node, 0)
    except Exception:
        return False


def run_count(path):
    out = subprocess.run(["evos", "--get-run-count", path], capture_output=True, text=True)
    return int(out.stdout.strip() or 0)


def app_action(name):
    subprocess.run(
        ["gdbus", "call", "--session", "--dest", "org.evos.Search",
         "--object-path", "/org/evos/Search", "--method", "org.gtk.Actions.Activate",
         name, "[]", "{}"],
        check=True, stdout=subprocess.DEVNULL, timeout=30,
    )


def find_named(name, roles, root=None):
    for n in find_all(root=root):
        if n.get_role_name() in roles and accessible_name(n).replace("_", "") == name:
            return n
    return None


def banner_showing():
    """The "cannot reach the service" banner is revealed (it is always in
    the tree, collapsed to no height while hidden)."""
    for n in find_all(name="Cannot reach the EverythingOS service"):
        if Atspi.Component.get_extents(n, Atspi.CoordType.WINDOW).height > 0:
            return True
    return False


def dialogs():
    return find_all("dialog") + find_all("alert")


def dialog_labels():
    """Texts of labels in an open dialog, or None if there is none."""
    for d in dialogs():
        return [n.get_name() for n in walk(d) if n.get_role_name() == "label"]
    return None


def close_dialog():
    """Closes the open dialog and waits until it is gone (a closing dialog
    still takes the clicks meant for the window)."""
    pressed = False
    for d in dialogs()[:1]:
        for b in walk(d):
            if b.get_role_name() == "push button" and \
                    accessible_name(b).replace("_", "") in ("Close", "OK"):
                pressed = bool(press(b))
                break
    if not pressed:
        subprocess.run(["xdotool", "key", "Escape"])
    wait(lambda: not dialogs(), timeout=5)
    time.sleep(0.3)
    return pressed


def preview_text():
    for panel in find_all("panel", "Preview"):
        for n in walk(panel):
            if n.get_role_name() == "text" and showing(n):
                return text_of(n)
    return ""


def preview_has_image():
    for panel in find_all("panel", "Preview"):
        for n in walk(panel):
            if n.get_role_name() in ("image", "picture") and showing(n):
                return True
    return False


def guarded(fn, *args):
    """Runs a group of checks; an exception fails it instead of silently
    skipping the rest."""
    try:
        fn(*args)
    except Exception:
        import traceback
        check(False, f"{fn.__name__} crashed: " + traceback.format_exc())


def search(query, *more):
    remote("-s", query, *more)


def local(disk, ro):
    settings_file = os.path.join(os.environ["XDG_CONFIG_HOME"], "evos", "evos-gtk.ini")

    search("report")
    check(wait(lambda: names() == ["q1 report.pdf", "q2 report.pdf"]),
          "a search from the command line reaches the running window")
    check(wait(lambda: count_text().startswith("2 objects")), "the status bar counts the results")
    check(wait(lambda: bold_text(first_name_label()) == "report"),
          "the search term is highlighted in the name "
          f"[{accessible_name(first_name_label())!r}: {bold_text(first_name_label())!r}]")

    search("שלום")
    check(wait(lambda: names() == ["שָׁלוֹם.txt"]),
          "a Hebrew search without niqqud finds the pointed name")
    check(wait(lambda: bold_text(first_name_label()) == "שָׁלוֹם"),
          "the highlight covers the pointed Hebrew word, niqqud included")
    search("ファイル")
    check(wait(lambda: names() == ["ファイル.txt"]), "Japanese names")
    search("ΑΡΧΕΊΟ")
    check(wait(lambda: names() == ["αρχείο.txt"]), "Greek names ignore case and accents")

    search("resume")
    check(wait(lambda: names() == ["Résumé.txt"]), "diacritics are ignored by default")
    remote("-diacritics")
    check(wait(lambda: total() == 0), "Search > Match Diacritics")
    remote("-no-diacritics")
    check(wait(lambda: total() == 1), "Match Diacritics off again")

    search("REPORT")
    wait(lambda: total() == 2)
    action("match-case")
    check(wait(lambda: total() == 0), "Search > Match Case (the menu action)")
    action("match-case")
    check(wait(lambda: total() == 2), "Match Case off again")

    remote("-filter", "pictures", "-s", "cat")
    check(wait(lambda: names() == ["cat.png"]), "the Pictures filter")
    remote("-filter", "folders", "-s", "dupes")
    check(wait(lambda: names() == ["dupes"]), "the Folders filter")
    remote("-filter", "everything")

    search("ext:pdf", "-sort", "size-descending")
    check(wait(lambda: names() == ["q1 report.pdf", "q2 report.pdf"]), "sort by size, descending")
    remote("-sort", "size-ascending")
    check(wait(lambda: names() == ["q2 report.pdf", "q1 report.pdf"]), "sort by size, ascending")
    remote("-sort", "name")

    remote("-add-columns", "type;attributes;extension")
    check(wait(lambda: all(t in header() for t in ("Type", "Attributes", "Extension"))),
          "columns added: " + header())
    check(wait(lambda: any("PDF Document" in r for r in rows())), "the Type column shows PDF Document")
    remote("-remove-columns", "attributes")
    check(wait(lambda: "Attributes" not in header()), "a column removed")

    remote("-preview", "-s", "notes.md")
    wait(lambda: names() == ["notes.md"])
    click_row(0)
    check(wait(lambda: preview_text().startswith("TODO: buy milk")), "the preview pane shows text")
    search("cat.png")
    wait(lambda: names() == ["cat.png"])
    click_row(0)
    check(wait(preview_has_image), "the preview pane shows images")
    if os.path.isdir(ro):
        search("ro-preview.txt")
        wait(lambda: names() == ["ro-preview.txt"])
        click_row(0)
        check(wait(lambda: preview_text().startswith("preview me")),
              "preview of a file on the strictatime volume")

    search("q1 report.pdf")
    wait(lambda: names() == ["q1 report.pdf"])
    click_row(0)
    action("open")
    path = os.path.join(disk, "docs", "q1 report.pdf")
    runs = wait(lambda: subprocess.run(["evos", "--get-run-count", path], capture_output=True,
                                       text=True).stdout.strip() == "1")
    check(runs, "opening a result counts a run")
    wait(lambda: dialog_labels() is not None, timeout=5)
    close_dialog()  # nothing in the container can open a PDF
    remote("-add-columns", "run-count")
    check(wait(lambda: any(r[-1] == "1" for r in rows())), "the Run Count column shows it")

    click_row(0)
    action("properties")
    labels = wait(dialog_labels) or []
    check("Date Modified" in labels and "Run Count" in labels and "PDF Document" in labels,
          "the Properties dialog")
    close_dialog()

    search("ext:pdf")
    wait(lambda: total() == 2)
    out = "/tmp/e2e-local/ui-export.csv"
    action("export-to", out)
    exported = wait(lambda: os.path.exists(out) and open(out, encoding="utf-8").read())
    check(bool(exported) and exported.startswith("Name,Path,Size") and "q2 report.pdf" in exported,
          "File > Export as CSV")
    efu = "/tmp/e2e-local/ui-export.efu"
    action("export-to", efu)
    data = wait(lambda: os.path.exists(efu) and open(efu, encoding="utf-8").read())
    check(bool(data) and data.startswith("Filename,Size,Date Modified") and data.count("\n") == 3,
          "File > Export as an Everything file list")

    action("add-bookmark", "PDFs")
    search("notes")
    wait(lambda: names() == ["notes.md"])
    action("open-bookmark", "PDFs")
    check(wait(lambda: names() == ["q1 report.pdf", "q2 report.pdf"]), "a bookmark restores its search")
    check(wait(lambda: "name=PDFs" in open(settings_file, encoding="utf-8").read()),
          "bookmarks are saved")
    check(wait(lambda: "ext:pdf" in open(settings_file, encoding="utf-8").read(), timeout=6),
          "searches go to the history")

    action("statistics")
    labels = wait(dialog_labels) or []
    check("Files" in labels and "Index Memory" in labels, "the Statistics dialog")
    close_dialog()

    search("ui-live-update")
    wait(lambda: total() == 0)
    open(os.path.join(disk, "docs", "ui-live-update.txt"), "w").close()
    check(wait(lambda: names() == ["ui-live-update.txt"]), "a new file appears in the open window")
    os.remove(os.path.join(disk, "docs", "ui-live-update.txt"))
    check(wait(lambda: total() == 0), "a deleted file disappears from the open window")

    guarded(keyboard_and_mouse, disk, settings_file)
    guarded(mounts_in_preferences, disk)
    guarded(service_restart)

    remote("-new-window", "-s", "report")
    check(wait(lambda: len(find_all("frame")) == 2), "File > New Window")


def keyboard_and_mouse(disk, settings_file):
    """What a user does with keyboard and mouse."""
    q1 = os.path.join(disk, "docs", "q1 report.pdf")
    q2 = os.path.join(disk, "docs", "q2 report.pdf")

    remote("-home", "-sort", "name")
    click_node(search_entry())
    type_text("report")
    check(wait(lambda: names() == ["q1 report.pdf", "q2 report.pdf"]),
          "typing in the search box searches as you type")
    key("ctrl+a")
    type_text("שלום")
    check(wait(lambda: names() == ["שָׁלוֹם.txt"]), "typing Hebrew in the search box")
    key("Escape")
    check(wait(lambda: total() > 5000), "Escape clears the search and shows everything")

    type_text("REPORT")
    wait(lambda: total() == 2)
    key("ctrl+i")
    check(wait(lambda: total() == 0), "Ctrl+I toggles Match Case")
    key("ctrl+i")
    check(wait(lambda: total() == 2), "Ctrl+I again turns it off")

    key("ctrl+a")
    type_text("notes.md")
    wait(lambda: names() == ["notes.md"])
    notes = os.path.join(disk, "docs", "notes.md")
    before = run_count(notes)
    key("Return")
    check(wait(lambda: run_count(notes) == before + 1),
          "Enter in the search box opens the first result")
    if wait(lambda: dialog_labels() is not None, timeout=5):
        close_dialog()

    # The default view, so every header is on screen.
    remote("-no-preview", "-columns", "name;path;size;date-modified", "-s", "ext:pdf")
    wait(lambda: names() == ["q1 report.pdf", "q2 report.pdf"])
    size = header_cell("Size")
    if check(size is not None, "the Size column header is there"):
        click_node(size)
        check(wait(lambda: names() == ["q2 report.pdf", "q1 report.pdf"]),
              "clicking the Size header sorts by size")
        click_node(header_cell("Size"))
        check(wait(lambda: names() == ["q1 report.pdf", "q2 report.pdf"]),
              "clicking it again reverses the order")
        remote("-sort", "name")
        wait(lambda: names() == ["q1 report.pdf", "q2 report.pdf"])

    click_row(0)
    click_row(1, mods=["ctrl"])
    check(wait(lambda: "2 selected" in count_text()), "Ctrl+click selects several results")
    key("ctrl+c")
    uris = wait(lambda: clipboard("text/uri-list"))
    check(bool(uris) and "q1%20report.pdf" in uris and "q2%20report.pdf" in uris,
          "Ctrl+C puts the selected files on the clipboard")

    click_row(0, button=3)
    item = wait(lambda: menu_item("Copy Full Path"), timeout=5)
    items = [accessible_name(n) for n in find_all("menu item")]
    if check(item is not None, f"a right click opens the result menu {items}"):
        pressed = press(item)
        check(wait(lambda: clipboard() == f"{q1}\n{q2}"),
              f"Copy Full Path copies the paths of the selection "
              f"[pressed={pressed} clipboard={clipboard()!r}]")
    if find_all("menu item"):
        xdo("key", "Escape")
    wait(lambda: not find_all("menu item"), timeout=5)

    remote("-sort", "name")
    search("bulk-")
    check(wait(lambda: total() == 5000), "5,000 results")
    click_row(0)
    key("End")
    check(wait(lambda: "bulk-04999.dat" in names()),
          f"End jumps to the last of 5,000 results (rows load as they scroll in) {names()[:3]}")
    key("Home")
    check(wait(lambda: "bulk-00000.dat" in names()), "Home jumps back to the first")

    search("ext:md")
    wait(lambda: names() == ["notes.md"])
    click_node(search_entry())
    key("ctrl+d")
    if check(wait(lambda: dialog_labels() is not None, timeout=5), "Ctrl+D asks for a bookmark name"):
        key("ctrl+a")
        type_text("Markdown files")
        key("Return")
        check(wait(lambda: "name=Markdown files" in open(settings_file, encoding="utf-8").read()),
              "the bookmark is saved under the typed name")

    check(wait(lambda: any(re.match(r"^\d+ volumes? up to date", n.get_name() or "")
                           for n in find_all("label"))),
          "the status bar reports the volumes as up to date")


def form_field(title):
    """An entry of an open form, or the row title next to it (clicking
    either focuses the entry)."""
    def find():
        for d in dialogs():
            hit = find_named(title, ("entry", "text", "password text"), root=d) or \
                find_named(title, ("label",), root=d)
            if hit:
                return hit
        return None
    return wait(find)


def fill(title, text):
    node = form_field(title)
    if node is None:
        return False
    click_node(node)
    type_text(text)
    return True


def open_mounts_page():
    app_action("preferences")
    tab = wait(lambda: find_named("Mounts", ("toggle button", "push button", "page tab")))
    if not check(tab is not None, "Preferences has a Mounts page"):
        close_dialog()
        return False
    press(tab)
    return True


def open_mount_form():
    if not open_mounts_page():
        return False
    add = wait(lambda: find_named("Add Mount", ("push button",)))
    if not check(add is not None, "the Mounts page has an Add button"):
        close_dialog()
        return False
    press(add)
    if not check(form_field("Name") is not None, "the Add Mount form opens"):
        close_dialog()
        return False
    return True


def remove_button_for(mount):
    """The Remove button in the row of a mount (the nearest one around
    its title)."""
    node = find_named(mount, ("label",))
    for _ in range(6):
        node = node.get_parent() if node is not None else None
        if node is None:
            return None
        for n in walk(node):
            if n.get_role_name() == "push button" and accessible_name(n) == "Remove":
                return n
    return None


def remove_mount(mount):
    """Removes a mount from the Preferences list, confirming the question."""
    remove = wait(lambda: remove_button_for(mount))
    if not check(remove is not None, f"{mount} can be removed from the list"):
        return False
    press(remove)

    def confirmation():
        for d in reversed(dialogs()):
            if find_named("Cancel", ("push button",), root=d):
                return find_named("Remove", ("push button",), root=d)
        return None
    confirm = wait(confirmation, timeout=5)
    if check(confirm is not None, "removing asks for confirmation"):
        press(confirm)
    return confirm is not None


def evos_count(query):
    out = subprocess.run(["evos", "--count", query], capture_output=True, text=True)
    return int(out.stdout.strip() or 0)


def mounts_in_preferences(disk):
    """Mounting a folder from Preferences makes its files searchable."""
    source = "/tmp/e2e-bind-source"
    os.makedirs(os.path.join(source, "inner"), exist_ok=True)
    open(os.path.join(source, "inner", "bound-file.txt"), "w").close()

    app_action("preferences")
    check(wait(lambda: disk in (dialog_labels() or [])), "Preferences lists the indexed volumes")
    close_dialog()

    if not open_mount_form():
        return
    fill("Name", "e2e-folder")
    kind = wait(lambda: find_named("Type", ("combo box",)))
    click_node(kind or form_field("Type"))
    time.sleep(0.5)
    # SMB, NFS, SSHFS, then "Local folder (bind mount)".
    key("Down", "Down", "Down", "Return")
    check(fill("Source, e.g. /path/to/folder", source), "choosing the folder type in the form")
    press(find_named("Add", ("push button",)))
    check(wait(lambda: evos_count("bound-file.txt") == 1, timeout=30),
          "a folder mounted from Preferences gets indexed")
    check(wait(lambda: find_all(name="e2e-folder")), "the new mount is listed")
    search("bound-file")
    check(wait(lambda: [r for r in rows() if r[0] == "bound-file.txt"
                        and r[1] == "/mnt/evos/e2e-folder/inner"]),
          "its files show up in the window")
    if remove_mount("e2e-folder"):
        check(wait(lambda: evos_count("bound-file.txt") == 0, timeout=30),
              "removing it unmounts the folder and its files leave the index")
    close_dialog()


def share_mount(source, user, password, name):
    """Mounting an SMB share from Preferences, with a login."""
    if not open_mount_form():
        return
    fill("Name", "gui-smb")
    # SMB is the first type.
    fill("Source, e.g. //server/share", source)
    fill("User name", user)
    fill("Password", password)
    press(find_named("Add", ("push button",)))
    root = "/mnt/evos/gui-smb"
    check(wait(lambda: evos_count(f"under:{root} \"{name}\"") == 1, timeout=180),
          "an SMB share mounted from Preferences gets indexed")
    close_dialog()
    remote("-filter", "everything", "-s", name)
    check(wait(lambda: [r for r in rows() if r[0] == name and r[1].startswith(root)], timeout=30),
          f"{name} from that share shows up in the window")
    if open_mounts_page() and remove_mount("gui-smb"):
        check(wait(lambda: evos_count(f"under:{root}") == 0, timeout=60),
              "removing the share unmounts it and its files leave the index")
    close_dialog()


def service_restart():
    """The window survives the service restarting, as after an upgrade."""
    config = os.environ.get("E2E_EVOSD_CONFIG")
    if not config:
        return
    search("report")
    wait(lambda: total() == 2)
    subprocess.run(["pkill", "-x", "evosd"])
    check(wait(banner_showing), "the window says when the service is gone")
    # It saves its index on the way out; a new one waits for the socket.
    wait(lambda: subprocess.run(["pgrep", "-x", "evosd"], capture_output=True).returncode != 0,
         timeout=30)
    log = open(os.environ.get("E2E_EVOSD_LOG", "/dev/null"), "a")
    subprocess.Popen(["evosd", "--config", config], stdout=log, stderr=log,
                     start_new_session=True)
    check(wait(lambda: not banner_showing() and total() == 2, timeout=30),
          "the window reconnects when the service is back")


def share(root, name):
    remote("-filter", "everything", "-s", name)
    found = wait(lambda: [r for r in rows() if r[0] == name], timeout=60)
    check(bool(found) and found[0][1].startswith(root), f"{name} on the share shows up in the window")


def main():
    if not wait(app, 30):
        check(False, "evos-gtk appears on the accessibility bus")
        return 1
    mode = sys.argv[1]
    if mode == "local":
        guarded(local, sys.argv[2], sys.argv[3])
    elif mode == "share":
        guarded(share, sys.argv[2], sys.argv[3])
    elif mode == "share-mount":
        guarded(share_mount, *sys.argv[2:6])
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
