// Typed wrappers over the Rust commands. Every call rejects with an `AppError`.

import { Channel, convertFileSrc, invoke } from "@tauri-apps/api/core";
import type {
  AppError,
  AppInfo,
  ArchiveFormat,
  CreateForm,
  ExtractOptions,
  ExtractPlace,
  FormatInfo,
  JobInfo,
  Memory,
  Opened,
  PathState,
  PreviewKind,
  Reply,
  Row,
  Settings,
  Sort,
  Summary,
  UpdateStatus,
} from "../types";

export function isAppError(value: unknown): value is AppError {
  return typeof value === "object" && value !== null && typeof (value as AppError).code === "string";
}

/** Turns anything thrown by `invoke` into an `AppError`. */
export function toAppError(value: unknown): AppError {
  if (isAppError(value)) {
    return value;
  }
  return { code: "unknown", params: { message: String(value) } };
}

/** One transparent pixel: the drag plugin wants a picture for the cursor. */
const DRAG_ICON =
  "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";

/** A look at a file of an open archive, as the `boxit:` protocol hands it over. */
export interface PreviewContent {
  kind: PreviewKind;
  /** The file is longer than what is shown of it. */
  truncated: boolean;
  blob: Blob;
}

/** What the browser shows each kind as, whatever the archive says the file is. */
function blobType(kind: PreviewKind, served: string): string {
  switch (kind) {
    case "image":
      return served.startsWith("image/") ? served : "application/octet-stream";
    case "text":
      return "text/plain";
    case "pdf":
      return "application/pdf";
    default: {
      const unknown: never = kind;
      throw new Error(`unknown preview kind: ${String(unknown)}`);
    }
  }
}

function previewKind(value: string | null): PreviewKind | null {
  return value === "image" || value === "text" || value === "pdf" ? value : null;
}

/** Reads a file of an open archive for a look. Rejects with an `AppError`. */
async function preview(session: number, row: number, signal: AbortSignal): Promise<PreviewContent> {
  const address = `${convertFileSrc("preview", "boxit")}?s=${session}&n=${row}`;
  const response = await fetch(address, { signal, cache: "no-store" });
  if (!response.ok) {
    let failure: unknown = null;
    try {
      failure = await response.json();
    } catch {
      // The answer was not the description of an error: the status says the rest.
    }
    throw isAppError(failure) ? failure : { code: "unknown", params: { message: `HTTP ${response.status}` } };
  }
  const kind = previewKind(response.headers.get("x-boxit-kind"));
  if (!kind) {
    throw { code: "noPreview" } satisfies AppError;
  }
  const bytes = await response.arrayBuffer();
  return {
    kind,
    truncated: response.headers.get("x-boxit-truncated") === "1",
    blob: new Blob([bytes], { type: blobType(kind, response.headers.get("content-type") ?? "") }),
  };
}

