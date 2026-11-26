// The queue of operations: what runs, what waits its turn, what is done.

import { api } from "../api";
import { Icon } from "../components/Icon";
import { baseName, useFormat, type Formatter } from "../format";
import { useT, type Translator } from "../i18n";
import type { CreateResult, ExtractResult, JobInfo, RepairResult, TestResult } from "../types";
import { isFinished, progressOf } from "./state";

/** What a finished job did, in a line. */
function outcome(job: JobInfo, { t }: Translator, format: Formatter): string {
  switch (job.kind) {
    case "extract": {
      const result = job.result as ExtractResult;
      const done = t("jobs.extracted", {
        files: format.count(result.files),
        folders: format.count(result.folders),
        size: format.size(result.bytes),
      });
      return result.skippedTotal > 0
        ? `${done} · ${t("jobs.skipped", { count: format.count(result.skippedTotal) })}`
        : done;
    }
    case "test": {
      const result = job.result as TestResult;
      return t("jobs.tested", { files: format.count(result.files), size: format.size(result.bytes) });
    }
    case "create": {
      const result = job.result as CreateResult;
      return t("jobs.created", {
        files: format.count(result.files),
        size: format.size(result.bytes),
        packed: format.size(result.packed),
        ratio: format.ratio(result.packed, result.bytes),
      });
    }
    case "repair": {
      const result = job.result as RepairResult;
      const done = t("jobs.repaired", {
        files: format.count(result.files),
        folders: format.count(result.folders),
        size: format.size(result.bytes),
      });
      return result.lostTotal > 0
        ? `${done} · ${t("jobs.repairedLost", { count: format.count(result.lostTotal) })}`
        : done;
    }
    default: {
      const unknown: never = job.kind;
      throw new Error(`unknown job kind: ${String(unknown)}`);
    }
  }
}

/** Where the result of a finished job is, to be shown in the file manager. */
function placeOf(job: JobInfo): string | null {
  if (job.state !== "done") {
    return null;
  }
  switch (job.kind) {
    case "extract":
      return (job.result as ExtractResult).destination;
    case "create":
      return (job.result as CreateResult).path;
    case "repair":
      return (job.result as RepairResult).path;
    case "test":
      return null;
    default: {
      const unknown: never = job.kind;
      throw new Error(`unknown job kind: ${String(unknown)}`);
    }
  }
}

function JobLine({ job, onReveal }: { job: JobInfo; onReveal: (path: string) => void }) {
  const translator = useT();
  const { t, td } = translator;
  const format = useFormat();
  const progress = progressOf(job);
  const finished = isFinished(job.state);
  const place = placeOf(job);
  const skipped = job.state === "done" && job.kind === "extract" ? (job.result as ExtractResult).skipped : [];

  let detail: string;
  if (job.state === "done") {
    detail = outcome(job, translator, format);
  } else if (job.state === "failed" && job.error) {
    detail = translator.error(job.error);
  } else if (job.state === "running" || job.state === "paused") {
    const parts = [
      job.bytesTotal === null
        ? format.size(job.bytesDone)
        : t("jobs.progress", { done: format.size(job.bytesDone), total: format.size(job.bytesTotal) }),
    ];
    if (job.state === "running" && job.speed !== null) {
      parts.push(t("jobs.speed", { speed: format.size(job.speed) }));
    }
    if (job.state === "running" && job.etaSeconds !== null) {
      parts.push(t("jobs.eta", { time: format.duration(job.etaSeconds) }));
    }
    detail = parts.join(" · ");
  } else {
    detail = "";
  }

  return (
    <li className={`job ${job.state}`}>
      <div className="job-head">
        <span className="job-kind">{td(`jobs.kind.${job.kind}`)}</span>
        <span className="job-name label" title={job.archive}>
          {baseName(job.archive)}
        </span>
        <span className="job-state dim">{td(`jobs.state.${job.state}`)}</span>
        <span className="job-actions">
          {(job.state === "running" || job.state === "paused") && (
            <button
              type="button"
              className="ghost square"
              onClick={() => void api.jobPause(job.id, job.state === "running")}
              title={job.state === "running" ? t("jobs.pause") : t("jobs.resume")}
            >
              <Icon name={job.state === "running" ? "pause" : "play"} />
            </button>
          )}
          {!finished && (
            <button
              type="button"
              className="ghost square"
              onClick={() => void api.jobCancel(job.id)}
              title={t("jobs.cancel")}
            >
              <Icon name="close" />
            </button>
          )}
          {place !== null && (
            <button type="button" className="ghost square" onClick={() => onReveal(place)} title={t("jobs.reveal")}>
              <Icon name="reveal" />
            </button>
          )}
        </span>
      </div>
      {!finished && (
        <div
          className={progress === null && job.state === "running" ? "bar unknown" : "bar"}
          role="progressbar"
          aria-valuemin={0}
          aria-valuemax={100}
          aria-valuenow={progress === null ? undefined : Math.round(progress * 100)}
        >
          <span style={{ transform: `scaleX(${progress ?? 0})` }} />
        </div>
      )}
      {detail !== "" && <p className="job-detail dim">{detail}</p>}
      {!finished && job.current !== "" && (
        <p className="job-current dim label" title={job.current}>
          {job.current}
        </p>
      )}
      {skipped.length > 0 && (
        <details className="job-skipped">
          <summary>{t("jobs.skippedList")}</summary>
          <ul>
            {skipped.map((entry, place) => (
              <li key={place}>
                <span className="label">{entry.path}</span>
                <span className="dim">{translator.error(entry.reason)}</span>
              </li>
            ))}
          </ul>
        </details>
      )}
    </li>
  );
}

export default function JobsPanel({
  jobs,
  onClose,
  onCleared,
  onReveal,
}: {
  /** The jobs in the order they were added. */
  jobs: JobInfo[];
  onClose: () => void;
  /** The finished jobs were forgotten: the list is to be read again. */
  onCleared: () => void;
  onReveal: (path: string) => void;
}) {
  const { t } = useT();
  const anyFinished = jobs.some((job) => isFinished(job.state));
  return (
    <section className="jobs" aria-label={t("jobs.title")}>
      <header className="jobs-head">
        <h2>{t("jobs.title")}</h2>
        <button
          type="button"
          className="ghost"
          disabled={!anyFinished}
          onClick={() => void api.jobsClear().then(onCleared)}
        >
          {t("jobs.clear")}
        </button>
        <button type="button" className="ghost square" onClick={onClose} title={t("common.close")}>
          <Icon name="close" />
        </button>
      </header>
      {jobs.length === 0 ? (
        <p className="jobs-empty dim">{t("jobs.empty")}</p>
      ) : (
        <ul className="jobs-list">
          {/* The latest on top: that is the one just started. */}
          {[...jobs].reverse().map((job) => (
            <JobLine key={job.id} job={job} onReveal={onReveal} />
          ))}
        </ul>
      )}
    </section>
  );
}
