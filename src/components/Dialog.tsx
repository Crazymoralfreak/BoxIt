// A dialog over the window: takes the keyboard while it is open and gives
// it back to what had it before.

import { useEffect, useId, useRef, type KeyboardEvent, type ReactNode } from "react";
import { useT } from "../i18n";
import { Icon } from "./Icon";

const FOCUSABLE =
  'button:not(:disabled), input:not(:disabled), select:not(:disabled), textarea:not(:disabled), [tabindex]:not([tabindex="-1"])';

export function Dialog({
  title,
  onClose,
  children,
  footer,
  wide = false,
}: {
  title: string;
  /** Escape and the cross; left out, the dialog has to be answered. */
  onClose?: () => void;
  children: ReactNode;
  footer: ReactNode;
  wide?: boolean;
}) {
  const { t } = useT();
  const titleId = useId();
  const panel = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const before = document.activeElement;
    const root = panel.current;
    const first = root?.querySelector<HTMLElement>("[data-autofocus]") ?? root?.querySelector<HTMLElement>(FOCUSABLE);
    (first ?? root)?.focus();
    return () => {
      if (before instanceof HTMLElement && before.isConnected) {
        before.focus();
      }
    };
  }, []);

  const onKeyDown = (event: KeyboardEvent<HTMLDivElement>) => {
    // The shortcuts of the window do not reach through a dialog.
    event.stopPropagation();
    if (event.key === "Escape" && onClose) {
      event.preventDefault();
      onClose();
      return;
    }
    if (event.key !== "Tab" || !panel.current) {
      return;
    }
    const stops = [...panel.current.querySelectorAll<HTMLElement>(FOCUSABLE)];
    if (stops.length === 0) {
      event.preventDefault();
      return;
    }
    const first = stops[0];
    const last = stops[stops.length - 1];
    if (event.shiftKey && document.activeElement === first) {
      event.preventDefault();
      last.focus();
    } else if (!event.shiftKey && document.activeElement === last) {
      event.preventDefault();
      first.focus();
    }
  };

  return (
    <div className="scrim" onKeyDown={onKeyDown}>
      <div
        ref={panel}
        className={wide ? "dialog wide" : "dialog"}
        role="dialog"
        aria-modal="true"
        aria-labelledby={titleId}
        tabIndex={-1}
      >
        <header className="dialog-head">
          <h2 id={titleId}>{title}</h2>
          {onClose && (
            <button type="button" className="ghost square" onClick={onClose} title={t("common.close")}>
              <Icon name="close" />
            </button>
          )}
        </header>
        <div className="dialog-body">{children}</div>
        <footer className="dialog-foot">{footer}</footer>
      </div>
    </div>
  );
}
