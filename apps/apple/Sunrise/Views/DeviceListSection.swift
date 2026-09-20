import SwiftUI

/// The device list, and the three security signals that belong on it.
///
/// A `Section` rather than a screen of its own, because it goes in the Settings
/// form both shells already present — `VaultWindow`'s sheet on the Mac,
/// `VaultTabs`'s on the phone — so one file reaches both clients. It sits
/// directly above the Vaults section, which is where "Add a device…" lives:
/// the surface that adds devices and the surface that lists them are the same
/// question asked twice.
///
/// Nothing here is phrased as an accusation, and `DeviceListModel` explains why
/// for each of the three marks a row can carry. A device list that cries wolf
/// about an ordinary pairing is a device list a user learns to close.
struct DeviceListSection: View {
    @Bindable var model: DeviceListModel

    @State private var revoking: DeviceListModel.DeviceRow?

    var body: some View {
        Section("Devices") {
            ForEach(model.rows) { row in
                deviceRow(row)
            }
            if model.rows.isEmpty {
                Text("No devices yet.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            identityKeyDisclosure
            revocationDisclosure
            if let error = model.errorMessage {
                Label(error, systemImage: "exclamationmark.triangle")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .accessibilityIdentifier("devices.error")
            }
        }
        .task { await model.refresh() }
        .task { await model.follow() }
        .confirmationDialog(
            "Remove \(revoking?.nickname ?? "this device")?",
            isPresented: Binding(get: { revoking != nil }, set: { if !$0 { revoking = nil } }),
            titleVisibility: .visible
        ) {
            // The four reasons the core takes, offered rather than defaulted.
            // `Command::RevokeDevice` records which one, and "stolen" and
            // "retired" are the same operation with very different meanings to
            // whoever reads the register later.
            ForEach(RevokeReasonChoice.allCases) { choice in
                Button(choice.title) {
                    if let row = revoking { Task { await model.revoke(row, reason: choice.reason) } }
                    revoking = nil
                }
            }
            Button("Cancel", role: .cancel) { revoking = nil }
        } message: {
            Text(
                """
                Every Stream key is rotated, so this device reads nothing \
                written afterwards. It can still read what it already has — \
                revocation is forward-only.
                """
            )
        }
    }

    @ViewBuilder
    private func deviceRow(_ row: DeviceListModel.DeviceRow) -> some View {
        VStack(alignment: .leading, spacing: 2) {
            HStack {
                Text(row.nickname.isEmpty ? "Unnamed device" : row.nickname)
                Spacer()
                Text(row.platform)
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            Text(String(row.deviceID.prefix(16)))
                .font(.caption2)
                .monospaced()
                .foregroundStyle(.secondary)
            ForEach(marks(row), id: \.self) { mark in
                Text(mark)
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        }
        .accessibilityIdentifier("devices.row")
        .swipeActions(edge: .trailing) {
            if !row.isThisDevice && !row.revoked {
                Button("Remove", role: .destructive) { revoking = row }
                    .accessibilityIdentifier("devices.revoke")
            }
        }
        .contextMenu {
            if !row.isThisDevice && !row.revoked {
                Button("Remove…", role: .destructive) { revoking = row }
            }
        }
    }

    /// The marks a row carries, in the order they matter.
    ///
    /// Prose rather than badges, because each one needs its consequence said in
    /// the same breath. `current` and `admittedAfterRevocation` are both states
    /// an honest device reaches, and a red dot cannot say so.
    private func marks(_ row: DeviceListModel.DeviceRow) -> [String] {
        var out: [String] = []
        if row.isThisDevice { out.append("This \(Platform.deviceName).") }
        if row.revoked { out.append("Removed. It reads nothing written since.") }
        if !row.revoked, row.readBounded {
            // The two removal facts have come apart. Since ADR-0041 the
            // register is a fold, so a removal stops being recorded once the
            // ledger shows its own author was removed first, while the key
            // bound never comes back. Without this line the row looks like any
            // other member while receiving nothing at all — the one outcome of
            // that design a user could be surprised by. Worded as a state with
            // its remedy, for the same reason the two marks below are.
            out.append(
                """
                Removed earlier. The account no longer records that, so it \
                shows as current — but it still receives no keys and reads \
                nothing written since. Remove it again from a device you \
                still trust.
                """
            )
        }
        if !row.current {
            out.append(
                """
                Not active on this account — the identity that certified it is \
                no longer the one in force. An honest device that has not \
                caught up with a change yet looks the same.
                """
            )
        }
        if row.admittedAfterRevocation {
            out.append(
                """
                Joined after a device was removed. Usually that is just a \
                device you added later; it is worth a look if it is not.
                """
            )
        }
        return out
    }

    /// #144's first signal. Shown only where it is true — a device that does
    /// not hold the key has nothing to be careful about, and a warning printed
    /// on every account is a warning nobody reads.
    @ViewBuilder
    private var identityKeyDisclosure: some View {
        if model.holdsIdentityKey {
            Text(
                """
                This \(Platform.deviceName) holds the account identity key. \
                Your recovery code is its only other copy: without one, losing \
                this device destroys the key permanently, and no recovery \
                feature added later can retrieve it.
                """
            )
            .font(.caption)
            .foregroundStyle(.secondary)
            .accessibilityIdentifier("devices.identityKey")
        }
    }

    /// #144's third signal, and #160's disclosure beside it: what the last
    /// revocation did **not** achieve.
    ///
    /// The gated case replaces the whole report rather than appending to it.
    /// Nothing was removed, so "Removed X" is the one sentence that must not
    /// appear, and the rotation and relay lines below would all be answers to
    /// a question the user no longer has.
    @ViewBuilder
    private var revocationDisclosure: some View {
        if let done = model.lastRevocation, done.gated {
            VStack(alignment: .leading, spacing: 4) {
                Text("\(done.nickname) was NOT removed.")
                Text(
                    """
                    This \(Platform.deviceName) has itself been removed from \
                    the account, so the account discards its removals of other \
                    devices. \(done.nickname) is still current everywhere and \
                    still receives new keys, and this removal tells the relay nothing.
                    """
                )
                Text(
                    """
                    Remove it from a device the account still trusts. The \
                    request is kept, not discarded, and is reconsidered \
                    whenever another removal arrives.
                    """
                )
                unwoundNotice(done)
                Button("Done") { model.dismissRevocation() }
            }
            .font(.caption)
            .accessibilityIdentifier("devices.revocationGated")
        } else if let done = model.lastRevocation {
            VStack(alignment: .leading, spacing: 4) {
                Text("Removed \(done.nickname) from this account.")
                if done.unrotatedStreams.isEmpty {
                    Text("Every Stream key was rotated, and the account identity with it.")
                } else {
                    Text(
                        """
                        The account identity rotated, and every Stream key but \
                        \(done.unrotatedStreams.count). That device may still \
                        read those. This is a damaged row in this vault's \
                        storage, not something the removal can retry.
                        """
                    )
                    ForEach(done.unrotatedStreams, id: \.self) { id in
                        Text(id).monospaced()
                    }
                }
                Text(
                    done.relayPending
                        ? """
                        The relay has NOT been told yet. That is queued and \
                        goes out on the next sync.
                        """
                        : "The relay has been told."
                )
                unwoundNotice(done)
                Button("Done") { model.dismissRevocation() }
            }
            .font(.caption)
            .accessibilityIdentifier("devices.revocationResult")
        }
    }

    /// What this removal did to the account's record of **other** removals.
    ///
    /// Shown on both branches, because it is not about the device the user
    /// acted on. Applying any removal re-folds the whole register, and that can
    /// discard an older removal whose own author the ledger removes — the
    /// device then goes back to reading as an ordinary member on the list above
    /// while still receiving nothing. Silent when the set is empty, which is
    /// almost always.
    @ViewBuilder
    private func unwoundNotice(_ done: DeviceListModel.Revocation) -> some View {
        if !done.unwound.isEmpty {
            Text(
                """
                The account no longer records a removal of \
                \(done.unwound.count) other device(s), because the device \
                that removed them was itself removed. They are not back in — \
                they receive no keys and read nothing written since — but the \
                account does not say they were removed. Remove each of them \
                again from a device you still trust.
                """
            )
            ForEach(done.unwound, id: \.self) { id in
                Text(id).monospaced()
            }
        }
    }
}

/// The four reasons, with the words a user would use.
///
/// A local enum rather than rendering `DeviceRevokeReason` directly: the seam's
/// case names are the register's vocabulary, and "retired" on a button is less
/// clear than "No longer using it".
private enum RevokeReasonChoice: String, CaseIterable, Identifiable {
    case lost, stolen, retired, compromised

    var id: String { rawValue }

    var title: String {
        switch self {
        case .lost: "I lost it"
        case .stolen: "It was stolen"
        case .retired: "No longer using it"
        case .compromised: "Someone else got into it"
        }
    }

    var reason: DeviceRevokeReason {
        switch self {
        case .lost: .lost
        case .stolen: .stolen
        case .retired: .retired
        case .compromised: .compromised
        }
    }
}
