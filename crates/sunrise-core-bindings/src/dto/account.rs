//! Account: what bootstrapping an account on the relay hands back.

/// What publishing a vault to the relay established, and the recovery code
/// that goes with it.
///
/// The return of [`crate::SunriseCore::bootstrap_account`], which is the Apple
/// clients' `sunrise bootstrap`.
#[derive(Clone, uniffi::Record)]
pub struct AccountBootstrap {
    /// The relay's id for this account.
    pub identity_id: String,
    /// The normalized email the relay recorded, which the identity provider
    /// owns and this value reports rather than sets.
    pub email: String,
    /// The id the relay assigned this device. Every later request names it.
    ///
    /// **The caller must record it before it lets this record go.** The relay
    /// returns it to the registering device and to nobody else, never sends it
    /// again, and without it a relay with `require_device_sig` refuses every
    /// request this device makes. The Apple app writes it to the vault's
    /// `KeychainRelayDeviceIDStore`; it used to read `recovery_code` off this
    /// record and drop the rest
    /// ([#183](https://github.com/justin13888/Sunrise/issues/183)).
    pub device_id: String,
    /// The twenty-four words, or `None` on a device admitted by pairing.
    ///
    /// **Shown once and never stored.** `sunrise-cli`'s `print_recovery_code`
    /// records why in full: a copy this process saved would be a copy an
    /// attacker reaching the machine also has, while doing nothing for the user
    /// who loses the machine. The Swift side must put this on screen and let it
    /// go — not into a file, not into a log, not into a pasteboard that
    /// outlives the screen.
    ///
    /// `None` is not a failure. It means this device holds no `ID_D_priv`, so
    /// it cannot seal a blob; the device that created the account is the one
    /// that can, and it already did.
    pub recovery_code: Option<String>,
}

// Hand-written, like `LoginCredentials`': `recovery_code` is the plaintext
// twenty-four words, and a derived `Debug` would put them in the first log line
// anyone writes while debugging the seam. Destructured so a new field has to be
// placed here before the crate builds.
impl std::fmt::Debug for AccountBootstrap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self {
            identity_id,
            email,
            device_id,
            recovery_code,
        } = self;
        f.debug_struct("AccountBootstrap")
            .field("identity_id", identity_id)
            .field("email", email)
            .field("device_id", device_id)
            .field(
                "recovery_code",
                &recovery_code.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::AccountBootstrap;

    /// The recovery code never prints; the ids around it still do, and a
    /// device admitted by pairing still reads as having none.
    #[test]
    fn an_account_bootstrap_redacts_its_recovery_code_under_debug() {
        let b = AccountBootstrap {
            identity_id: "ident-1".into(),
            email: "a@example.test".into(),
            device_id: "dev-1".into(),
            recovery_code: Some("abandon ability able".into()),
        };
        let s = format!("{b:?}");
        assert!(!s.contains("abandon"), "{s}");
        assert!(s.contains("recovery_code: Some(\"<redacted>\")"), "{s}");
        assert!(s.contains("ident-1") && s.contains("dev-1"), "{s}");
        let none = format!(
            "{:?}",
            AccountBootstrap {
                recovery_code: None,
                ..b
            }
        );
        assert!(none.contains("recovery_code: None"), "{none}");
    }
}
