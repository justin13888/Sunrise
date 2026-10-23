#!/usr/bin/env python3
"""Fail when Swift under apps/apple gains logic that is not a view's.

Why this gate exists
--------------------

`CLAUDE.md` asks for core logic isolated where it can be tested
deterministically, and `docs/01-architecture/shared-core.md` §The client
layer says where: `sunrise-client-core`, which every client links. The Apple
app predates that crate, so much of what it decides — which badge sync shows,
what a `sunrise://` link means, how a pairing walks its legs — was written in
Swift, where the CLI and the web client cannot reach it and Linux CI cannot
test it. That logic is moving down, file by file. Nothing stopped new logic
landing in Swift meanwhile, and a migration with no ratchet runs both ways.

So: every Swift source file under apps/apple that holds anything but views is
listed in ALLOWED below, with what keeps it there. A file that gains logic and
is not listed fails the gate; so does a listed file that no longer holds any,
or no longer exists — the list may only shrink, and an entry nobody is forced
to prune is a slower way of deleting the check.

What counts as logic
--------------------

The check reads top-level declarations, the column-0 `struct`, `class`,
`enum`, `actor`, `protocol`, `extension`, `func`, `let` and `var` lines, after
any attributes and modifiers. A declaration is a **view's** when its
inheritance clause names one of VIEW_PROTOCOLS, or it extends a type some
file declared that way, or it extends `View` itself. Anything else at the top
level — a model, a value type, a free function, an extension of a model — is
logic. It is a heuristic, deliberately coarse and deterministic: a file is
either all views or it is on the list, and the list says why.

Tests (SunriseTests, the UI test bundles and UITestSupport) are not app logic
and are not scanned.

Usage: swift-logic-gate.py [--root PATH] [--list]
`--list` prints every file the heuristic says holds logic, with the first
declaration that made it so, for maintaining ALLOWED. Exit 0 clean, 1 on a violation, a stale entry, or a tree with no
Swift to read (a gate that measured nothing is red, not green).

The routes above are asserted in `test_swift_logic_gate.py` beside this file.
"""

from __future__ import annotations

import argparse
import pathlib
import re
import sys

SCANNED = "apps/apple"
SKIPPED_DIRS = frozenset({"SunriseTests", "SunriseUITests", "SunriseiOSUITests", "UITestSupport"})

# A type that conforms to one of these is presentation, whatever else it does.
VIEW_PROTOCOLS = frozenset(
    {
        "App",
        "ButtonStyle",
        "Commands",
        "LabelStyle",
        "NSViewControllerRepresentable",
        "NSViewRepresentable",
        "PreviewProvider",
        "PrimitiveButtonStyle",
        "Scene",
        "Shape",
        "TextFieldStyle",
        "ToggleStyle",
        "UIViewControllerRepresentable",
        "UIViewRepresentable",
        "View",
        "ViewModifier",
        "Widget",
        "WidgetBundle",
    }
)

# Why each file still holds logic. Every entry is one of these, so the list
# reads as a plan: `moving(...)` names the client-core API that replaces the
# file's decisions in the Mac pass; the others say why the logic is the
# platform's own, or is next in line.
PLATFORM = "platform IO the core cannot do: Keychain, App Group, notifications, URLSession, widgets, intents"
SEAM = "a model over CoreBridge; what it still decides in Swift is a candidate for the next move"
SHELL = "app and window wiring: routing, surfaces, menus, keyboard dispatch"
PRESENTATION = "a view file's own value types: choices, copy and helpers that shape one screen"


def moving(api: str) -> str:
    """A file whose decisions `sunrise_client_core::<api>` now makes."""
    return f"moving to sunrise_client_core::{api} in the Mac pass"


