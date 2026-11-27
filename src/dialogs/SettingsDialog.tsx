// The settings of the window, and what this copy of BoxIt is.

import { useEffect, useEffectEvent, useState } from "react";
import { api, toAppError } from "../api";
import { useAppEvent } from "../api/events";
import { Dialog } from "../components/Dialog";
import { LANGUAGE_NAMES, useT, type Translator } from "../i18n";
import {
  LANGUAGES,
  THEMES,
  type AppError,
  type AppInfo,
  type Language,
  type Settings,
  type Theme,
  type UpdateStatus,
} from "../types";

/** What is known about updates, in a line. */
function updateLine(status: UpdateStatus, { t }: Translator): string {
  const version = status.version ?? "";
  switch (status.state) {
    case "idle":
      return t("update.idle");
    case "checking":
      return t("update.checking");
    case "upToDate":
      return t("update.upToDate");
    case "available":
      return t("update.available", { version });
    case "downloading":
      return t("update.downloading", { version });
    case "ready":
      return t("update.ready", { version });
    case "installing":
      return t("update.installing", { version });
    case "failed":
      return status.version === null ? t("update.failedCheck") : t("update.failedInstall", { version });
    default: {
      const unknown: never = status.state;
      throw new Error(`unknown update state: ${String(unknown)}`);
    }
  }
}

/** Nothing is asked of the network until the button here is pressed. */
function Updates({ onFailure }: { onFailure: (error: AppError) => void }) {
  const translator = useT();
  const { t } = translator;
  const [status, setStatus] = useState<UpdateStatus | null>(null);
  const fail = useEffectEvent(onFailure);

  // What a check of this session has already found.
  useEffect(() => {
    let alive = true;
    api
      .updateStatus()
      .then((found) => {
        if (alive) {
          // An event that came meanwhile is newer than this answer.
          setStatus((current) => current ?? found);
        }
      })
      .catch((error: unknown) => fail(toAppError(error)));
    return () => {
      alive = false;
    };
  }, []);
  useAppEvent("update:status", setStatus);

  if (!status) {
    return null;
  }
  const working =
    status.state === "checking" ||
    status.state === "downloading" ||
    status.state === "ready" ||
    status.state === "installing";
  // A version that was found and is not on its way yet.
  const offered = status.version !== null && (status.state === "available" || status.state === "failed");
  const run = (action: Promise<void>) => {
    action.catch((error: unknown) => onFailure(toAppError(error)));
  };

  return (
    <fieldset className="choices">
      <legend>{t("update.title")}</legend>
      <p className={status.state === "failed" ? "update-line failed" : "update-line"} role="status">
        {updateLine(status, translator)}
      </p>
      {status.state === "downloading" && (
        <div className={status.total > 0 ? "bar" : "bar unknown"}>
          <span style={{ transform: `scaleX(${status.total > 0 ? Math.min(1, status.downloaded / status.total) : 0})` }} />
        </div>
      )}
      {offered && !status.installable && <p className="dim">{t("update.portable")}</p>}
      <div className="buttons">
        <button type="button" disabled={working} onClick={() => run(api.updateCheck())}>
          {t("update.check")}
        </button>
        {offered && status.installable && (
          <button type="button" className="primary" onClick={() => run(api.updateInstall())}>
            {t("update.install")}
          </button>
        )}
        {status.version !== null && (
          <button type="button" onClick={() => run(api.updatePage())}>
            {t("update.page")}
          </button>
        )}
      </div>
    </fieldset>
  );
}

