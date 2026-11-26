// A new archive: what goes into it, where it is written, and how it is
// compressed. Only the options the chosen format honours are shown.

import { useEffect, useState, type FormEvent } from "react";
import { api, toAppError } from "../api";
import { Dialog } from "../components/Dialog";
import { Icon } from "../components/Icon";
import { baseName, formatName, methodName, useFormat } from "../format";
import { useT } from "../i18n";
import {
  LEVELS,
  type AppError,
  type ArchiveFormat,
  type CreateChoices,
  type CreateForm,
  type FormatInfo,
  type Level,
  type Memory,
  type Method,
} from "../types";

const KIB = 1024;
/** Sizes of the dictionary on offer: every power of two from 64 KB to 1 GB. */
const DICTIONARIES = Array.from({ length: 15 }, (_, power) => 64 * KIB * 2 ** power);
/** How long the options rest before the memory they take is asked for, in milliseconds. */
const MEMORY_DELAY = 150;

/** `path` with the extension of `from` replaced by the one of `to`, when it ends with it. */
function withExtension(path: string, from: ArchiveFormat, to: ArchiveFormat): string {
  const suffix = `.${from}`;
  return path.toLowerCase().endsWith(suffix) ? `${path.slice(0, -suffix.length)}.${to}` : path;
}