ALLOWED: dict[str, str] = {
    "apps/apple/Share/Extension/ShareReceiver.swift": PLATFORM,
    "apps/apple/Share/Extension/ShareViewController.swift": PLATFORM,
    "apps/apple/Share/Shared/PendingCapture.swift": PLATFORM,
    "apps/apple/Sunrise/App/AppSurfaces.swift": SHELL,
    "apps/apple/Sunrise/App/VaultModels.swift": SHELL,
    "apps/apple/Sunrise/App/WidgetPublisher.swift": PLATFORM,
    "apps/apple/Sunrise/Browse/BrowseModel.swift": SEAM,
    "apps/apple/Sunrise/Browse/Destination.swift": SHELL,
    "apps/apple/Sunrise/Browse/StreamOrder.swift": SEAM,
    "apps/apple/Sunrise/Calendar/BlockLayout.swift": SEAM,
    "apps/apple/Sunrise/Calendar/CalendarModel.swift": SEAM,
    "apps/apple/Sunrise/Core/ChangeFeed.swift": SEAM,
    "apps/apple/Sunrise/Core/CoreBridge+Attachments.swift": SEAM,
    "apps/apple/Sunrise/Core/CoreBridge.swift": SEAM,
    "apps/apple/Sunrise/Core/TimeZoneWatch.swift": PLATFORM,
    "apps/apple/Sunrise/Design/Tokens.swift": PLATFORM,
    "apps/apple/Sunrise/Focus/FocusActivityPlan.swift": SEAM,
    "apps/apple/Sunrise/Focus/FocusFilter.swift": SEAM,
    "apps/apple/Sunrise/Focus/FocusModel.swift": SEAM,
    "apps/apple/Sunrise/Ical/AppSurfaces+Ical.swift": SHELL,
    "apps/apple/Sunrise/Ical/IcalFiles.swift": PLATFORM,
    "apps/apple/Sunrise/Ical/IcalModel.swift": SEAM,
    "apps/apple/Sunrise/Identity/AccountModel.swift": moving("account_session::AccountSession"),
    "apps/apple/Sunrise/Identity/AccountModelFirstLook.swift": PLATFORM,
    "apps/apple/Sunrise/Identity/AccountModelRenewalTick.swift": moving("account_session::AccountSession::renewal_due"),
    "apps/apple/Sunrise/Identity/AccountModelSignOutDisclosure.swift": moving("account_session::SignOutResidue"),
    "apps/apple/Sunrise/Identity/AppSettings.swift": PLATFORM,
    "apps/apple/Sunrise/Identity/CredentialStore.swift": PLATFORM,
    "apps/apple/Sunrise/Identity/DeviceListModel.swift": SEAM,
    "apps/apple/Sunrise/Identity/Keychain.swift": PLATFORM,
    "apps/apple/Sunrise/Identity/KeychainDomain.swift": PLATFORM,
    "apps/apple/Sunrise/Identity/KeychainError.swift": PLATFORM,
    "apps/apple/Sunrise/Identity/KeychainMigration.swift": PLATFORM,
    "apps/apple/Sunrise/Identity/RecoveryCodeModel.swift": SEAM,
    "apps/apple/Sunrise/Identity/RelayDeviceStore.swift": PLATFORM,
    "apps/apple/Sunrise/Identity/SessionModel.swift": moving("vault_session::VaultSession"),
    "apps/apple/Sunrise/Identity/SessionModelRelayDevice.swift": PLATFORM,
    "apps/apple/Sunrise/Identity/SessionModelRenewal.swift": PLATFORM,
    "apps/apple/Sunrise/Identity/SessionModelRestore.swift": PLATFORM,
    "apps/apple/Sunrise/Identity/UITestHarness.swift": PLATFORM,
    "apps/apple/Sunrise/Identity/VaultLocation.swift": PLATFORM,
    "apps/apple/Sunrise/Identity/VaultRegistry.swift": PLATFORM,
    "apps/apple/Sunrise/Identity/VaultRootStore.swift": PLATFORM,
    "apps/apple/Sunrise/Intents/CaptureTaskIntent.swift": PLATFORM,
    "apps/apple/Sunrise/Intents/CompleteTaskIntent.swift": PLATFORM,
    "apps/apple/Sunrise/Intents/DeferTaskIntent.swift": PLATFORM,
    "apps/apple/Sunrise/Intents/FocusIntents.swift": PLATFORM,
    "apps/apple/Sunrise/Intents/IntentError.swift": PLATFORM,
    "apps/apple/Sunrise/Intents/IntentVault.swift": PLATFORM,
    "apps/apple/Sunrise/Intents/SetFocusFilterIntent.swift": PLATFORM,
    "apps/apple/Sunrise/Intents/StreamEntity.swift": PLATFORM,
    "apps/apple/Sunrise/Intents/StreamSummaryIntent.swift": PLATFORM,
    "apps/apple/Sunrise/Intents/SunriseShortcuts.swift": PLATFORM,
    "apps/apple/Sunrise/Intents/TaskEntity.swift": PLATFORM,
    "apps/apple/Sunrise/Intents/TaskListIntents.swift": PLATFORM,
    "apps/apple/Sunrise/Keyboard/CheatSheet.swift": SHELL,
    "apps/apple/Sunrise/Keyboard/CommandPalette.swift": moving("palette::{palette_rank, palette_highlight}"),
    "apps/apple/Sunrise/Keyboard/KeyChord.swift": SHELL,
    "apps/apple/Sunrise/Keyboard/KeyboardCommands.swift": SHELL,
    "apps/apple/Sunrise/Keyboard/Keymap.swift": SHELL,
    "apps/apple/Sunrise/Keyboard/ListCommands.swift": SHELL,
    "apps/apple/Sunrise/Keyboard/ListSelection.swift": moving("selection::ListSelection"),
    "apps/apple/Sunrise/Keyboard/VimMode.swift": SHELL,
    "apps/apple/Sunrise/Menu/MenuBarModel.swift": SHELL,
    "apps/apple/Sunrise/Notes/NoteEditorBlock.swift": moving("note_blocks::EditorBlock"),
    "apps/apple/Sunrise/Notes/NoteEditorModel.swift": moving("note_blocks::{edit_lock, set_kind, indent_row, outdent_row}"),
    "apps/apple/Sunrise/Notes/NoteInlineText.swift": SEAM,
    "apps/apple/Sunrise/Notifications/DailyBrief.swift": SEAM,
    "apps/apple/Sunrise/Notifications/DeepLink.swift": moving("links::DeepLink"),
    "apps/apple/Sunrise/Notifications/NotificationAuthorization.swift": PLATFORM,
    "apps/apple/Sunrise/Notifications/NotificationCenterClient.swift": PLATFORM,
    "apps/apple/Sunrise/Notifications/NotificationPreferences.swift": SEAM,
    "apps/apple/Sunrise/Notifications/ReminderPlan.swift": SEAM,
    "apps/apple/Sunrise/Notifications/ReminderScheduler.swift": PLATFORM,
    "apps/apple/Sunrise/Pairing/PairingCopy.swift": SEAM,
    "apps/apple/Sunrise/Pairing/PairingModel.swift": moving("pairing_flow::PairingFlow"),
    "apps/apple/Sunrise/Pairing/PairingRelay.swift": moving("pairing_flow::PairingFlow (the health probe stays)"),
    "apps/apple/Sunrise/Pairing/QRCode.swift": PLATFORM,
    "apps/apple/Sunrise/Pairing/QRScanner.swift": PLATFORM,
    "apps/apple/Sunrise/Platform/PlatformKit.swift": PLATFORM,
    "apps/apple/Sunrise/Print/PrintDocument.swift": moving("print::PrintDocument::paginate"),
    "apps/apple/Sunrise/Print/PrintPage.swift": PLATFORM,
    "apps/apple/Sunrise/Review/ReviewModel.swift": SEAM,
    "apps/apple/Sunrise/Routines/RoutineModel.swift": SEAM,
    "apps/apple/Sunrise/Sync/BackgroundSync.swift": PLATFORM,
    "apps/apple/Sunrise/Sync/EditGateModel.swift": SEAM,
    "apps/apple/Sunrise/Sync/ParkedOpsModel.swift": SEAM,
    "apps/apple/Sunrise/Sync/PushTokenRegistrar.swift": PLATFORM,
    "apps/apple/Sunrise/Sync/SessionModelBackground.swift": PLATFORM,
    "apps/apple/Sunrise/Sync/SyncPlan.swift": moving("sync_status::plan_sync, relay_url::parse_relay_url"),
    "apps/apple/Sunrise/Sync/SyncPresentation.swift": moving("sync_status::sync_badge"),
    "apps/apple/Sunrise/Sync/SyncStatusModel.swift": SEAM,
    "apps/apple/Sunrise/Tasks/ActivityModel.swift": SEAM,
    "apps/apple/Sunrise/Tasks/AttachmentCacheModel.swift": PLATFORM,
    "apps/apple/Sunrise/Tasks/AttachmentsModel.swift": SEAM,
    "apps/apple/Sunrise/Tasks/CaptureModel.swift": SEAM,
    "apps/apple/Sunrise/Tasks/ListOrder.swift": SEAM,
    "apps/apple/Sunrise/Tasks/NameBook.swift": SEAM,
    "apps/apple/Sunrise/Tasks/SearchModel.swift": SEAM,
    "apps/apple/Sunrise/Tasks/SharedCapture.swift": moving("share::share_draft (the filing stays)"),
    "apps/apple/Sunrise/Tasks/TaskFacets.swift": SEAM,
    "apps/apple/Sunrise/Tasks/TaskListModel.swift": SEAM,
    "apps/apple/Sunrise/Tasks/ThumbnailRenderer.swift": PLATFORM,
    "apps/apple/Sunrise/Undo/SavedViewsModel.swift": SEAM,
    "apps/apple/Sunrise/Undo/UndoModel.swift": SEAM,
    "apps/apple/Sunrise/Views/AccountView.swift": PRESENTATION,
    "apps/apple/Sunrise/Views/AttachmentsView.swift": PRESENTATION,
    "apps/apple/Sunrise/Views/BlockEditorView.swift": PRESENTATION,
    "apps/apple/Sunrise/Views/BrowseSidebar.swift": PRESENTATION,
    "apps/apple/Sunrise/Views/CalendarView.swift": PRESENTATION,
    "apps/apple/Sunrise/Views/DeviceListSection.swift": PRESENTATION,
    "apps/apple/Sunrise/Views/KeyboardTips.swift": PRESENTATION,
    "apps/apple/Sunrise/Views/OnboardingView.swift": PRESENTATION,
    "apps/apple/Sunrise/Views/PairingView.swift": PRESENTATION,
    "apps/apple/Sunrise/Views/RecoveryCodeView.swift": PRESENTATION,
    "apps/apple/Sunrise/Views/RestoreFromCodeView.swift": PRESENTATION,
    "apps/apple/Sunrise/Views/ReviewView.swift": PRESENTATION,
    "apps/apple/Sunrise/Views/RoutineEditorView.swift": PRESENTATION,
    "apps/apple/Sunrise/Views/RoutinesView.swift": PRESENTATION,
    "apps/apple/Sunrise/Views/TaskEditorView.swift": PRESENTATION,
    "apps/apple/Sunrise/Views/TaskListView.swift": PRESENTATION,
    "apps/apple/Widgets/Extension/NextUpProvider.swift": PLATFORM,
    "apps/apple/Widgets/Extension/NextUpWidget.swift": PLATFORM,
    "apps/apple/Widgets/Shared/FocusActivity.swift": PLATFORM,
    "apps/apple/Widgets/Shared/WidgetSnapshot.swift": PLATFORM,
    "apps/apple/iOS/BackgroundHost.swift": PLATFORM,
    "apps/apple/iOS/FocusLiveActivity.swift": PLATFORM,
    "apps/apple/iOS/IcalDocuments.swift": PLATFORM,
    "apps/apple/iOS/PrintController.swift": PLATFORM,
    "apps/apple/iOS/ShareInbox.swift": PLATFORM,
    "apps/apple/iOS/TabRoute.swift": PLATFORM,
    "apps/apple/macOS/HotkeyCenter.swift": PLATFORM,
    "apps/apple/macOS/IcalPanels.swift": PLATFORM,
    "apps/apple/macOS/PrintJob.swift": PLATFORM,
    "apps/apple/macOS/QuickCapturePanel.swift": PLATFORM,
    "apps/apple/macOS/SoftwareUpdate.swift": PLATFORM,
    "apps/apple/macOS/SunriseMacApp.swift": PLATFORM,
}

