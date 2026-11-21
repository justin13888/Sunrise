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
///
/// Every word on it is `L10n.Devices`, compiled from `i18n/en.toml`'s
/// `[apple.devices]` table: the first view read from the string catalog.
struct DeviceListSection: View {
    @Bindable var model: DeviceListModel

    @State private var revoking: DeviceListModel.DeviceRow?
    @State private var withdrawing: DeviceListModel.DeviceRow?

    var body: some View {
        Section(L10n.Devices.title) {
            ForEach(model.rows) { row in
                deviceRow(row)
            }
            if model.rows.isEmpty {
                Text(L10n.Devices.empty)
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            identityKeyDisclosure
            revocationDisclosure
            withdrawalReport
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
            Self.removeConfirmTitle(nickname: revoking?.nickname),
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
            Button(L10n.Devices.cancel, role: .cancel) { revoking = nil }
        } message: {
            Text(L10n.Devices.removeConfirmMessage)
        }
        // ADR-0056 §6: the three things a withdrawal does that a user would
        // not guess, stated before the button rather than after it.
        .confirmationDialog(
            Self.withdrawConfirmTitle(nickname: withdrawing?.nickname),
            isPresented: Binding(get: { withdrawing != nil }, set: { if !$0 { withdrawing = nil } }),
            titleVisibility: .visible
        ) {
            Button(L10n.Devices.withdrawConfirm) {
                if let row = withdrawing { Task { await model.withdraw(row) } }
                withdrawing = nil
            }
            .accessibilityIdentifier("devices.withdrawConfirm")
            Button(L10n.Devices.cancel, role: .cancel) { withdrawing = nil }
        } message: {
            Text(Self.withdrawConfirmMessage(device: Platform.deviceName))
        }
    }

    /// The confirmation dialog's title. A nickname is a non-optional `String`,
    /// so "no nickname" is the empty one, not only the absent row: both get the
    /// unnamed title rather than "Remove ?".
    nonisolated static func removeConfirmTitle(nickname: String?) -> String {
        guard let nickname, !nickname.isEmpty else { return L10n.Devices.removeConfirmTitleUnnamed }
        return L10n.Devices.removeConfirmTitle(name: nickname)
    }

    /// The withdrawal confirmation's title, unnamed on the same terms as
    /// ``removeConfirmTitle(nickname:)``.
    nonisolated static func withdrawConfirmTitle(nickname: String?) -> String {
        guard let nickname, !nickname.isEmpty else { return L10n.Devices.withdrawConfirmTitleUnnamed }
        return L10n.Devices.withdrawConfirmTitle(name: nickname)
    }

    /// ADR-0056 §6's three warnings, in its order: the keys the device is sent,
    /// the other devices whose removals still stand, and the relay, which
    /// nothing can tell to readmit a device yet
    /// ([#1299](https://github.com/justin13888/Sunrise/issues/1299)).
    nonisolated static func withdrawConfirmMessage(device: String) -> String {
        [
            L10n.Devices.withdrawConfirmKeys(device: device),
            L10n.Devices.withdrawConfirmOthers,
            L10n.Devices.withdrawConfirmRelay
        ].joined(separator: "\n\n")
    }

    @ViewBuilder
    private func deviceRow(_ row: DeviceListModel.DeviceRow) -> some View {
        VStack(alignment: .leading, spacing: 2) {
            HStack {
                Text(row.nickname.isEmpty ? L10n.Devices.unnamed : row.nickname)
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
                Button(L10n.Devices.remove, role: .destructive) { revoking = row }
                    .accessibilityIdentifier("devices.revoke")
            }
            // Only where the core says this device may withdraw: a removal
            // another device made, or one signed as the account identity, is
            // not this device's to take back (ADR-0056 §6).
            if row.withdrawable {
                Button(L10n.Devices.withdraw) { withdrawing = row }
                    .accessibilityIdentifier("devices.withdraw")
            }
        }
        .contextMenu {
            if !row.isThisDevice && !row.revoked {
                Button(L10n.Devices.removeEllipsis, role: .destructive) { revoking = row }
            }
            if row.withdrawable {
                Button(L10n.Devices.withdrawEllipsis) { withdrawing = row }
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
        if row.isThisDevice { out.append(L10n.Devices.markThisDevice(device: Platform.deviceName)) }
        // ADR-0058 §7: a removal nothing can take back, beside one a later
        // change can. Both say "removed"; only the signed one says it is for
        // good, and the ordinary one says what taking it back would leave.
        if row.revoked {
            out.append(
                row.revokedByAuthority ? L10n.Devices.markRemovedByAuthority : L10n.Devices.markRemoved
            )
        }
        if !row.revoked, row.readBounded {
            // The two removal facts have come apart. Since ADR-0041 the
            // register is a fold, so a removal stops being recorded once the
            // ledger shows its own author was removed first, while the key
            // bound never comes back. Without this line the row looks like any
            // other member while receiving nothing at all — the one outcome of
            // that design a user could be surprised by. Worded as a state with
            // its remedy, for the same reason the two marks below are.
            out.append(L10n.Devices.markReadBounded)
        }
        if !row.current {
            out.append(L10n.Devices.markNotCurrent)
        }
        if row.admittedAfterRevocation {
            out.append(L10n.Devices.markAdmittedAfterRevocation)
        }
        return out
    }

    /// #144's first signal. Shown only where it is true — a device that does
    /// not hold the key has nothing to be careful about, and a warning printed
    /// on every account is a warning nobody reads.
    @ViewBuilder
    private var identityKeyDisclosure: some View {
        if model.holdsIdentityKey {
            Text(L10n.Devices.identityKey(device: Platform.deviceName))
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
    ///
    /// Neither branch can contradict the relay queue. The gated branch says in
    /// fixed copy that the removal tells the relay nothing, and that is exact:
    /// a gated removal leaves its target current, and the core owes the relay
    /// an intent only while the register calls its device revoked — so an
    /// older intent for the same device, queued before its removal unwound, is
    /// held rather than sent (#257). The other branch reads
    /// `Core::relay_revocation_pending`, which answers from that same set.
    @ViewBuilder
    private var revocationDisclosure: some View {
        if let done = model.lastRevocation, done.gated {
            VStack(alignment: .leading, spacing: 4) {
                Text(L10n.Devices.gatedTitle(name: done.nickname))
                Text(L10n.Devices.gatedReason(device: Platform.deviceName, name: done.nickname))
                Text(L10n.Devices.gatedRemedy)
                unwoundNotice(done)
                Button(L10n.Devices.done) { model.dismissRevocation() }
            }
            .font(.caption)
            .accessibilityIdentifier("devices.revocationGated")
        } else if let done = model.lastRevocation {
            VStack(alignment: .leading, spacing: 4) {
                Text(L10n.Devices.removedTitle(name: done.nickname))
                if done.unrotatedStreams.isEmpty {
                    Text(L10n.Devices.removedAllRotated)
                } else {
                    Text(L10n.Devices.removedSomeUnrotated(count: done.unrotatedStreams.count))
                    ForEach(done.unrotatedStreams, id: \.self) { id in
                        Text(id).monospaced()
                    }
                }
                authorityNotice(done)
                Text(done.relayPending ? L10n.Devices.relayPending : L10n.Devices.relayTold)
                unwoundNotice(done)
                Button(L10n.Devices.done) { model.dismissRevocation() }
            }
            .font(.caption)
            .accessibilityIdentifier("devices.revocationResult")
        }
    }

    /// What the last withdrawal did, in the model's words: only a reinstated
    /// device is called back, and the relay's half is said either way.
    @ViewBuilder
    private var withdrawalReport: some View {
        if let done = model.lastWithdrawal {
            VStack(alignment: .leading, spacing: 4) {
                ForEach(done.lines(device: Platform.deviceName), id: \.self) { line in
                    Text(line)
                }
                Button(L10n.Devices.done) { model.dismissWithdrawal() }
            }
            .font(.caption)
            .accessibilityIdentifier("devices.withdrawalResult")
        }
    }

    /// Which kind of removal went through (ADR-0058 §7).
    ///
    /// On the effective branch only: a signed removal is never gated, so a
    /// gated one is ordinary, and the gated report already says nothing was
    /// removed. When only out-of-date builds kept it ordinary, those devices
    /// are listed, because updating them is the remedy.
    @ViewBuilder
    private func authorityNotice(_ done: DeviceListModel.Revocation) -> some View {
        switch done.authority {
        case .signed:
            Text(L10n.Devices.authoritySigned)
                .accessibilityIdentifier("devices.authoritySigned")
        case .ordinary:
            Text(L10n.Devices.authorityOrdinary(device: Platform.deviceName))
                .accessibilityIdentifier("devices.authorityOrdinary")
        case let .awaitsUpdate(ids):
            Text(L10n.Devices.authorityAwaitsUpdate(count: ids.count))
                .accessibilityIdentifier("devices.authorityAwaitsUpdate")
            ForEach(ids, id: \.self) { id in
                Text(id).monospaced()
            }
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
            Text(L10n.Devices.unwound(count: done.unwound.count))
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
        case .lost: L10n.Devices.reasonLost
        case .stolen: L10n.Devices.reasonStolen
        case .retired: L10n.Devices.reasonRetired
        case .compromised: L10n.Devices.reasonCompromised
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