export default function CreateDialog({
  formats,
  sources: given,
  choices,
  onStarted,
  onClose,
}: {
  formats: FormatInfo[];
  /** The files and folders the dialog starts with. */
  sources: string[];
  /** The choices of the last time. */
  choices: CreateChoices;
  /** The creation is queued; the choices are the ones to remember. */
  onStarted: (choices: CreateChoices) => void;
  onClose: () => void;
}) {
  const translator = useT();
  const { t, td } = translator;
  const format = useFormat();
  const writable = formats.filter((info) => info.canWrite);
  const [sources, setSources] = useState(given);
  const [formatId, setFormatId] = useState<ArchiveFormat>(
    writable.some((info) => info.id === choices.format) ? choices.format : (writable[0]?.id ?? "zip"),
  );
  const [destination, setDestination] = useState("");
  /** The destination was typed or picked: it is not replaced by a proposal any more. */
  const [own, setOwn] = useState(false);
  const [level, setLevel] = useState<Level>(choices.level);
  const [method, setMethod] = useState<Method | null>(null);
  const [dictionary, setDictionary] = useState<number | null>(null);
  const [solid, setSolid] = useState(true);
  const [threads, setThreads] = useState(0);
  const [volume, setVolume] = useState<number | null>(null);
  const [sfx, setSfx] = useState(false);
  const [password, setPassword] = useState("");
  const [again, setAgain] = useState("");
  const [saveName, setSaveName] = useState("");
  const [saved, setSaved] = useState<string[]>([]);
  const [encryptNames, setEncryptNames] = useState(false);
  const [comment, setComment] = useState("");
  const [memory, setMemory] = useState<Memory | null>(null);
  const [failure, setFailure] = useState<AppError | null>(null);
  const [busy, setBusy] = useState(false);

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

  const info = writable.find((candidate) => candidate.id === formatId);
  const used = method ?? info?.method ?? "store";
  const hasDictionary = info?.dictionary.includes(used) ?? false;
  const encrypts = (info?.encrypt ?? false) && password !== "";

  const form: CreateForm = {
    sources,
    destination,
    format: formatId,
    level,
    method: info && info.methods.length > 0 ? method : null,
    solid: info?.solid ? solid : true,
    dictionary: hasDictionary ? dictionary : null,
    comment: info?.comment && comment.trim() !== "" ? comment : null,
    password: encrypts ? password : null,
    encryptNames: encrypts && (info?.encryptNames ?? false) && encryptNames,
    threads,
    overwrite: false,
    volume: sfx ? null : volume,
    selfExtracting: sfx,
  };

  // The name the archive is offered, until the user gives one.
  useEffect(() => {
    if (own || sources.length === 0) {
      return;
    }
    let alive = true;
    api
      .createSuggest(sources, formatId)
      .then((path) => {
        if (alive && path !== null) {
          setDestination(path);
        }
      })
      .catch(() => undefined);
    return () => {
      alive = false;
    };
  }, [own, sources, formatId]);

  // The memory the options take, once they rest.
  const { format: memoryFormat, level: memoryLevel, method: memoryMethod, dictionary: memoryDictionary } = form;
  const memorySolid = form.solid;
  useEffect(() => {
    let alive = true;
    const timer = window.setTimeout(() => {
      const asked: CreateForm = {
        sources: [],
        destination: "",
        format: memoryFormat,
        level: memoryLevel,
        method: memoryMethod,
        solid: memorySolid,
        dictionary: memoryDictionary,
        comment: null,
        password: null,
        encryptNames: false,
        threads,
        overwrite: false,
        volume: null,
        selfExtracting: false,
      };
      api
        .createMemory(asked)
        .then((found) => {
          if (alive) {
            setMemory(found);
          }
        })
        .catch(() => undefined);
    }, MEMORY_DELAY);
    return () => {
      alive = false;
      window.clearTimeout(timer);
    };
  }, [memoryFormat, memoryLevel, memoryMethod, memorySolid, memoryDictionary, threads]);

  const changeFormat = (next: ArchiveFormat) => {
    if (own) {
      setDestination(withExtension(destination, formatId, next));
    }
    setFormatId(next);
    setMethod(null);
    setDictionary(null);
    if (next !== "zip" && next !== "7z") {
      setSfx(false);
    }
  };

  const add = async (folders: boolean) => {
    const chosen = await api.pickSources(folders);
    if (chosen.length > 0) {
      setSources([...sources, ...chosen.filter((path) => !sources.includes(path))]);
    }
  };

  const browse = async () => {
    const chosen = await api.pickSave(destination, formatId);
    if (chosen !== null) {
      setDestination(chosen);
      setOwn(true);
    }
  };

  const mismatch = encrypts && password !== again;
  const lonely = info?.singleFile === true && sources.length > 1;
  const ready = sources.length > 0 && destination.trim() !== "" && !mismatch && !lonely && !busy;

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    if (!ready) {
      return;
    }
    setBusy(true);
    setFailure(null);
    try {
      let overwrite = false;
      if ((await api.pathState(destination)) === "file") {
        overwrite = await api.confirm(t("create.replaceAsk", { path: destination }), t("create.replace"));
        if (!overwrite) {
          setBusy(false);
          return;
        }
      }
      if (saveName.trim() !== "" && password !== "") {
        await api.passwordSave(saveName.trim(), password);
      }
      await api.createStart({ ...form, overwrite });
      onStarted({ format: formatId, level });
    } catch (error) {
      setFailure(toAppError(error));
      setBusy(false);
    }
  };

  const cores = navigator.hardwareConcurrency || 1;

  return (
    <Dialog
      title={t("create.title")}
      onClose={onClose}
      wide
      footer={
        <>
          {memory && (
            <span className={memory.needed > memory.available ? "memory over" : "memory dim"}>
              {memory.needed > memory.available && <Icon name="alert" />}
              {t("create.memory", { needed: format.size(memory.needed), available: format.size(memory.available) })}
            </span>
          )}
          <span className="spacer" />
          <button type="button" onClick={onClose}>
            {t("common.cancel")}
          </button>
          <button type="submit" form="create" className="primary" disabled={!ready}>
            {t("create.start")}
          </button>
        </>
      }
    >
      <form id="create" className="form" onSubmit={(event) => void submit(event)}>
        <fieldset className="choices">
          <legend>{t("create.sources")}</legend>
          {sources.length === 0 ? (
            <p className="dim">{t("create.nothing")}</p>
          ) : (
            <ul className="sources">
              {sources.map((path) => (
                <li key={path}>
                  <span className="label" title={path}>
                    {baseName(path)}
                  </span>
                  <span className="dim label source-path">{path}</span>
                  <button
                    type="button"
                    className="ghost square"
                    onClick={() => setSources(sources.filter((other) => other !== path))}
                    title={t("create.remove")}
                  >
                    <Icon name="close" size={14} />
                  </button>
                </li>
              ))}
            </ul>
          )}
          <div className="buttons">
            <button type="button" onClick={() => void add(false)} data-autofocus>
              {t("create.addFiles")}
            </button>
            <button type="button" onClick={() => void add(true)}>
              {t("create.addFolders")}
            </button>
          </div>
        </fieldset>

        <label className="field">
          <span>{t("create.destination")}</span>
          <div className="with-button">
            <input
              type="text"
              value={destination}
              onChange={(event) => {
                setDestination(event.target.value);
                setOwn(true);
              }}
              spellCheck={false}
            />
            <button type="button" onClick={() => void browse()}>
              {t("common.browse")}
            </button>
          </div>
        </label>

        <div className="fields">
          <label className="field">
            <span>{t("create.format")}</span>
            <select value={formatId} onChange={(event) => changeFormat(event.target.value)}>
              {writable.map((candidate) => (
                <option key={candidate.id} value={candidate.id}>
                  {formatName(candidate.id)}
                </option>
              ))}
            </select>
          </label>
          {info?.levels && (
            <label className="field">
              <span>{t("create.level")}</span>
              <select value={level} onChange={(event) => setLevel(event.target.value as Level)}>
                {LEVELS.map((option) => (
                  <option key={option} value={option}>
                    {td(`create.levelKind.${option}`)}
                  </option>
                ))}
              </select>
            </label>
          )}
          {info && info.methods.length > 0 && (
            <label className="field">
              <span>{t("create.method")}</span>
              <select
                value={used}
                onChange={(event) => {
                  setMethod(event.target.value as Method);
                  setDictionary(null);
                }}
              >
                {info.methods.map((option) => (
                  <option key={option} value={option}>
                    {methodName(option)}
                  </option>
                ))}
              </select>
            </label>
          )}
          {hasDictionary && (
            <label className="field">
              <span>{t("create.dictionary")}</span>
              <select
                value={dictionary ?? 0}
                onChange={(event) => setDictionary(Number(event.target.value) || null)}
              >
                <option value={0}>{t("create.dictionaryAuto")}</option>
                {DICTIONARIES.map((size) => (
                  <option key={size} value={size}>
                    {format.size(size)}
                  </option>
                ))}
              </select>
            </label>
          )}
          <label className="field">
            <span>{t("create.threads")}</span>
            <select value={threads} onChange={(event) => setThreads(Number(event.target.value))}>
              <option value={0}>{t("create.threadsAuto", { count: cores })}</option>
              {Array.from({ length: cores }, (_, place) => place + 1).map((count) => (
                <option key={count} value={count}>
                  {count}
                </option>
              ))}
            </select>
          </label>
        </div>

        {(formatId === "zip" || formatId === "7z") && (
          <label className="check">
            <input
              type="checkbox"
              checked={sfx}
              onChange={(event) => {
                const on = event.target.checked;
                setSfx(on);
                if (on) {
                  setVolume(null);
                  setDestination((current) => (current === "" ? current : current.replace(/\.[^.\\/]+$/, ".exe")));
                }
              }}
            />
            <span>{t("create.sfx")}</span>
          </label>
        )}
        {!sfx && (
          <label className="field">
            <span>{t("create.volume")}</span>
            <select
              value={volume === null ? "none" : String(volume)}
              onChange={(event) => {
                const chosen = event.target.value;
                setVolume(chosen === "none" ? null : Number(chosen));
              }}
            >
              <option value="none">{t("create.volumeNone")}</option>
              <option value={10 * 1024 * 1024}>{t("create.volume10")}</option>
              <option value={100 * 1024 * 1024}>{t("create.volume100")}</option>
              <option value={700 * 1024 * 1024}>{t("create.volume700")}</option>
              <option value={4 * 1024 * 1024 * 1024}>{t("create.volume4g")}</option>
            </select>
          </label>
        )}

        {info?.solid && (
          <label className="check">
            <input type="checkbox" checked={solid} onChange={(event) => setSolid(event.target.checked)} />
            <span>{t("create.solid")}</span>
          </label>
        )}

        {info?.encrypt && (
          <fieldset className="choices">
            <legend>{t("create.protection")}</legend>
            <div className="fields">
              <label className="field">
                <span>{t("create.password")}</span>
                <input
                  type="password"
                  value={password}
                  onChange={(event) => setPassword(event.target.value)}
                  autoComplete="new-password"
                />
              </label>
              <div className="buttons">
                <button
                  type="button"
                  onClick={() =>
                    void api
                      .passwordGenerate(20, true)
                      .then((drawn) => {
                        setPassword(drawn);
                        setAgain(drawn);
                      })
                      .catch((error: unknown) => setFailure(toAppError(error)))
                  }
                >
                  {t("create.generate")}
                </button>
              </div>
              {saved.length > 0 && (
                <label className="field">
                  <span>{t("create.saved")}</span>
                  <select
                    value=""
                    onChange={(event) => {
                      const chosen = event.target.value;
                      if (chosen !== "") {
                        void api.passwordNamed(chosen).then((text) => {
                          if (text !== null) {
                            setPassword(text);
                            setAgain(text);
                          }
                        });
                      }
                    }}
                  >
                    <option value="">{t("create.savedNone")}</option>
                    {saved.map((name) => (
                      <option key={name} value={name}>
                        {name}
                      </option>
                    ))}
                  </select>
                </label>
              )}
              <label className="field">
                <span>{t("create.saveName")}</span>
                <input value={saveName} onChange={(event) => setSaveName(event.target.value)} autoComplete="off" />
              </label>
              <label className="field">
                <span>{t("create.passwordAgain")}</span>
                <input
                  type="password"
                  value={again}
                  onChange={(event) => setAgain(event.target.value)}
                  autoComplete="new-password"
                  disabled={password === ""}
                />
              </label>
            </div>
            {info.encryptNames && (
              <label className="check">
                <input
                  type="checkbox"
                  checked={encryptNames}
                  onChange={(event) => setEncryptNames(event.target.checked)}
                  disabled={password === ""}
                />
                <span>{t("create.encryptNames")}</span>
              </label>
            )}
          </fieldset>
        )}

        {info?.comment && (
          <label className="field">
            <span>{t("create.comment")}</span>
            <textarea value={comment} rows={2} onChange={(event) => setComment(event.target.value)} />
          </label>
        )}

        {mismatch && again !== "" && <p className="notice error">{translator.error({ code: "passwordMismatch" })}</p>}
        {lonely && <p className="notice error">{translator.error({ code: "singleFileOnly" })}</p>}
        {failure && <p className="notice error">{translator.error(failure)}</p>}
      </form>
    </Dialog>
  );
}
