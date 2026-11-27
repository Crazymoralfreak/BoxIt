// What the window shows before an archive is open.

import { Icon } from "../components/Icon";
import { baseName, parentPath } from "../format";
import { useT } from "../i18n";

export function Welcome({
  recent,
  onOpen,
  onOpenPath,
  onCreate,
  onForget,
}: {
  /** The archives opened lately, the latest first. */
  recent: string[];
  onOpen: () => void;
  onOpenPath: (path: string) => void;
  onCreate: () => void;
  /** Empties the list of the archives opened lately. */
  onForget: () => void;
}) {
  const { t } = useT();
  return (
    <section className="welcome">
      <div className="welcome-hero">
        <h1 className="wordmark">BoxIt</h1>
        <p className="welcome-line">{t("welcome.line")}</p>
        <div className="buttons">
          <button type="button" className="primary large" onClick={onOpen}>
            <Icon name="open" size={18} />
            {t("welcome.open")}
          </button>
          <button type="button" className="large" onClick={onCreate}>
            <Icon name="box" size={18} />
            {t("welcome.create")}
          </button>
        </div>
      </div>

      {recent.length > 0 && (
        <div className="recent">
          <header>
            <h2>{t("welcome.recent")}</h2>
            <button type="button" className="ghost" onClick={onForget}>
              {t("welcome.forget")}
            </button>
          </header>
          <ul>
            {recent.map((path) => (
              <li key={path}>
                <button type="button" className="ghost recent-item" onClick={() => onOpenPath(path)} title={path}>
                  <Icon name="box" />
                  <span className="label">{baseName(path)}</span>
                  <span className="label dim">{parentPath(path)}</span>
                </button>
              </li>
            ))}
          </ul>
        </div>
      )}
    </section>
  );
}