DECL = re.compile(
    r"^(?:@[\w.]+(?:\([^)]*\))?\s+)*"
    r"(?:(?:public|private|fileprivate|internal|open|final|nonisolated|indirect|static)\s+)*"
    r"(struct|class|enum|actor|protocol|extension|func|let|var)\s+([A-Za-z_][\w.]*)"
    r"([^{]*)"
)


def swift_files(root: pathlib.Path) -> list[pathlib.Path]:
    base = root / SCANNED
    return sorted(
        p
        for p in base.rglob("*.swift")
        if not SKIPPED_DIRS.intersection(p.relative_to(base).parts)
    )


def declarations(text: str) -> list[tuple[str, str, str]]:
    """(keyword, name, inheritance clause) for each top-level declaration."""
    found = []
    pending_attributes = ""
    for line in text.splitlines():
        if not line or line[0].isspace() or line.startswith(("//", "/*", "*", "}", "#", "import ")):
            pending_attributes = ""
            continue
        if line.startswith("@") and DECL.match(line) is None:
            # An attribute on its own line belongs to the declaration below.
            pending_attributes += line + " "
            continue
        match = DECL.match(pending_attributes + line)
        pending_attributes = ""
        if match:
            keyword, name, rest = match.groups()
            clause = rest.split(" where ", 1)[0]
            found.append((keyword, name, clause))
    return found


