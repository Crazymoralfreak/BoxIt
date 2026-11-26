// The shell: boot, the theme and the language, the open archive, the queue
// of operations, the dialogs, and everything that arrives from outside the
// window (drops, a second start).

import { getCurrentWebview } from "@tauri-apps/api/webview";
import { lazy, Suspense, useEffect, useEffectEvent, useRef, useState } from "react";
import { api, toAppError } from "./api";
import { useAppEvent } from "./api/events";
import { Browser, type Picked } from "./browser/Browser";
import { Icon, type IconName } from "./components/Icon";
import { baseName } from "./format";
import { I18nProvider, useT } from "./i18n";
import { isFinished, progressOf } from "./jobs/state";
import { Welcome } from "./pages/Welcome";
import type {
  AppError,
  AppInfo,
  CreateChoices,
  ExtractChoices,
  FormatInfo,
  JobInfo,
  Opened,
  Reply,
  Row,
  Settings,
  Theme,
} from "./types";

// What is not on the screen at the start is loaded when it is first shown.
const PreviewPane = lazy(() => import("./preview/PreviewPane"));
const JobsPanel = lazy(() => import("./jobs/JobsPanel"));
const QuestionDialog = lazy(() => import("./jobs/QuestionDialog"));
const ExtractDialog = lazy(() => import("./dialogs/ExtractDialog"));
const CreateDialog = lazy(() => import("./dialogs/CreateDialog"));
const RepairDialog = lazy(() => import("./dialogs/RepairDialog"));
const SettingsDialog = lazy(() => import("./dialogs/SettingsDialog"));
const PasswordDialog = lazy(() => import("./dialogs/PasswordDialog"));

/** How long a message about a failure stays, in milliseconds. */
const TOAST_TIME = 8000;

/** A part of the path of the folders BoxIt drags its own files out of. */
const OWN_DRAG = /[\\/]cache[\\/]temp[\\/]drag-/;

interface Boot {
  info: AppInfo;
  settings: Settings;
  formats: FormatInfo[];
  encodings: string[];
}

/** The reading of the list of an archive that is under way. */
interface OpeningState {
  token: number;
  path: string;
  entries: number;
}

/** An archive whose list needs a password. */
interface Locked {
  path: string;
  encoding: string | null;
  /** A password was given and did not fit. */
  wrong: boolean;
}

type DialogState =
  | { kind: "extract"; picked: Picked }
  | { kind: "create"; sources: string[] }
  | { kind: "repair" }
  | { kind: "settings" }
  | { kind: "previewPassword" };

/** Applies the theme of the settings; "system" leaves the choice to the style sheet. */
function applyTheme(theme: Theme) {
  const root = document.documentElement;
  switch (theme) {
    case "system":
      delete root.dataset.theme;
      break;
    case "light":
    case "dark":
      root.dataset.theme = theme;
      break;
    default: {
      const unknown: never = theme;
      throw new Error(`unknown theme: ${String(unknown)}`);
    }
  }
}

/** Asks the core to show the window once the frame with the content is on the screen. */
function showWindowAfterPaint() {
  requestAnimationFrame(() => {
    requestAnimationFrame(() => {
      void api.windowShow();
    });
  });
}

/** The jobs with `job` put in its place: they stay in the order they were added. */
function withJob(jobs: JobInfo[], job: JobInfo): JobInfo[] {
  const place = jobs.findIndex((known) => known.id === job.id);
  if (place < 0) {
    return [...jobs, job].sort((one, other) => one.id - other.id);
  }
  const next = [...jobs];
  next[place] = job;
  return next;
}

function ToolButton({
  icon,
  label,
  shortcut,
  disabled = false,
  active = false,
  onClick,
}: {
  icon: IconName;
  label: string;
  shortcut?: string;
  disabled?: boolean;
  active?: boolean;
  onClick: () => void;
}) {
  return (
    <button
      type="button"
      className={active ? "tool active" : "tool"}
      disabled={disabled}
      aria-pressed={active || undefined}
      title={shortcut ? `${label} (${shortcut})` : label}
      onClick={onClick}
    >
      <Icon name={icon} size={18} />
      <span>{label}</span>
    </button>
  );
}

/** The start failed: there is nothing to show but the reason. */
function Failure({ error }: { error: AppError }) {
  const translator = useT();
  return (
    <main className="failure">
      <div className="notice error">{translator.error(error)}</div>
    </main>
  );
}

