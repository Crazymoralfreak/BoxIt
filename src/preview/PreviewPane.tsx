// A look at the chosen file without extracting it. What is inside an
// archive comes from a stranger, so nothing of it runs here: a picture is
// shown as a picture, a text as plain text, a PDF in the viewer of the
// system, and the core decides which of the three a file is.

import { useEffect, useState } from "react";
import { api, toAppError, type PreviewContent } from "../api";
import { Icon } from "../components/Icon";
import { useT } from "../i18n";
import type { AppError, Row } from "../types";

/** How long the choice rests before the file is read, in milliseconds. */
const DELAY = 120;

/** Errors a password would cure. */
const NEEDS_PASSWORD = ["passwordRequired", "wrongPassword", "damagedOrWrongPassword"];

type Look =
  | { state: "loading" }
  | { state: "failed"; error: AppError }
  | { state: "image"; address: string }
  | { state: "pdf"; address: string }
  | { state: "text"; text: string; truncated: boolean };

async function shown(content: PreviewContent): Promise<Look> {
  switch (content.kind) {
    case "image":
      return { state: "image", address: URL.createObjectURL(content.blob) };
    case "pdf":
      return { state: "pdf", address: URL.createObjectURL(content.blob) };
    case "text":
      return { state: "text", text: await content.blob.text(), truncated: content.truncated };
    default: {
      const unknown: never = content.kind;
      throw new Error(`unknown preview kind: ${String(unknown)}`);
    }
  }
}

function release(look: Look) {
  if (look.state === "image" || look.state === "pdf") {
    URL.revokeObjectURL(look.address);
  }
}

export default function PreviewPane({
  session,
  row,
  version,
  onPassword,
}: {
  /** The open archive. */
  session: number;
  /** The file to show; `null` when the choice is not one file. */
  row: Row | null;
  /** Changes when the file is worth reading again: a password was given. */
  version: number;
  /** Asks for the password of the archive. */
  onPassword: () => void;
}) {
  const translator = useT();
  const { t } = translator;
  const [look, setLook] = useState<Look | null>(null);
  const id = row?.id ?? null;

  useEffect(() => {
    if (id === null) {
      setLook(null);
      return;
    }
    const abort = new AbortController();
    let made: Look | null = null;
    setLook({ state: "loading" });
    const timer = window.setTimeout(() => {
      api
        .preview(session, id, abort.signal)
        .then(shown)
        .then((next) => {
          if (abort.signal.aborted) {
            release(next);
          } else {
            made = next;
            setLook(next);
          }
        })
        .catch((error: unknown) => {
          if (!abort.signal.aborted) {
            setLook({ state: "failed", error: toAppError(error) });
          }
        });
    }, DELAY);
    return () => {
      window.clearTimeout(timer);
      abort.abort();
      if (made) {
        release(made);
      }
    };
  }, [session, id, version]);

  let content;
  if (!row || !look) {
    content = <p className="preview-note dim">{t("preview.none")}</p>;
  } else {
    switch (look.state) {
      case "loading":
        content = <p className="preview-note dim">{t("preview.loading")}</p>;
        break;
      case "failed":
        content = (
          <div className="preview-note">
            <p className="dim">{translator.error(look.error)}</p>
            {NEEDS_PASSWORD.includes(look.error.code) && (
              <button type="button" onClick={onPassword}>
                <Icon name="lock" />
                {t("preview.password")}
              </button>
            )}
          </div>
        );
        break;
      case "image":
        content = (
          <div className="preview-image">
            <img src={look.address} alt={row.name} draggable={false} />
          </div>
        );
        break;
      case "pdf":
        content = <iframe className="preview-pdf" src={look.address} title={row.name} />;
        break;
      case "text":
        content = (
          <>
            {look.truncated && <p className="preview-cut dim">{t("preview.truncated")}</p>}
            <pre className="preview-text">{look.text}</pre>
          </>
        );
        break;
      default: {
        const unknown: never = look;
        throw new Error(`unknown look: ${String(unknown)}`);
      }
    }
  }

  return (
    <aside className="preview" aria-label={t("preview.title")}>
      <header className="preview-head">
        <span className="label" title={row?.name}>
          {row ? row.name : t("preview.title")}
        </span>
      </header>
      <div className="preview-body">{content}</div>
    </aside>
  );
}