def conforms_to_view(clause: str) -> bool:
    if ":" not in clause:
        return False
    names = re.findall(r"[A-Za-z_][\w.]*", clause.split(":", 1)[1])
    return any(n.split(".")[-1] in VIEW_PROTOCOLS for n in names)


def logic_files(root: pathlib.Path) -> tuple[dict[str, str], int]:
    """Each file that holds logic, with the first declaration that is not a
    view's; and how many files were scanned."""
    files = swift_files(root)
    parsed = {p: declarations(p.read_text(encoding="utf-8")) for p in files}
    view_types = {
        name
        for decls in parsed.values()
        for keyword, name, clause in decls
        if keyword in {"struct", "class", "enum"} and conforms_to_view(clause)
    }
    holding = {}
    for path, decls in parsed.items():
        for keyword, name, clause in decls:
            if keyword in {"struct", "class", "enum"} and conforms_to_view(clause):
                continue
            if keyword == "extension" and (name in view_types or name == "View" or conforms_to_view(clause)):
                continue
            holding[path.relative_to(root).as_posix()] = f"{keyword} {name}"
            break
    return dict(sorted(holding.items())), len(files)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--root", default=".")
    ap.add_argument("--list", action="store_true")
    args = ap.parse_args()
    root = pathlib.Path(args.root).resolve()

    holding, scanned = logic_files(root)
    if scanned == 0:
        raise SystemExit(f"::error::swift-logic: no Swift under {SCANNED}; the gate could not run.")
    if args.list:
        for path, first in holding.items():
            print(f"{path}\t{first}")
        return 0

    failed = False
    for path in sorted(set(ALLOWED) - set(holding)):
        exists = (root / path).exists()
        why = "holds no logic now" if exists else "no longer exists"
        print(f"::error file={path}::swift-logic: `{path}` is allowed to hold logic but {why}; remove it from ALLOWED.")
        failed = True
    for path in holding:
        if path not in ALLOWED:
            print(
                f"::error file={path}::swift-logic: `{path}` holds logic that is not a view's. "
                "Put the decision in sunrise-client-core and call it over UniFFI; "
                "if it is the platform's own, add the file to ALLOWED with the reason."
            )
            failed = True

    print(f"swift-logic: {scanned} Swift file(s) scanned; {len(holding)} hold logic; {len(ALLOWED)} allowed.")
    if failed:
        return 1
    print("OK: swift-logic clean.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