export const api = {
  appInfo: () => invoke<AppInfo>("app_info"),
  /** Tells the core that the first frame is on the screen: the window may appear. */
  windowShow: () => invoke<void>("window_show"),
  /** Files and folders BoxIt was started with; handed out once. */
  openedFiles: () => invoke<string[]>("opened_files"),
  settingsGet: () => invoke<Settings>("settings_get"),
  /** Stores the settings and returns them as they were stored. */
  settingsSet: (settings: Settings) => invoke<Settings>("settings_set", { settings }),
  formats: () => invoke<FormatInfo[]>("formats_list"),
  nameEncodings: () => invoke<string[]>("name_encodings"),

  /** Opens an archive and reads its list; `archive:opening` events with `token` tell how far the reading is. */
  archiveOpen: (path: string, token: number, password: string | null, encoding: string | null) =>
    invoke<Opened>("archive_open", { path, token, password, encoding }),
  archiveCancelOpen: (token: number) => invoke<void>("archive_cancel_open", { token }),
  archiveClose: (id: number) => invoke<void>("archive_close", { id }),
  /** The rows of a folder in the order of the list, as their numbers. */
  archiveFolder: async (id: number, folder: number, sort: Sort, filter: string) =>
    new Uint32Array(await invoke<ArrayBuffer>("archive_folder", { id, folder, sort, filter })),
  archiveRows: (id: number, rows: number[]) => invoke<Row[]>("archive_rows", { id, rows }),
  archiveSummary: (id: number, rows: number[]) => invoke<Summary>("archive_summary", { id, rows }),
  archiveSetPassword: (id: number, password: string) => invoke<void>("archive_set_password", { id, password }),
  /** The format of a file that is an archive BoxIt reads; `null` for anything else. */
  recognize: (path: string) => invoke<ArchiveFormat | null>("recognize", { path }),
  preview,

  extractDestination: (id: number, place: ExtractPlace, chosen: string) =>
    invoke<string>("extract_destination", { id, place, chosen }),
  /** Queues an extraction of rows of the folder `base`; no rows stand for the whole folder. */
  extractStart: (id: number, base: number, rows: number[], destination: string, options: ExtractOptions) =>
    invoke<number>("extract_start", { id, base, rows, destination, options }),
  testStart: (id: number, base: number, rows: number[], sums = false) =>
    invoke<number>("test_start", { id, base, rows, sums }),
  repairStart: (id: number, destination: string, overwrite: boolean) =>
    invoke<number>("repair_start", { id, destination, overwrite }),
  repairSuggest: (id: number) => invoke<string>("repair_suggest", { id }),
  createStart: (form: CreateForm) => invoke<number>("create_start", { form }),
  createMemory: (form: CreateForm) => invoke<Memory>("create_memory", { form }),
  createSuggest: (sources: string[], format: ArchiveFormat) =>
    invoke<string | null>("create_suggest", { sources, format }),
  pathState: (path: string) => invoke<PathState>("path_state", { path }),

  passwordGenerate: (length: number, symbols: boolean) => invoke<string>("password_generate", { length, symbols }),
  passwordNames: () => invoke<string[]>("password_names"),
  passwordSave: (name: string, password: string) => invoke<Settings>("password_save", { name, password }),
  passwordForget: (name: string) => invoke<Settings>("password_forget", { name }),
  passwordNamed: (name: string) => invoke<string | null>("password_named", { name }),
  passwordRemember: (path: string, password: string) => invoke<void>("password_remember", { path, password }),
  passwordRecall: (path: string) => invoke<string | null>("password_recall", { path }),
  passwordForgetArchive: (path: string) => invoke<void>("password_forget_archive", { path }),

  jobsList: () => invoke<JobInfo[]>("jobs_list"),
  jobCancel: (id: number) => invoke<void>("job_cancel", { id }),
  jobPause: (id: number, paused: boolean) => invoke<void>("job_pause", { id, paused }),
  /** Answers the question a job stopped at; `false` when it does not wait for the answer any more. */
  jobAnswer: (id: number, question: number, reply: Reply) => invoke<boolean>("job_answer", { id, question, reply }),
  jobsClear: () => invoke<void>("jobs_clear"),

  updateStatus: () => invoke<UpdateStatus>("update_status"),
  /** Looks for a newer version: the only request BoxIt sends to the network. `update:status` events tell the rest. */
  updateCheck: () => invoke<void>("update_check"),
  /** Downloads and installs the version the last check found. */
  updateInstall: () => invoke<void>("update_install"),
  /** Opens the page of the version found in the browser of the system. */
  updatePage: () => invoke<void>("update_page"),

  pickArchive: () => invoke<string | null>("pick_archive"),
  pickSources: (folders: boolean) => invoke<string[]>("pick_sources", { folders }),
  pickFolder: (start: string | null) => invoke<string | null>("pick_folder", { start }),
  pickSave: (path: string, format: ArchiveFormat) => invoke<string | null>("pick_save", { path, format }),
  /** Asks a question in a dialog of the system; `true` for the answer `accept`. */
  confirm: (message: string, accept: string) => invoke<boolean>("confirm", { message, accept }),
  revealPath: (path: string) => invoke<void>("reveal_path", { path }),
  revealLogs: () => invoke<void>("reveal_logs"),

  /** Extracts rows into a temporary folder to be dragged out of the window; returns what to drag. */
  dragOut: (id: number, base: number, rows: number[]) => invoke<string[]>("drag_out", { id, base, rows }),
  /** Starts dragging files out of the window, into the file manager or another program. */
  dragFiles: (paths: string[]) =>
    invoke<void>("plugin:drag|start_drag", { item: paths, image: DRAG_ICON, onEvent: new Channel<unknown>() }),
};
