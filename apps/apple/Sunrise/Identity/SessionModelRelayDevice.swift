import Foundation

// Read in its own file only because `SessionModel.swift` is at the length this
// project lints for; it reads nothing the class keeps private.
extension SessionModel {
    /// The relay's id for this device against the open vault, valid on
    /// `relayURL` under `bearer`'s account, `nil` for an unbound sync driver,
    /// or the Keychain's refusal to say which. Takes the same two values the
    /// caller's `SyncPlan` does, so the id and the connection it is presented
    /// on describe one relay and one account.
    ///
    /// A `Result`, not a throw: its consumers, `SyncPlan` above all, turn a
    /// refusal into a plan to stay off (#284), so each sync start passes it
    /// through.
    ///
    /// A read rather than stored state: the environment override
    /// `RelayDeviceID.resolve` consults is a launch-time fact, and the stored
    /// half is written by registration — the recovery ceremony's or
    /// ``bindRelayDevice()``'s — so re-reading is what makes a driver started
    /// after registration pick the binding up.
    func relayDeviceID(relayURL: String, bearer: String?) -> Result<String?, any Error> {
        let scope = RelayDeviceScope(relayURL: relayURL, bearer: bearer)
        return Result { try RelayDeviceID.resolve(store: relayDeviceStore, scope: scope) }
    }
}
