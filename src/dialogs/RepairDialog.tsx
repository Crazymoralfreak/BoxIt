// Writes what still reads of the open archive into a new file.

import { useEffect, useState, type FormEvent } from "react";
import { api, toAppError } from "../api";
import { Dialog } from "../components/Dialog";
import { useT } from "../i18n";
import type { AppError } from "../types";

export default function RepairDialog({
  id,
  onStarted,
  onClose,
}: {
  id: number;
  onStarted: () => void;
  onClose: () => void;
}) {
  const translator = useT();
  const { t } = translator;
  const [destination, setDestination] = useState("");
  const [failure, setFailure] = useState<AppError | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    let alive = true;
    api
      .repairSuggest(id)
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
  }, [id]);

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    if (destination.trim() === "" || busy) {
      return;
    }
    setBusy(true);
    setFailure(null);
    try {
      let overwrite = false;
      if ((await api.pathState(destination)) === "file") {
        overwrite = await api.confirm(t("repair.replaceAsk", { path: destination }), t("repair.replace"));
        if (!overwrite) {
          setBusy(false);
          return;
        }
      }
      await api.repairStart(id, destination, overwrite);
      onStarted();
    } catch (error) {
      setFailure(toAppError(error));
      setBusy(false);
    }
  };

  return (
    <Dialog
      title={t("repair.title")}
      onClose={onClose}
      footer={
        <>
          <button type="button" onClick={onClose}>
            {t("common.cancel")}
          </button>
          <button type="submit" form="repair" className="primary" disabled={busy || destination.trim() === ""}>
            {t("repair.start")}
          </button>
        </>
      }
    >
      <form id="repair" className="form" onSubmit={(event) => void submit(event)}>
        <p className="dim">{t("repair.lead")}</p>
        <label className="field">
          <span>{t("repair.destination")}</span>
          <input value={destination} onChange={(event) => setDestination(event.target.value)} />
        </label>
        {failure && <p className="notice error">{translator.error(failure)}</p>}
      </form>
    </Dialog>
  );
}
