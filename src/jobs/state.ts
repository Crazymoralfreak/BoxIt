// What the shell and the queue both ask of a job.

import type { JobInfo, JobState } from "../types";

/** A job that has nothing more to do. */
export function isFinished(state: JobState): boolean {
  switch (state) {
    case "done":
    case "failed":
    case "cancelled":
      return true;
    case "queued":
    case "running":
    case "paused":
      return false;
    default: {
      const unknown: never = state;
      throw new Error(`unknown job state: ${String(unknown)}`);
    }
  }
}

/** The part of the work done, from 0 to 1; `null` when the amount of work is not known. */
export function progressOf(job: JobInfo): number | null {
  if (job.bytesTotal !== null && job.bytesTotal > 0) {
    return Math.min(1, job.bytesDone / job.bytesTotal);
  }
  if (job.entriesTotal !== null && job.entriesTotal > 0) {
    return Math.min(1, job.entriesDone / job.entriesTotal);
  }
  return null;
}
