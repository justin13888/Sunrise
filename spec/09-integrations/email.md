---
status: accepted
---

# Email-to-Sunrise (Deferred)

Out of scope for v1. Specced here so the surface is anticipated and not built into a corner.

## Concept

Each user gets a per-account inbound email address: `<account-handle>@in.sunrise.example`. Emails sent there become Inbox tasks.

## Why deferred

- Inbound email infrastructure is operationally non-trivial (DKIM/SPF/DMARC handling, abuse mitigation, attachments).
- Privacy concern: the receiving server *would* see plaintext (email is plaintext until accepted). This breaks the E2EE story unless we either:
  - Accept inbound emails on a separate, isolated path that immediately encrypts under the user's identity DH key and then becomes ciphertext (the receiver must hold a key the user can give out via OOB).
  - Run inbound parsing on-device (impractical without persistent server-side reception).

We prefer the first approach if we ship this.

## Sketch (when revisited)

1. User generates an inbound key for their identity (a sub-key derived from `ID_D`).
2. User publishes a public-key bundle that an SMTP relay can use.
3. The Sunrise-operated SMTP relay (or self-hosted) accepts mail, encrypts the email body to the user's inbound public key, and stores ciphertext as a "pending capture" blob.
4. User's device fetches pending captures, decrypts, materializes as Inbox tasks.

This is genuinely tricky; spending more spec ink before we commit is unwise.

## Workaround for v1

Users can use:

- **iOS Share Sheet** to send a selected email body or link to Sunrise as a captured note.
- **Android share intent** equivalently.
- **Browser extension** "send page to Sunrise" that captures email web content.

These cover the primary use case ("save this for later") without needing inbound mail.