export default function SettingsDialog({
  info,
  settings,
  onChange,
  onFailure,
  onClose,
}: {
  info: AppInfo;
  settings: Settings;
  onChange: (settings: Settings) => void;
  onFailure: (error: AppError) => void;
  onClose: () => void;
}) {
  const { t, td } = useT();
  return (
    <Dialog
      title={t("settings.title")}
      onClose={onClose}
      footer={
        <button type="button" className="primary" onClick={onClose} data-autofocus>
          {t("common.close")}
        </button>
      }
    >
      <div className="form">
        <div className="fields">
          <label className="field">
            <span>{t("settings.language")}</span>
            <select
              value={settings.language}
              onChange={(event) => onChange({ ...settings, language: event.target.value as Language | "" })}
            >
              <option value="">{t("settings.languageSystem")}</option>
              {LANGUAGES.map((language) => (
                <option key={language} value={language}>
                  {LANGUAGE_NAMES[language]}
                </option>
              ))}
            </select>
          </label>
          <label className="field">
            <span>{t("settings.theme")}</span>
            <select
              value={settings.theme}
              onChange={(event) => onChange({ ...settings, theme: event.target.value as Theme })}
            >
              {THEMES.map((theme) => (
                <option key={theme} value={theme}>
                  {td(`settings.themeKind.${theme}`)}
                </option>
              ))}
            </select>
          </label>
        </div>

        <fieldset className="choices">
          <legend>{t("settings.about")}</legend>
          <dl className="facts">
            <dt>BoxIt</dt>
            <dd>{t("settings.version", { version: info.version })}</dd>
            <dt>{t("settings.dataFolder")}</dt>
            <dd className="path-text">{info.dataDir}</dd>
            <dt>{t("settings.mode")}</dt>
            <dd>{info.portable ? t("settings.modePortable") : t("settings.modeInstalled")}</dd>
          </dl>
          <div className="buttons">
            <button
              type="button"
              onClick={() => void api.revealPath(info.dataDir).catch((error: unknown) => onFailure(toAppError(error)))}
            >
              {t("settings.openData")}
            </button>
            <button
              type="button"
              onClick={() => void api.revealLogs().catch((error: unknown) => onFailure(toAppError(error)))}
            >
              {t("settings.openLogs")}
            </button>
          </div>
        </fieldset>

        <fieldset className="choices">
          <legend>{t("settings.keys")}</legend>
          <div className="fields">
            {(
              [
                ["view", "keyView"],
                ["edit", "keyEdit"],
                ["copy", "keyCopy"],
                ["extract", "keyExtract"],
              ] as const
            ).map(([key, label]) => (
              <label className="field" key={key}>
                <span>{t(`settings.${label}`)}</span>
                <input
                  value={settings.keys[key]}
                  onChange={(event) =>
                    onChange({ ...settings, keys: { ...settings.keys, [key]: event.target.value } })
                  }
                />
              </label>
            ))}
          </div>
        </fieldset>

        <fieldset className="choices">
          <legend>{t("settings.icons")}</legend>
          <div className="fields">
            {["zip", "7z", "rar", "tar"].map((format) => (
              <label className="field" key={format}>
                <span>{format}</span>
                <select
                  value={settings.icons[format] ?? ""}
                  onChange={(event) => {
                    const icons = { ...settings.icons };
                    if (event.target.value === "") {
                      delete icons[format];
                    } else {
                      icons[format] = event.target.value;
                    }
                    onChange({ ...settings, icons });
                  }}
                >
                  <option value="">{t("create.savedNone")}</option>
                  {["box", "file", "folder", "extract", "test", "lock", "repair"].map((icon) => (
                    <option key={icon} value={icon}>
                      {icon}
                    </option>
                  ))}
                </select>
              </label>
            ))}
          </div>
        </fieldset>

        <fieldset className="choices">
          <legend>{t("settings.explorer")}</legend>
          <div className="buttons">
            <button
              type="button"
              onClick={() => void api.shellRegister().catch((error: unknown) => onFailure(toAppError(error)))}
            >
              {t("settings.explorerRegister")}
            </button>
            <button
              type="button"
              onClick={() => void api.shellUnregister().catch((error: unknown) => onFailure(toAppError(error)))}
            >
              {t("settings.explorerRemove")}
            </button>
          </div>
        </fieldset>

        <fieldset className="choices">
          <legend>{t("settings.passwords")}</legend>
          {settings.savedPasswords.length === 0 ? (
            <p className="dim">{t("settings.passwordsEmpty")}</p>
          ) : (
            <ul className="saved-passwords">
              {settings.savedPasswords.map((name) => (
                <li key={name}>
                  <span>{name}</span>
                  <button
                    type="button"
                    onClick={() =>
                      void api
                        .passwordForget(name)
                        .then(onChange)
                        .catch((error: unknown) => onFailure(toAppError(error)))
                    }
                  >
                    {t("settings.passwordForget")}
                  </button>
                </li>
              ))}
            </ul>
          )}
        </fieldset>

        <Updates onFailure={onFailure} />
      </div>
    </Dialog>
  );
}
