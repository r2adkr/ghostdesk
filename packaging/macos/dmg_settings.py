# dmgbuild settings for the first-install disk image (.github/workflows/release.yml).
#
#   dmgbuild -s packaging/macos/dmg_settings.py -D app=path/to/GhostDesk.app GhostDesk out.dmg
#
# The window shows GhostDesk.app on the left and an Applications shortcut on the right, over the
# background drawn by make-art.swift (an arrow between them and one line of instructions).
# Dragging it into Applications or just opening it both end with the app in
# ~/Applications/GhostDesk.app (src/install.rs take_over_install).
import os.path

app = defines.get("app", "GhostDesk.app")  # noqa: F821 (dmgbuild provides `defines`)
here = defines.get("art", "packaging/macos")  # noqa: F821 (run from the repository root)

format = "UDZO"
filesystem = "APFS"
files = [app]
symlinks = {"Applications": "/Applications"}
icon = os.path.join(here, "AppIcon.icns")
badge_icon = None

background = os.path.join(here, "dmg-background.tiff")
window_rect = ((200, 160), (640, 400))
default_view = "icon-view"
show_status_bar = False
show_tab_view = False
show_toolbar = False
show_pathbar = False
show_sidebar = False
icon_size = 112
text_size = 13
arrange_by = None
icon_locations = {
    os.path.basename(app): (180, 160),
    "Applications": (460, 160),
}
