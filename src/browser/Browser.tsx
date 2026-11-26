// The list of an open archive: one folder at a time, in the order the core
// sorted it. The core sends the numbers of the rows of a folder at once and
// the rows themselves as they come into view, so a folder of a million
// files costs the window what fits on the screen.

import {
  useDeferredValue,
  useEffect,
  useEffectEvent,
  useLayoutEffect,
  useMemo,
  useReducer,
  useRef,
  useState,
  type KeyboardEvent,
  type PointerEvent,
  type RefObject,
} from "react";
import { api, toAppError } from "../api";
import { Icon, type IconName } from "../components/Icon";
import { baseName, formatName, useFormat } from "../format";
import { useT } from "../i18n";
import type { AppError, EntryKind, Opened, Row, Sort, SortKey, Summary } from "../types";

/** Height of a row in pixels; the style sheet draws rows of the same height. */
const ROW = 28;
/** Rows asked for at once. */
const CHUNK = 128;
/** Rows drawn above and below what is seen, so a scroll has something to show. */
const OVERSCAN = 8;
/** The tallest element a browser scrolls reliably; a longer list is scrolled in proportion. */
const MAX_HEIGHT = 8_000_000;
/** Rows kept in memory; past that the ones out of view are let go. */
const ROWS_KEPT = 50_000;
/** A selection larger than this is counted, not measured. */
const SUMMARY_LIMIT = 200_000;
/** How far the pointer moves with the button down before it is a drag, in pixels. */
const DRAG_DISTANCE = 6;
/** How long the selection rests before its size is asked for, in milliseconds. */
const SUMMARY_DELAY = 150;

const ROOT = 0;

export interface Crumb {
  id: number;
  name: string;
}

/** What an operation started from the toolbar works on. */
export interface Picked {
  /** The folder the rows are shown in. */
  folder: Crumb;
  /** The rows chosen; none stands for the whole folder. */
  rows: number[];
  /** How many rows are chosen. */
  count: number;
}

interface Column {
  key: SortKey;
  label: "name" | "size" | "packed" | "modified";
  className: string;
  /** The order a first click on the column gives. */
  descending: boolean;
}

const COLUMNS: Column[] = [
  { key: "name", label: "name", className: "name", descending: false },
  { key: "size", label: "size", className: "num", descending: true },
  { key: "packed", label: "packed", className: "num packed", descending: true },
  { key: "modified", label: "modified", className: "date", descending: true },
];

function iconOf(kind: EntryKind): IconName {
  switch (kind) {
    case "directory":
      return "folder";
    case "file":
      return "file";
    case "symlink":
    case "hardlink":
      return "link";
    default: {
      const unknown: never = kind;
      throw new Error(`unknown entry kind: ${String(unknown)}`);
    }
  }
}

interface Press {
  index: number;
  x: number;
  y: number;
  pointer: number;
  dragging: boolean;
  released: boolean;
  /** The row was chosen among others before the press: a plain click leaves it alone. */
  collapse: boolean;
}

