import Foundation
import Security
import Testing

@testable import Sunrise

/// `KeychainItem.meansTheOtherStoreWasUnreachable`, asserted directly.
///
/// The predicate decides whether a refusal from the *other* keychain means
/// that store was never reachable from this build — swallow it, it says
/// nothing about whether a copy of ours survives — or that it was reached well
/// enough to refuse on its own terms, in which case a copy may still be sitting
/// in it and the refusal is raised.
///
/// It makes no `Security.framework` call: it is a pure `OSStatus -> Bool`. The
/// only thing that kept its `false` arm unpinned was the `private` keyword,
/// which `@testable import` does not reach past. Widening it to `internal`
/// observes a decision the production path takes and cannot make
/// `Security.framework` answer anything it would not, which is the ground
/// `probeRuns` and `probeInsertQuery` were widened on.
///
/// No platform gate, because no keychain is touched on any platform.
struct KeychainUnreachableStatusTests {
    /// The two statuses that mean the other store was never reachable from
    /// here, and so say nothing about what it holds.
    @Test
    func theTwoUnreachableStatusesAreSwallowed() {
        #expect(KeychainItem.meansTheOtherStoreWasUnreachable(errSecMissingEntitlement))
        #expect(KeychainItem.meansTheOtherStoreWasUnreachable(errSecItemNotFound))
    }

    /// The `false` arm, which executes in no cross-domain delete this suite can
    /// stage. Each of these is a real refusal from a store that exists and
    /// answered — a locked keychain, a failed or cancelled prompt, an I/O
    /// failure — and each must be raised rather than swallowed, because a copy
    /// of ours may survive in it and reporting the clear as a success is the
    /// failure `deleteAcrossDomains` exists to prevent.
    @Test(arguments: [
        errSecInteractionNotAllowed,
        errSecAuthFailed,
        errSecUserCanceled,
        errSecIO
    ])
    func aStoreThatRefusedOnItsOwnTermsIsNotUnreachable(_ status: OSStatus) {
        #expect(!KeychainItem.meansTheOtherStoreWasUnreachable(status))
    }
}