function Shell({
  boot,
  problem,
  onSettings,
  onRecent,
}: {
  boot: Boot;
  /** The settings could not be stored. */
  problem: AppError | null;
  onSettings: (settings: Settings) => void;
  /** An archive was opened: the list of the recent ones has changed. */
  onRecent: () => void;
}) {
  const translator = useT();
  const { t } = translator;
  const { info, settings } = boot;
  const [archive, setArchive] = useState<Opened | null>(null);
  const [opening, setOpening] = useState<OpeningState | null>(null);
  const [locked, setLocked] = useState<Locked | null>(null);
  const [dialog, setDialog] = useState<DialogState | null>(null);
  const [jobs, setJobs] = useState<JobInfo[]>([]);
  const [queueOpen, setQueueOpen] = useState(false);
  const [focused, setFocused] = useState<Row | null>(null);
  const [previewVersion, setPreviewVersion] = useState(0);
  const [answered, setAnswered] = useState<number | null>(null);
  const [dropping, setDropping] = useState(false);
  const [failure, setFailure] = useState<AppError | null>(null);

  const tokens = useRef(0);
  /** The open archive as of now, for what finishes after the frame it started in. */
  const open = useRef<Opened | null>(null);
  const picked = useRef<Picked | null>(null);
  const ownDrag = useRef(false);

  const fail = (error: AppError) => {
    if (error.code !== "cancelled") {
      setFailure(error);
    }
  };

  useEffect(() => {
    if (problem) {
      setFailure(problem);
    }
  }, [problem]);

  useEffect(() => {
    if (!failure) {
      return;
    }
    const timer = window.setTimeout(() => setFailure(null), TOAST_TIME);
    return () => window.clearTimeout(timer);
  }, [failure]);

  const show = (next: Opened | null) => {
    if (open.current) {
      void api.archiveClose(open.current.id);
    }
    open.current = next;
    picked.current = null;
    setFocused(null);
    setArchive(next);
  };

  const closeArchive = () => show(null);

  const openArchive = async (
    path: string,
    password: string | null = null,
    encoding: string | null = null,
    remember = false,
  ) => {
    const token = ++tokens.current;
    setLocked(null);
    setOpening({ token, path, entries: 0 });
    let secret = password;
    let recalled = false;
    if (secret === null) {
      try {
        secret = await api.passwordRecall(path);
        recalled = secret !== null;
      } catch {
        secret = null;
      }
    }
    try {
      const opened = await api.archiveOpen(path, token, secret, encoding);
      if (remember && password !== null) {
        void api.passwordRemember(path, password).catch((error: unknown) => fail(toAppError(error)));
      }
      if (tokens.current !== token) {
        // Another archive was asked for meanwhile.
        void api.archiveClose(opened.id);
        return;
      }
      show(opened);
      onRecent();
    } catch (raised) {
      const error = toAppError(raised);
      if (tokens.current !== token) {
        return;
      }
      if (error.code === "passwordRequired" || error.code === "wrongPassword") {
        if (recalled) {
          void api.passwordForgetArchive(path).catch(() => undefined);
        }
        setLocked({ path, encoding, wrong: password !== null });
      } else {
        fail(error);
      }
    } finally {
      setOpening((current) => (current?.token === token ? null : current));
    }
  };

  const cancelOpening = () => {
    if (opening) {
      void api.archiveCancelOpen(opening.token);
    }
  };

  /** One archive is opened; anything else is what a new archive is made of. */
  const openPaths = async (paths: string[]) => {
    if (paths.length === 0) {
      return;
    }
    try {
      if (paths.length === 1 && (await api.recognize(paths[0])) !== null) {
        await openArchive(paths[0]);
      } else {
        setDialog({ kind: "create", sources: paths });
      }
    } catch (error) {
      fail(toAppError(error));
    }
  };

  const pickArchive = async () => {
    try {
      const path = await api.pickArchive();
      if (path !== null) {
        await openArchive(path);
      }
    } catch (error) {
      fail(toAppError(error));
    }
  };

  const startExtract = () => {
    if (archive && picked.current) {
      setDialog({ kind: "extract", picked: picked.current });
    }
  };

  const startTest = () => {
    if (!archive || !picked.current) {
      return;
    }
    api
      .testStart(archive.id, picked.current.folder.id, picked.current.rows)
      .then(() => setQueueOpen(true))
      .catch((error: unknown) => fail(toAppError(error)));
  };

  const togglePreview = () => onSettings({ ...settings, preview: !settings.preview });

  const refreshJobs = () => {
    api
      .jobsList()
      .then(setJobs)
      .catch((error: unknown) => fail(toAppError(error)));
  };

  // What was there before the window: the jobs, and what BoxIt was started with.
  const start = useEffectEvent(() => {
    refreshJobs();
    api
      .openedFiles()
      .then(openPaths)
      .catch((error: unknown) => fail(toAppError(error)));
  });
  useEffect(() => start(), []);

  useAppEvent("jobs:changed", (job) => setJobs((current) => withJob(current, job)));
  useAppEvent("archive:opening", (progress) =>
    setOpening((current) => (current?.token === progress.token ? { ...current, entries: progress.entries } : current)),
  );
  useAppEvent("open:files", (paths) => void openPaths(paths));

  // Files and folders dropped on the window.
  const onDrop = useEffectEvent((paths: string[]) => {
    setDropping(false);
    const foreign = paths.filter((path) => !OWN_DRAG.test(path));
    if (!ownDrag.current && dialog === null && foreign.length > 0) {
      void openPaths(foreign);
    }
  });
  const onDragEnter = useEffectEvent((paths: string[]) => {
    setDropping(!ownDrag.current && dialog === null && paths.some((path) => !OWN_DRAG.test(path)));
  });
  useEffect(() => {
    let cancelled = false;
    let unlisten: (() => void) | undefined;
    void getCurrentWebview()
      .onDragDropEvent((event) => {
        const payload = event.payload;
        switch (payload.type) {
          case "enter":
            onDragEnter(payload.paths);
            break;
          case "over":
            break;
          case "drop":
            onDrop(payload.paths);
            break;
          case "leave":
            setDropping(false);
            break;
          default: {
            const unknown: never = payload;
            throw new Error(`unknown drop event: ${JSON.stringify(unknown)}`);
          }
        }
      })
      .then((stop) => {
        if (cancelled) {
          stop();
        } else {
          unlisten = stop;
        }
      });
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, []);

  // The shortcuts of the window, by the place of the key so they work in any layout.
  const onShortcut = useEffectEvent((event: KeyboardEvent) => {
    if (!(event.ctrlKey || event.metaKey) || event.altKey || dialog !== null || locked !== null) {
      return;
    }
    const actions: Record<string, (() => void) | undefined> = {
      KeyO: () => void pickArchive(),
      KeyN: () => setDialog({ kind: "create", sources: [] }),
      KeyE: startExtract,
      KeyT: startTest,
      KeyW: closeArchive,
      // Without an archive the key does nothing, rather than what the web view does with it.
      KeyP: () => {
        if (archive) {
          togglePreview();
        }
      },
      KeyJ: () => setQueueOpen((open) => !open),
      Comma: () => setDialog({ kind: "settings" }),
    };
    const action = actions[event.code];
    if (action) {
      event.preventDefault();
      action();
    }
  });
  useEffect(() => {
    const listener = (event: KeyboardEvent) => onShortcut(event);
    window.addEventListener("keydown", listener);
    return () => window.removeEventListener("keydown", listener);
  }, []);

  const asking = jobs.find((job) => job.question !== null && job.question.id !== answered);
  const reply = (job: JobInfo, question: number, answer: Reply) => {
    setAnswered(question);
    api.jobAnswer(job.id, question, answer).catch((error: unknown) => fail(toAppError(error)));
  };

  const active = jobs.filter((job) => !isFinished(job.state));
  const running = active.find((job) => job.state === "running" || job.state === "paused");
  const runningProgress = running ? progressOf(running) : null;

  const reveal = (path: string) => {
    api.revealPath(path).catch((error: unknown) => fail(toAppError(error)));
  };

  const extractStarted = (choices: ExtractChoices) => {
    setDialog(null);
    setQueueOpen(true);
    onSettings({ ...settings, extract: choices });
  };

  const createStarted = (choices: CreateChoices) => {
    setDialog(null);
    setQueueOpen(true);
    // The dialog may have stored a password name after this render, so the
    // choices are written onto the settings the core has now.
    void api
      .settingsGet()
      .then((latest) => onSettings({ ...latest, create: choices }))
      .catch((error: unknown) => fail(toAppError(error)));
  };

  const givePreviewPassword = (password: string, remember = false) => {
    if (!archive) {
      return;
    }
    if (remember) {
      void api.passwordRemember(archive.info.path, password).catch((error: unknown) => fail(toAppError(error)));
    }
    setDialog(null);
    api
      .archiveSetPassword(archive.id, password)
      .then(() => setPreviewVersion((version) => version + 1))
      .catch((error: unknown) => fail(toAppError(error)));
  };

  return (
    <div className="app">
      <header className="toolbar">
        <span className="brand" aria-hidden="true">
          BoxIt
        </span>
        <ToolButton icon="open" label={t("toolbar.open")} shortcut="Ctrl+O" onClick={() => void pickArchive()} />
        <ToolButton
          icon="box"
          label={t("toolbar.create")}
          shortcut="Ctrl+N"
          onClick={() => setDialog({ kind: "create", sources: [] })}
        />
        <span className="divider" />
        <ToolButton
          icon="extract"
          label={t("toolbar.extract")}
          shortcut="Ctrl+E"
          disabled={!archive}
          onClick={startExtract}
        />
        <ToolButton icon="test" label={t("toolbar.test")} shortcut="Ctrl+T" disabled={!archive} onClick={startTest} />
        <ToolButton
          icon="repair"
          label={t("toolbar.repair")}
          disabled={!archive}
          onClick={() => archive && setDialog({ kind: "repair" })}
        />
        <span className="spacer" />
        <ToolButton
          icon="preview"
          label={t("toolbar.preview")}
          shortcut="Ctrl+P"
          disabled={!archive}
          active={archive !== null && settings.preview}
          onClick={togglePreview}
        />
        <button
          type="button"
          className={queueOpen ? "tool active" : "tool"}
          aria-pressed={queueOpen || undefined}
          title={`${t("toolbar.queue")} (Ctrl+J)`}
          onClick={() => setQueueOpen(!queueOpen)}
        >
          <Icon name="queue" size={18} />
          <span>{t("toolbar.queue")}</span>
          {active.length > 0 && <span className="badge">{active.length}</span>}
          {running && (
            <span className={runningProgress === null ? "tool-bar unknown" : "tool-bar"}>
              <span style={{ transform: `scaleX(${runningProgress ?? 0})` }} />
            </span>
          )}
        </button>
        <ToolButton
          icon="settings"
          label={t("toolbar.settings")}
          shortcut="Ctrl+,"
          onClick={() => setDialog({ kind: "settings" })}
        />
      </header>

      <main className="stage">
        {archive ? (
          <>
            <Browser
              key={archive.id}
              archive={archive}
              encodings={boot.encodings}
              ownDrag={ownDrag}
              onPick={(next) => {
                picked.current = next;
              }}
              onFocus={setFocused}
              onActivate={() => {
                if (!settings.preview) {
                  togglePreview();
                }
              }}
              onEncoding={(encoding) => void openArchive(archive.info.path, null, encoding)}
              onFailure={fail}
            />
            {settings.preview && (
              <Suspense fallback={<aside className="preview" />}>
                <PreviewPane
                  session={archive.id}
                  row={focused}
                  version={previewVersion}
                  onPassword={() => setDialog({ kind: "previewPassword" })}
                />
              </Suspense>
            )}
          </>
        ) : (
          <Welcome
            recent={settings.recent}
            onOpen={() => void pickArchive()}
            onOpenPath={(path) => void openArchive(path)}
            onCreate={() => setDialog({ kind: "create", sources: [] })}
            onForget={() => onSettings({ ...settings, recent: [] })}
          />
        )}
      </main>

      {queueOpen && (
        <Suspense fallback={null}>
          <JobsPanel jobs={jobs} onClose={() => setQueueOpen(false)} onCleared={refreshJobs} onReveal={reveal} />
        </Suspense>
      )}

      {dropping && (
        <div className="drop-veil">
          <p>
            <Icon name="box" size={28} />
            {t("drop.hint")}
          </p>
        </div>
      )}

      {opening && (
        <div className="scrim">
          <div className="dialog opening" role="dialog" aria-modal="true" aria-label={t("opening.title")}>
            <div className="dialog-body">
              <p className="opening-title">{t("opening.title")}</p>
              <p className="dim label" title={opening.path}>
                {baseName(opening.path)}
              </p>
              <div className="bar unknown">
                <span />
              </div>
              <p className="dim">{t("opening.entries", { count: opening.entries })}</p>
            </div>
            <footer className="dialog-foot">
              <button type="button" onClick={cancelOpening}>
                {t("common.cancel")}
              </button>
            </footer>
          </div>
        </div>
      )}

      <Suspense fallback={null}>
        {locked && (
          <PasswordDialog
            key={`${locked.path}:${String(locked.wrong)}`}
            name={baseName(locked.path)}
            wrong={locked.wrong}
            onSubmit={(password, remember) => void openArchive(locked.path, password, locked.encoding, remember)}
            onClose={() => setLocked(null)}
          />
        )}
        {dialog?.kind === "extract" && archive && (
          <ExtractDialog
            archive={archive}
            picked={dialog.picked}
            choices={settings.extract}
            onStarted={extractStarted}
            onClose={() => setDialog(null)}
          />
        )}
        {dialog?.kind === "create" && (
          <CreateDialog
            formats={boot.formats}
            sources={dialog.sources}
            choices={settings.create}
            onStarted={createStarted}
            onClose={() => setDialog(null)}
          />
        )}
        {dialog?.kind === "repair" && archive && (
          <RepairDialog
            id={archive.id}
            onStarted={() => {
              setDialog(null);
              setQueueOpen(true);
            }}
            onClose={() => setDialog(null)}
          />
        )}
        {dialog?.kind === "settings" && (
          <SettingsDialog
            info={info}
            settings={settings}
            onChange={onSettings}
            onFailure={fail}
            onClose={() => setDialog(null)}
          />
        )}
        {dialog?.kind === "previewPassword" && archive && (
          <PasswordDialog
            name={focused?.name ?? baseName(archive.info.path)}
            wrong={false}
            onSubmit={givePreviewPassword}
            onClose={() => setDialog(null)}
          />
        )}
        {asking?.question && (
          <QuestionDialog
            key={asking.question.id}
            question={asking.question}
            onReply={(answer) => reply(asking, asking.question?.id ?? 0, answer)}
          />
        )}
      </Suspense>

      {failure && (
        <div className="toast notice error" role="alert">
          <span>{translator.error(failure)}</span>
          <button type="button" className="ghost square" onClick={() => setFailure(null)} title={t("common.close")}>
            <Icon name="close" size={14} />
          </button>
        </div>
      )}
    </div>
  );
}

export default function App() {
  const [boot, setBoot] = useState<Boot | null>(null);
  const [failure, setFailure] = useState<AppError | null>(null);
  const [problem, setProblem] = useState<AppError | null>(null);

  useEffect(() => {
    let alive = true;
    Promise.all([api.appInfo(), api.settingsGet(), api.formats(), api.nameEncodings()])
      .then(([info, settings, formats, encodings]) => {
        if (alive) {
          applyTheme(settings.theme);
          setBoot({ info, settings, formats, encodings });
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
  }, []);

  const ready = boot !== null || failure !== null;
  useEffect(() => {
    if (ready) {
      showWindowAfterPaint();
    }
  }, [ready]);

  const stored = (settings: Settings) => {
    applyTheme(settings.theme);
    setBoot((current) => (current ? { ...current, settings } : current));
  };

  const changeSettings = (settings: Settings) => {
    // The interface follows the choice at once; the core answers with what it stored.
    stored(settings);
    api
      .settingsSet(settings)
      .then(stored)
      .catch((error: unknown) => setProblem(toAppError(error)));
  };

  const refreshSettings = () => {
    api
      .settingsGet()
      .then(stored)
      .catch((error: unknown) => setProblem(toAppError(error)));
  };

  if (!boot) {
    return failure ? (
      <I18nProvider language="en">
        <Failure error={failure} />
      </I18nProvider>
    ) : null;
  }

  const language = boot.settings.language || boot.info.systemLanguage;
  return (
    <I18nProvider language={language}>
      <Shell boot={boot} problem={problem} onSettings={changeSettings} onRecent={refreshSettings} />
    </I18nProvider>
  );
}
