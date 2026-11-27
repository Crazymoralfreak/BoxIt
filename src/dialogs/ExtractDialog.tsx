// Where the content of the archive goes, and what happens when something
// is already there.

import { useEffect, useState, type FormEvent } from "react";
import { api, toAppError } from "../api";
import type { Picked } from "../browser/Browser";
import { Dialog } from "../components/Dialog";
import { baseName, parentPath, useFormat } from "../format";
import { useT } from "../i18n";
import {
  CONFLICT_POLICIES,
  EXTRACT_PLACES,
  SYMLINK_POLICIES,
  type AppError,
  type ConflictPolicy,
  type ExtractChoices,
  type ExtractPlace,
  type Opened,
  type SymlinkPolicy,
} from "../types";

export default function ExtractDialog({
  archive,
  picked,
  choices,
  onStarted,
  onClose,
}: {
  archive: Opened;
  /** The rows to extract, and the folder they are shown in. */
  picked: Picked;
  /** The choices of the last time. */
  choices: ExtractChoices;
  /** The extraction is queued; the choices are the ones to remember. */
  onStarted: (choices: ExtractChoices) => void;
  onClose: () => void;
}) {
  const translator = useT();
  const { t, td } = translator;
  const format = useFormat();
  const [place, setPlace] = useState<ExtractPlace>(choices.place);
  const [folder, setFolder] = useState(choices.folder);
  const [conflicts, setConflicts] = useState<ConflictPolicy>(choices.conflicts);
  const [symlinks, setSymlinks] = useState<SymlinkPolicy>(choices.symlinks);
  const [destination, setDestination] = useState("");
  const [failure, setFailure] = useState<AppError | null>(null);
  const [busy, setBusy] = useState(false);

  // Where the choice leads, as the core resolves it.
  useEffect(() => {
    let alive = true;
    api
      .extractDestination(archive.id, place, folder)
      .then((path) => {
        if (alive) {
          setDestination(path);
        }
      })
      .catch((error: unknown) => {
        if (alive) {
          setFailure(toAppError(error));
        }
      });
    return () => {
      alive = false;
    };
  }, [archive.id, place, folder]);

  const browse = async () => {
    const start = folder.trim() !== "" ? folder : parentPath(archive.info.path);
    const chosen = await api.pickFolder(start);
    if (chosen !== null) {
      setFolder(chosen);
      setPlace("chosen");
    }
  };

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    setBusy(true);
    setFailure(null);
    try {
      await api.extractStart(archive.id, picked.folder.id, picked.rows, destination, { conflicts, symlinks });
      onStarted({ place, folder, conflicts, symlinks });
    } catch (error) {
      setFailure(toAppError(error));
      setBusy(false);
    }
  };

  const scope =
    picked.count > 0
      ? t("extract.scopeSelected", { count: format.count(picked.count) })
      : picked.folder.id === 0
        ? t("extract.scopeAll")
        : t("extract.scopeFolder", { name: picked.folder.name });

  return (
    <Dialog
      title={t("extract.title")}
      onClose={onClose}
      wide
      footer={
        <>
          <button type="button" onClick={onClose}>
            {t("common.cancel")}
          </button>
          <button type="submit" form="extract" className="primary" disabled={busy || destination === ""}>
            {t("extract.start")}
          </button>
        </>
      }
    >
      <form id="extract" className="form" onSubmit={(event) => void submit(event)}>
        <p>
          <span className="dim">{baseName(archive.info.path)}</span> · {scope}
        </p>

        <fieldset className="choices">
          <legend>{t("extract.where")}</legend>
          {EXTRACT_PLACES.map((option) => (
            <label key={option} className="check">
              <input
                type="radio"
                name="place"
                checked={place === option}
                onChange={() => setPlace(option)}
                data-autofocus={place === option ? true : undefined}
              />
              <span>{td(`extract.place.${option}`)}</span>
            </label>
          ))}
          <div className="with-button">
            <input
              type="text"
              value={folder}
              placeholder={t("extract.folder")}
              onChange={(event) => {
                setFolder(event.target.value);
                setPlace("chosen");
              }}
              spellCheck={false}
            />
            <button type="button" onClick={() => void browse()}>
              {t("common.browse")}
            </button>
          </div>
          <p className="dim path-text">{t("extract.into", { path: destination })}</p>
        </fieldset>

        <div className="fields">
          <label className="field">
            <span>{t("extract.conflicts")}</span>
            <select value={conflicts} onChange={(event) => setConflicts(event.target.value as ConflictPolicy)}>
              {CONFLICT_POLICIES.map((policy) => (
                <option key={policy} value={policy}>
                  {td(`extract.conflictKind.${policy}`)}
                </option>
              ))}
            </select>
          </label>
          <label className="field">
            <span>{t("extract.symlinks")}</span>
            <select value={symlinks} onChange={(event) => setSymlinks(event.target.value as SymlinkPolicy)}>
              {SYMLINK_POLICIES.map((policy) => (
                <option key={policy} value={policy}>
                  {td(`extract.symlinkKind.${policy}`)}
                </option>
              ))}
            </select>
          </label>
        </div>

        {failure && <p className="notice error">{translator.error(failure)}</p>}
      </form>
    </Dialog>
  );
}