export function Browser({
  archive,
  encodings,
  ownDrag,
  onPick,
  onFocus,
  onActivate,
  onEncoding,
  onFailure,
}: {
  archive: Opened;
  /** The encodings offered for the names of an archive. */
  encodings: string[];
  /** Set while files dragged out of this window are in the air. */
  ownDrag: RefObject<boolean>;
  onPick: (picked: Picked) => void;
  /** The only row chosen, when it is a file: what the preview shows. */
  onFocus: (row: Row | null) => void;
  /** Enter or a double click on a file. */
  onActivate: (row: Row) => void;
  onEncoding: (encoding: string) => void;
  onFailure: (error: AppError) => void;
}) {
  const { t, td } = useT();
  const format = useFormat();
  const [trail, setTrail] = useState<Crumb[]>(() => [{ id: ROOT, name: baseName(archive.info.path) }]);
  const [sort, setSort] = useState<Sort>({ key: "name", descending: false });
  const [filter, setFilter] = useState("");
  const deferredFilter = useDeferredValue(filter);
  const [order, setOrder] = useState<Uint32Array | null>(null);
  const [selected, setSelected] = useState<ReadonlySet<number>>(() => new Set());
  const [cursorId, setCursorId] = useState<number | null>(null);
  const [anchorId, setAnchorId] = useState<number | null>(null);
  const [summary, setSummary] = useState<Summary | null>(null);
  const [preparing, setPreparing] = useState(false);
  const [scrollTop, setScrollTop] = useState(0);
  const [height, setHeight] = useState(0);
  const [, redraw] = useReducer((version: number) => version + 1, 0);

  const viewport = useRef<HTMLDivElement>(null);
  const cache = useRef(new Map<number, Row>());
  const pending = useRef(new Set<number>());
  const press = useRef<Press | null>(null);
  /** The cursor has moved by the keyboard or by a change of folder: bring it into view. */
  const reveal = useRef(false);

  // What the shell is told, as it wants to hear it at the time.
  const fail = useEffectEvent(onFailure);
  const pick = useEffectEvent(onPick);
  const focus = useEffectEvent(onFocus);

  const folder = trail[trail.length - 1];
  const count = order?.length ?? 0;
  const cursor = useMemo(
    () => (order && cursorId !== null ? order.indexOf(cursorId) : -1),
    [order, cursorId],
  );
  const whole = count > 0 && selected.size === count && deferredFilter.trim() === "";

  // The order of the folder, from the core.
  useEffect(() => {
    let alive = true;
    api
      .archiveFolder(archive.id, folder.id, sort, deferredFilter)
      .then((rows) => {
        if (alive) {
          setOrder(rows);
        }
      })
      .catch((error: unknown) => {
        if (alive) {
          setOrder(new Uint32Array());
          fail(toAppError(error));
        }
      });
    return () => {
      alive = false;
    };
  }, [archive.id, folder.id, sort, deferredFilter]);

  // The size of what is seen.
  useLayoutEffect(() => {
    const element = viewport.current;
    if (!element) {
      return;
    }
    setHeight(element.clientHeight);
    const observer = new ResizeObserver(() => setHeight(element.clientHeight));
    observer.observe(element);
    return () => observer.disconnect();
  }, []);

  // A list taller than a browser scrolls is scrolled in proportion.
  const full = count * ROW;
  const canvas = Math.min(full, MAX_HEIGHT);
  const ratio = full > canvas && canvas > height ? (full - height) / (canvas - height) : 1;
  const top = scrollTop * ratio;
  const first = Math.max(0, Math.floor(top / ROW) - OVERSCAN);
  const last = Math.min(count, Math.ceil((top + height) / ROW) + OVERSCAN);

  // The rows in view, from the core.
  useEffect(() => {
    if (!order || last <= first) {
      return;
    }
    for (let chunk = Math.floor(first / CHUNK); chunk * CHUNK < last; chunk += 1) {
      const wanted: number[] = [];
      for (const id of order.subarray(chunk * CHUNK, (chunk + 1) * CHUNK)) {
        if (!cache.current.has(id) && !pending.current.has(id)) {
          wanted.push(id);
        }
      }
      if (wanted.length === 0) {
        continue;
      }
      for (const id of wanted) {
        pending.current.add(id);
      }
      api
        .archiveRows(archive.id, wanted)
        .then((rows) => {
          if (cache.current.size + rows.length > ROWS_KEPT) {
            cache.current.clear();
          }
          for (const row of rows) {
            cache.current.set(row.id, row);
          }
          redraw();
        })
        .catch((error: unknown) => fail(toAppError(error)))
        .finally(() => {
          for (const id of wanted) {
            pending.current.delete(id);
          }
        });
    }
  }, [archive.id, order, first, last]);

  /** Scrolls as little as it takes to see the row. */
  const scrollTo = useEffectEvent((index: number) => {
    const element = viewport.current;
    if (!element) {
      return;
    }
    let wanted = top;
    if (index * ROW < top) {
      wanted = index * ROW;
    } else if ((index + 1) * ROW > top + height) {
      wanted = (index + 1) * ROW - height;
    }
    if (wanted !== top) {
      element.scrollTop = wanted / ratio;
    }
  });

  useEffect(() => {
    if (reveal.current && order && cursor >= 0) {
      reveal.current = false;
      scrollTo(cursor);
    }
  }, [order, cursor]);

  // What the toolbar works on.
  useEffect(() => {
    pick({ folder, rows: whole ? [] : [...selected], count: selected.size });
  }, [folder, selected, whole]);

  // What the preview shows.
  const only = selected.size === 1 ? cache.current.get(selected.values().next().value as number) : undefined;
  const focused = only && only.kind === "file" ? only : null;
  useEffect(() => {
    focus(focused);
  }, [focused]);

  // The size of the selection, once it rests.
  useEffect(() => {
    setSummary(null);
    if (selected.size === 0 || (!whole && selected.size > SUMMARY_LIMIT)) {
      return;
    }
    let alive = true;
    const timer = window.setTimeout(() => {
      api
        .archiveSummary(archive.id, whole ? [folder.id] : [...selected])
        .then((found) => {
          if (alive) {
            // A folder counts itself; here it stands for what is inside it.
            setSummary(whole ? { ...found, folders: Math.max(0, found.folders - 1) } : found);
          }
        })
        .catch(() => undefined);
    }, SUMMARY_DELAY);
    return () => {
      alive = false;
      window.clearTimeout(timer);
    };
  }, [archive.id, folder.id, selected, whole]);

  const leave = (next: Crumb[], back: number | null) => {
    setTrail(next);
    setFilter("");
    setOrder(null);
    setSelected(back === null ? new Set() : new Set([back]));
    setCursorId(back);
    setAnchorId(back);
    reveal.current = back !== null;
    if (viewport.current) {
      viewport.current.scrollTop = 0;
    }
    setScrollTop(0);
  };

  const enter = (row: Row) => leave([...trail, { id: row.id, name: row.name }], null);

  const goTo = (depth: number) => {
    if (depth < trail.length - 1) {
      leave(trail.slice(0, depth + 1), trail[depth + 1].id);
    }
  };

  const goUp = () => goTo(trail.length - 2);

  const choose = (index: number, how: { toggle?: boolean; range?: boolean }) => {
    if (!order || index < 0 || index >= order.length) {
      return;
    }
    const id = order[index];
    if (how.range) {
      const from = anchorId === null ? -1 : order.indexOf(anchorId);
      const start = from < 0 ? index : from;
      const span = order.subarray(Math.min(start, index), Math.max(start, index) + 1);
      setSelected(how.toggle ? new Set([...selected, ...span]) : new Set(span));
      if (from < 0) {
        setAnchorId(id);
      }
    } else if (how.toggle) {
      const next = new Set(selected);
      if (!next.delete(id)) {
        next.add(id);
      }
      setSelected(next);
      setAnchorId(id);
    } else {
      setSelected(new Set([id]));
      setAnchorId(id);
    }
    setCursorId(id);
  };

  const activate = (index: number) => {
    const row = order ? cache.current.get(order[index]) : undefined;
    if (!row) {
      return;
    }
    if (row.kind === "directory") {
      enter(row);
    } else {
      onActivate(row);
    }
  };

  const onKeyDown = (event: KeyboardEvent<HTMLDivElement>) => {
    if (!order) {
      return;
    }
    const page = Math.max(1, Math.floor(height / ROW) - 1);
    const move = (index: number) => {
      event.preventDefault();
      if (count === 0) {
        return;
      }
      reveal.current = true;
      choose(Math.min(count - 1, Math.max(0, index)), { range: event.shiftKey });
    };
    switch (event.key) {
      case "ArrowDown":
        move(cursor + 1);
        break;
      case "ArrowUp":
        if (event.altKey) {
          event.preventDefault();
          goUp();
        } else {
          move(cursor < 0 ? 0 : cursor - 1);
        }
        break;
      case "PageDown":
        move(cursor + page);
        break;
      case "PageUp":
        move(cursor - page);
        break;
      case "Home":
        move(0);
        break;
      case "End":
        move(count - 1);
        break;
      case "Enter":
        event.preventDefault();
        if (cursor >= 0) {
          activate(cursor);
        }
        break;
      case "Backspace":
        event.preventDefault();
        goUp();
        break;
      case " ":
        event.preventDefault();
        if (cursor >= 0) {
          choose(cursor, { toggle: true });
        }
        break;
      case "Escape":
        if (selected.size > 0) {
          event.preventDefault();
          setSelected(new Set());
        }
        break;
      default:
        // By the place of the key, so it works in any keyboard layout.
        if (event.code === "KeyA" && (event.ctrlKey || event.metaKey)) {
          event.preventDefault();
          setSelected(new Set(order));
        }
        break;
    }
  };

  /** A press on the empty part of the list lets the selection go. */
  const onBackgroundDown = (event: PointerEvent<HTMLDivElement>) => {
    const body = event.currentTarget;
    const onScrollbar = event.clientX >= body.getBoundingClientRect().left + body.clientWidth;
    if (event.button === 0 && !onScrollbar && !(event.target as HTMLElement).closest(".row")) {
      setSelected(new Set());
    }
  };

  const onPointerDown = (event: PointerEvent<HTMLDivElement>, index: number) => {
    if (event.button !== 0 || !order) {
      return;
    }
    const toggle = event.ctrlKey || event.metaKey;
    const already = selected.has(order[index]);
    if (toggle || event.shiftKey || !already) {
      choose(index, { toggle, range: event.shiftKey });
    }
    press.current = {
      index,
      x: event.clientX,
      y: event.clientY,
      pointer: event.pointerId,
      dragging: false,
      released: false,
      collapse: already && !toggle && !event.shiftKey,
    };
  };

  const onPointerMove = (event: PointerEvent<HTMLDivElement>) => {
    const pressed = press.current;
    if (!pressed || pressed.dragging || pressed.released || selected.size === 0) {
      return;
    }
    if (Math.hypot(event.clientX - pressed.x, event.clientY - pressed.y) < DRAG_DISTANCE) {
      return;
    }
    // The files leave the window: they are taken out of the archive into a
    // temporary folder, and what stands there is dragged. The pointer is
    // held so its release is seen wherever it happens: files must not fall
    // out of a hand that has already let go.
    pressed.dragging = true;
    pressed.collapse = false;
    event.currentTarget.setPointerCapture(pressed.pointer);
    setPreparing(true);
    api
      .dragOut(archive.id, folder.id, whole ? [] : [...selected])
      .then((paths) => {
        if (pressed.released || paths.length === 0) {
          return undefined;
        }
        ownDrag.current = true;
        return api.dragFiles(paths);
      })
      .catch((error: unknown) => {
        const failure = toAppError(error);
        if (failure.code !== "cancelled") {
          onFailure(failure);
        }
      })
      .finally(() => {
        ownDrag.current = false;
        setPreparing(false);
      });
  };

  const onPointerUp = () => {
    const pressed = press.current;
    if (!pressed) {
      return;
    }
    pressed.released = true;
    press.current = null;
    if (pressed.collapse) {
      choose(pressed.index, {});
    }
  };

  const sortBy = (column: Column) => {
    setSort(
      sort.key === column.key
        ? { key: column.key, descending: !sort.descending }
        : { key: column.key, descending: column.descending },
    );
  };

  const rows = [];
  if (order) {
    for (let index = first; index < last; index += 1) {
      const id = order[index];
      const row = cache.current.get(id);
      const classes = ["row"];
      if (selected.has(id)) {
        classes.push("selected");
      }
      if (index === cursor) {
        classes.push("cursor");
      }
      rows.push(
        <div
          key={id}
          role="row"
          aria-selected={selected.has(id)}
          className={classes.join(" ")}
          style={{ transform: `translateY(${index * ROW - top + scrollTop}px)` }}
          onPointerDown={(event) => onPointerDown(event, index)}
          onDoubleClick={() => activate(index)}
        >
          {row ? (
            <>
              <span className="cell name" role="gridcell">
                <span className={`kind ${row.kind}`}>
                  <Icon name={iconOf(row.kind)} />
                </span>
                <span className="label" title={row.name}>
                  {row.name}
                </span>
                {row.target !== undefined && <span className="target dim">→ {row.target}</span>}
                {row.encrypted && (
                  <span className="mark" title={t("browser.encrypted")}>
                    <Icon name="lock" size={12} />
                  </span>
                )}
              </span>
              <span className="cell num" role="gridcell">
                {row.size === undefined ? "" : format.size(row.size)}
              </span>
              <span className="cell num packed" role="gridcell">
                {row.packed === undefined || row.packed === 0 ? "" : format.size(row.packed)}
              </span>
              <span className="cell date" role="gridcell">
                {row.modified === undefined ? "" : format.date(row.modified)}
              </span>
            </>
          ) : (
            <span className="cell name" role="gridcell">
              <span className="skeleton" />
            </span>
          )}
        </div>,
      );
    }
  }

  const info = archive.info;
  return (
    <section className="browser">
      <div className="pathbar">
        <button
          type="button"
          className="ghost square"
          onClick={goUp}
          disabled={trail.length < 2}
          title={t("browser.up")}
        >
          <Icon name="up" />
        </button>
        <nav className="crumbs" aria-label={t("browser.path")}>
          {trail.map((crumb, depth) => (
            <span key={crumb.id} className="crumb">
              {depth > 0 && <Icon name="chevron" size={12} />}
              <button
                type="button"
                className={depth === trail.length - 1 ? "ghost here" : "ghost"}
                onClick={() => goTo(depth)}
                title={depth === 0 ? info.path : crumb.name}
              >
                {depth === 0 && <Icon name="box" />}
                <span>{crumb.name}</span>
              </button>
            </span>
          ))}
        </nav>
        <label className="filter">
          <Icon name="search" size={14} />
          <input
            type="search"
            value={filter}
            placeholder={t("browser.filter")}
            onChange={(event) => {
              setFilter(event.target.value);
              setSelected(new Set());
              setCursorId(null);
              setAnchorId(null);
            }}
            spellCheck={false}
          />
        </label>
      </div>

      <div className="list" role="grid" aria-rowcount={count} aria-multiselectable="true">
        <div className="list-head" role="row">
          {COLUMNS.map((column) => (
            <button
              key={column.key}
              type="button"
              role="columnheader"
              aria-sort={sort.key !== column.key ? "none" : sort.descending ? "descending" : "ascending"}
              className={`cell ${column.className}`}
              onClick={() => sortBy(column)}
            >
              <span>{td(`browser.column.${column.label}`)}</span>
              {sort.key === column.key && <Icon name={sort.descending ? "sortDown" : "sortUp"} size={12} />}
            </button>
          ))}
        </div>
        <div
          ref={viewport}
          className={preparing ? "list-body preparing" : "list-body"}
          tabIndex={0}
          onScroll={(event) => setScrollTop(event.currentTarget.scrollTop)}
          onKeyDown={onKeyDown}
          onPointerDown={onBackgroundDown}
          onPointerMove={onPointerMove}
          onPointerUp={onPointerUp}
          onPointerCancel={onPointerUp}
        >
          <div className="list-canvas" style={{ height: canvas }}>
            {rows}
          </div>
          {order === null && <p className="list-note dim">{t("browser.loading")}</p>}
          {order !== null && count === 0 && (
            <p className="list-note dim">
              {deferredFilter.trim() === "" ? t("browser.empty") : t("browser.nothingFound")}
            </p>
          )}
        </div>
      </div>

      <footer className="statusbar">
        <span className="status-main">
          {preparing
            ? t("browser.preparingDrag")
            : selected.size === 0
              ? t("browser.items", { count: format.count(count) })
              : summary
                ? t("browser.selectedSize", { count: format.count(selected.size), size: format.size(summary.bytes) })
                : t("browser.selected", { count: format.count(selected.size) })}
        </span>
        <span className="status-archive dim" title={info.comment}>
          {t("browser.archive", {
            format: formatName(info.format),
            files: format.count(info.files),
            size: format.size(info.bytes),
            packed: format.size(info.size),
            ratio: format.ratio(info.size, info.bytes),
          })}
          {info.solid && ` · ${t("browser.solid")}`}
          {info.encrypted && ` · ${t("browser.encrypted")}`}
        </span>
        <label className="status-encoding">
          <span className="dim">{t("browser.encoding")}</span>
          <select value={archive.encoding} onChange={(event) => onEncoding(event.target.value)}>
            <option value="auto">{t("browser.encodingAuto")}</option>
            {encodings.map((label) => (
              <option key={label} value={label}>
                {label.toUpperCase()}
              </option>
            ))}
          </select>
        </label>
      </footer>
    </section>
  );
}
