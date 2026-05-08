---
status: draft
---

# Accessibility

Accessibility is a baseline, not a feature. Every release passes the checks in this spec.

## Targets

- **WCAG 2.1 AA** for visual contrast, keyboard navigation, screen reader support.
- **iOS:** VoiceOver, Dynamic Type, Reduce Motion, Differentiate Without Color, Voice Control.
- **Android:** TalkBack, font scaling, color inversion, switch access.
- **Desktop / Web:** NVDA, VoiceOver (macOS), JAWS; full keyboard equivalence.
- **TUI:** runs in 256-color and monochrome; respects `NO_COLOR`; works with screen readers attached to terminal emulators.

## Concrete requirements

### Color and contrast

- Text on surface: AA (4.5:1) minimum, AAA (7:1) preferred for body text.
- Interactive elements at AA against their background.
- Color is never the *only* signal: every color-coded state has a glyph, label, or pattern.

### Type

- Respect Dynamic Type (iOS) and font scale (Android, OS).
- Supports up to 200% scale without truncation or layout breakage.
- Minimum body size: 14pt on mobile, 13pt on desktop.

### Motion

- Respect `prefers-reduced-motion`.
- Transitions disabled or instant in reduced-motion mode.
- No essential information is conveyed only through motion.

### Keyboard

- Every action reachable by keyboard.
- Tab order matches visual order.
- Focus indicator always visible (`outline: none` is forbidden).

### Screen reader

- Every interactive element has an accessible name.
- Live regions used for status changes (sync state, save confirmation).
- Custom controls expose ARIA roles or platform equivalents.

### Touch targets

- 44×44pt minimum on iOS; 48dp on Android.
- Adequate spacing between adjacent targets.

### Captions / alternatives

- Voice captures: optional transcription preview.
- Attached audio/video: future; no autoplay.

## Tooling

- **axe-core** in CI for the web app (zero violations on critical paths).
- **iOS Accessibility Inspector** smoke pass per release.
- **Android Accessibility Scanner** smoke pass per release.
- **Manual screen reader** spot checks: TalkBack on Android, VoiceOver on iOS and macOS, NVDA on Windows.
- **TUI:** validate with `screen-reader-friendly` mode that outputs structured text rather than glyph-art.

## Reduced-functionality fallbacks

| Feature | Fallback |
|---|---|
| Drag-and-drop | Keyboard equivalent (cut + paste, or "Move to…") |
| Calendar grid | List of blocks per day in tabular form |
| Color-coded streams | Stream name always visible; color is supplemental |
| QR pairing | Numeric / alphanumeric code entry |

## Process

- Every PR touching UI runs the automated a11y checks.
- Every release includes a manual a11y test script run by a designer + engineer.
- A11y bugs are P1 by default; we don't ship a release with a known severe a11y regression.
