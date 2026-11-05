import { type FormEvent, useId, useState } from "react";
import { t } from "./i18n";
import {
    currentServerText,
    forgetServerUrl,
    forgottenText,
    issueText,
    planServerChange,
    type SettingsWrite,
    saveServerUrl,
    settingsView,
} from "./server-settings";
import { configuredServer, replaceConfiguredServer } from "./server-url";

/** A switch waiting on the user's yes: from typing, or from a refused link. */
type Pending = { from: string; to: string; fromLink: boolean };

/** One line of feedback under the form. */
type Notice = { text: string; alert: boolean };

const buildDefault = (): string | undefined =>
    import.meta.env.SUNRISE_WEB_DEFAULT_SERVER_URL;

/** `localStorage`, reached on each call: reading the global can itself throw. */
const storage = {
    getItem: (key: string) => localStorage.getItem(key),
    setItem: (key: string, value: string) => localStorage.setItem(key, value),
    removeItem: (key: string) => localStorage.removeItem(key),
};

/**
 * The sync server section of the settings view: the relay in use, a field to
 * change it, and a confirmation before a stored relay is replaced. The
 * decisions are `server-settings.ts`'s; this only renders them.
 */
export function ServerSettings() {
    const [resolution, setResolution] = useState(configuredServer);
    const view = settingsView(resolution);
    const [typed, setTyped] = useState("");
    const [notice, setNotice] = useState<Notice | null>(null);
    const [pending, setPending] = useState<Pending | null>(
        view.offered !== null && view.url !== null
            ? { from: view.url, to: view.offered, fromLink: true }
            : null,
    );
    const fieldId = useId();
    const noticeId = useId();

    function commit(write: SettingsWrite, done: string) {
        replaceConfiguredServer(write.resolution);
        setResolution(write.resolution);
        setPending(null);
        setNotice(
            write.persisted
                ? { text: done, alert: false }
                : { text: t.web.settings.storageRefused(), alert: true },
        );
    }

    function save(origin: string) {
        commit(
            saveServerUrl(storage, origin, buildDefault()),
            t.web.settings.saved({ url: origin }),
        );
        setTyped("");
    }

    function onSubmit(event: FormEvent) {
        event.preventDefault();
        const change = planServerChange(typed, view);
        switch (change.kind) {
            case "invalid":
                setNotice({ text: issueText(change.issue), alert: true });
                return;
            case "unchanged":
                setNotice({ text: t.web.settings.unchanged(), alert: false });
                return;
            case "save":
                save(change.origin);
                return;
            case "confirm":
                setNotice(null);
                setPending({
                    from: change.from,
                    to: change.to,
                    fromLink: false,
                });
                return;
        }
    }

    function onForget() {
        const write = forgetServerUrl(storage, buildDefault());
        commit(write, forgottenText(write.resolution));
    }

    function onCancel() {
        // A refused link is answered once; dropping it from the resolution
        // keeps it from being offered again on this page.
        if (pending?.fromLink) {
            const next = { ...resolution, offered: null };
            replaceConfiguredServer(next);
            setResolution(next);
        }
        setPending(null);
    }

    return (
        <section aria-labelledby={`${fieldId}-heading`}>
            <h3 id={`${fieldId}-heading`}>{t.web.settings.serverHeading()}</h3>
            <p>{currentServerText(view)}</p>
            <p style={{ opacity: 0.75 }}>{t.web.settings.notSyncingYet()}</p>

            {pending !== null && (
                <div
                    role="alertdialog"
                    aria-labelledby={`${fieldId}-confirm`}
                    style={{
                        border: "1px solid currentColor",
                        borderRadius: 6,
                        padding: 12,
                        marginBlock: 12,
                    }}
                >
                    {pending.fromLink && (
                        <p>
                            {t.web.settings.linkOffer({
                                url: pending.to,
                                current: pending.from,
                            })}
                        </p>
                    )}
                    <p id={`${fieldId}-confirm`}>
                        {t.web.settings.confirmReplace({
                            from: pending.from,
                            to: pending.to,
                        })}
                    </p>
                    <div style={{ display: "flex", gap: 8 }}>
                        <button type="button" onClick={() => save(pending.to)}>
                            {t.web.settings.confirmSwitch()}
                        </button>
                        <button type="button" onClick={onCancel}>
                            {t.web.settings.cancelSwitch()}
                        </button>
                    </div>
                </div>
            )}

            <form onSubmit={onSubmit} noValidate>
                <label htmlFor={fieldId} style={{ display: "block" }}>
                    {t.web.settings.serverField()}
                </label>
                <div style={{ display: "flex", gap: 8, marginBlock: 4 }}>
                    <input
                        id={fieldId}
                        type="url"
                        inputMode="url"
                        autoComplete="url"
                        spellCheck={false}
                        placeholder={t.web.settings.serverPlaceholder()}
                        value={typed}
                        aria-describedby={notice ? noticeId : undefined}
                        aria-invalid={notice?.alert ? true : undefined}
                        onChange={(e) => setTyped(e.target.value)}
                        style={{ flex: 1, minInlineSize: 0 }}
                    />
                    <button type="submit">{t.web.settings.useServer()}</button>
                </div>
            </form>
            {notice !== null && (
                <p
                    id={noticeId}
                    role={notice.alert ? "alert" : "status"}
                    style={notice.alert ? { color: "tomato" } : undefined}
                >
                    {notice.text}
                </p>
            )}
            {view.canForget && (
                <button type="button" onClick={onForget}>
                    {t.web.settings.forgetServer()}
                </button>
            )}
        </section>
    );
}
