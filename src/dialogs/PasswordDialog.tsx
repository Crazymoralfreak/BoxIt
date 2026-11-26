// Asks for the password of an archive: to read its list, or to look at a
// file of it.

import { useEffect, useState, type FormEvent } from "react";
import { api } from "../api";
import { Dialog } from "../components/Dialog";
import { useT } from "../i18n";

export default function PasswordDialog({
  name,
  wrong,
  onSubmit,
  onClose,
}: {
  /** What the password is for. */
  name: string;
  /** The password given before did not fit. */
  wrong: boolean;
  onSubmit: (password: string, remember: boolean) => void;
  onClose: () => void;
}) {
  const { t } = useT();
  const [password, setPassword] = useState("");
  const [remember, setRemember] = useState(false);
  const [saved, setSaved] = useState<string[]>([]);
  useEffect(() => {
    let alive = true;
    api
      .passwordNames()
      .then((names) => {
        if (alive) {
          setSaved(names);
        }
      })
      .catch(() => undefined);
    return () => {
      alive = false;
    };
  }, []);
  const useSaved = (chosen: string) => {
    if (chosen === "") {
      return;
    }
    void api.passwordNamed(chosen).then((text) => {
      if (text !== null) {
        setPassword(text);
      }
    });
  };
  const submit = (event: FormEvent) => {
    event.preventDefault();
    if (password !== "") {
      onSubmit(password, remember);
    }
  };
  return (
    <Dialog
      title={t("question.passwordTitle")}
      onClose={onClose}
      footer={
        <>
          <button type="button" onClick={onClose}>
            {t("common.cancel")}
          </button>
          <button type="submit" form="archive-password" className="primary" disabled={password === ""}>
            {t("common.ok")}
          </button>
        </>
      }
    >
      <form id="archive-password" className="form" onSubmit={submit}>
        {wrong && <p className="notice error">{t("question.passwordWrong")}</p>}
        <label className="field">
          <span>{t("question.passwordFor", { name })}</span>
          <input
            type="password"
            value={password}
            onChange={(event) => setPassword(event.target.value)}
            autoComplete="off"
            data-autofocus
          />
        </label>
        {saved.length > 0 && (
          <label className="field">
            <span>{t("create.saved")}</span>
            <select value="" onChange={(event) => useSaved(event.target.value)}>
              <option value="">{t("create.savedNone")}</option>
              {saved.map((savedName) => (
                <option key={savedName} value={savedName}>
                  {savedName}
                </option>
              ))}
            </select>
          </label>
        )}
        <label className="check">
          <input type="checkbox" checked={remember} onChange={(event) => setRemember(event.target.checked)} />
          <span>{t("question.passwordRemember")}</span>
        </label>
      </form>
    </Dialog>
  );
}
