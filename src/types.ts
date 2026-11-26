// Shapes of what the Rust commands take and return; the names mirror the
// structures of `boxit-core`, `boxit-engine` and `src-tauri`.

export const LANGUAGES = ["ru", "en"] as const;
export type Language = (typeof LANGUAGES)[number];

export const THEMES = ["system", "light", "dark"] as const;
export type Theme = (typeof THEMES)[number];

/** An error of a command: a code the dictionaries have a text for, and its values. */
export interface AppError {
  code: string;
  params?: Record<string, string>;
}

export interface AppInfo {
  version: string;
  dataDir: string;
  portable: boolean;
  systemLanguage: Language;
  /** Language of the interface: the setting, or the system language. */
  language: Language;
}

/** A format by its usual extension: `zip`, `7z`, `tar.gz`. */
export type ArchiveFormat = string;

export const LEVELS = ["store", "fast", "normal", "maximum", "ultra"] as const;
export type Level = (typeof LEVELS)[number];

export type Method = "store" | "deflate" | "deflate64" | "bzip2" | "lzma" | "lzma2" | "ppmd" | "zstd" | "xz";

export const CONFLICT_POLICIES = ["ask", "skip", "overwrite", "rename"] as const;
export type ConflictPolicy = (typeof CONFLICT_POLICIES)[number];

export const SYMLINK_POLICIES = ["skip", "extract", "ask"] as const;
export type SymlinkPolicy = (typeof SYMLINK_POLICIES)[number];

export const EXTRACT_PLACES = ["named", "beside", "chosen"] as const;
export type ExtractPlace = (typeof EXTRACT_PLACES)[number];

export interface ExtractChoices {
  place: ExtractPlace;
  /** The folder chosen the last time. */
  folder: string;
  conflicts: ConflictPolicy;
  symlinks: SymlinkPolicy;
}

export interface CreateChoices {
  format: ArchiveFormat;
  level: Level;
}

export interface Settings {
  /** Interface language; empty follows the system. */
  language: Language | "";
  theme: Theme;
  /** The pane with a look at the chosen file is open. */
  preview: boolean;
  extract: ExtractChoices;
  create: CreateChoices;
  /** The archives opened lately, the latest first. */
  recent: string[];
  /** Names of passwords kept in the credential store, never the passwords. */
  savedPasswords: string[];
  keys: Keys;
  /** Icon name for a format id such as `zip`. */
  icons: Record<string, string>;
}

/** Keys that work on the list. A value is `F3` or `Alt+F9`. */
export interface Keys {
  view: string;
  edit: string;
  copy: string;
  extract: string;
}

/** A format as the dialogs see it. */
export interface FormatInfo {
  id: ArchiveFormat;
  /** Extensions of the files of the format, the main one first. */
  extensions: string[];
  canWrite: boolean;
  /** The format holds one compressed file, not files and folders. */
  singleFile: boolean;
  /** The methods to choose from, the usual one first; none for a format that has one. */
  methods: Method[];
  method: Method;
  /** The methods the size of the dictionary can be chosen for. */
  dictionary: Method[];
  levels: boolean;
  encrypt: boolean;
  encryptNames: boolean;
  solid: boolean;
  comment: boolean;
}

// The list of an archive

export type EntryKind = "file" | "directory" | "symlink" | "hardlink";

export const SORT_KEYS = ["name", "size", "packed", "modified", "kind"] as const;
export type SortKey = (typeof SORT_KEYS)[number];

export interface Sort {
  key: SortKey;
  descending: boolean;
}

/** A line of the list. */
export interface Row {
  id: number;
  name: string;
  kind: EntryKind;
  /** Size of a file; of a folder, the size of everything inside it. */
  size?: number;
  packed?: number;
  /** Seconds since 1970. */
  modified?: number;
  method?: string;
  encrypted: boolean;
  /** Where a link points. */
  target?: string;
  /** Files and folders right inside a folder. */
  items?: number;
  crc32?: number;
}

export interface ArchiveInfo {
  /** The archive file. */
  path: string;
  format: ArchiveFormat;
  /** Size of the archive file. */
  size: number;
  /** The number of files the archive is in. */
  volumes: number;
  comment?: string;
  files: number;
  folders: number;
  /** Bytes of content, as far as the archive tells before it is unpacked. */
  bytes: number;
  solid: boolean;
  encrypted: boolean;
}

/** An archive whose list is read. */
export interface Opened {
  id: number;
  info: ArchiveInfo;
  /** The encoding the names were asked to be read in: a label, or `auto`. */
  encoding: string;
  entries: number;
}

/** How far the reading of a list is. */
export interface Opening {
  token: number;
  entries: number;
}

export interface Summary {
  files: number;
  folders: number;
  bytes: number;
  packed: number;
}

// Operations

export interface Limits {
  maxFileSize: number;
  maxRatio: number;
  ratioFloor: number;
  checkFreeSpace: boolean;
}

export interface ExtractOptions {
  conflicts: ConflictPolicy;
  symlinks: SymlinkPolicy;
  /** Left out, the limits of the core apply. */
  limits?: Limits;
}

/** A new archive as the dialog describes it. */
export interface CreateForm {
  sources: string[];
  destination: string;
  format: ArchiveFormat;
  level: Level;
  method: Method | null;
  solid: boolean;
  /** Size of the dictionary in bytes; `null` follows the level. */
  dictionary: number | null;
  comment: string | null;
  password: string | null;
  encryptNames: boolean;
  /** Worker threads; 0 takes one per processor. */
  threads: number;
  overwrite: boolean;
  /** Cut the archive into volumes of this many bytes. */
  volume: number | null;
  /** Glue the archive to the program that unpacks it. ZIP and 7z. */
  selfExtracting: boolean;
}

export interface Memory {
  needed: number;
  available: number;
}

export type PathState = "file" | "folder" | "missing";

export type JobKind = "extract" | "create" | "test" | "repair" | "update";
export type JobState = "queued" | "running" | "paused" | "done" | "failed" | "cancelled";

export type Question =
  | { kind: "password"; id: number; archive: string; entry: string | null; retry: boolean }
  | {
      kind: "conflict";
      id: number;
      path: string;
      existingSize: number;
      existingModified: number | null;
      size: number | null;
      modified: number | null;
    }
  | { kind: "symlink"; id: number; name: string; target: string };

export type ConflictChoice = "skip" | "overwrite" | "rename" | "cancel";

export type Reply =
  | { kind: "password"; password: string | null }
  | { kind: "conflict"; choice: ConflictChoice; all: boolean }
  | { kind: "symlink"; create: boolean };

/** An entry that was not extracted, and why. */
export interface Skipped {
  path: string;
  reason: AppError;
}

/** What an extraction did. */
export interface ExtractResult {
  files: number;
  folders: number;
  links: number;
  bytes: number;
  skipped: Skipped[];
  skippedTotal: number;
  destination: string;
}

/** The sums of one file of an archive. */
export interface Sum {
  path: string;
  size: number;
  crc32: number;
  sha256: string;
}

/** What a test read. */
export interface TestResult {
  files: number;
  bytes: number;
  failed: Skipped[];
  failedTotal: number;
  complete: boolean;
  sums?: Sum[];
  archiveSha256?: string;
}

/** What a repair saved. */
export interface RepairResult {
  path: string;
  format: ArchiveFormat;
  files: number;
  folders: number;
  bytes: number;
  packed: number;
  rebuilt: boolean;
  unchecked: number;
  lost: Skipped[];
  lostTotal: number;
}

/** What a change of an archive did. */
export interface UpdateResult {
  path: string;
  format: ArchiveFormat;
  repacked: boolean;
  files: number;
  folders: number;
  bytes: number;
  packed: number;
}

/** A file or folder on the disk, in the second pane. */
export interface DiskEntry {
  name: string;
  path: string;
  directory: boolean;
  size: number;
}

/** When a file was last written, and how long it is. */
export interface Stamp {
  modified: number;
  size: number;
}

/** What a creation did. */
export interface CreateResult {
  path: string;
  files: number;
  folders: number;
  links: number;
  bytes: number;
  packed: number;
}

export interface JobInfo {
  id: number;
  kind: JobKind;
  /** The archive the job reads or writes. */
  archive: string;
  /** The folder an extraction goes to. */
  target: string | null;
  state: JobState;
  bytesDone: number;
  bytesTotal: number | null;
  entriesDone: number;
  entriesTotal: number | null;
  /** What is being worked on right now. */
  current: string;
  /** Bytes of content a second, lately. */
  speed: number | null;
  etaSeconds: number | null;
  /** The job waits for an answer to this. */
  question: Question | null;
  error: AppError | null;
  /** What the job did, once it is done: by its kind. */
  result: unknown;
}

export type PreviewKind = "image" | "text" | "pdf";

export type UpdateState =
  | "idle"
  | "checking"
  | "upToDate"
  | "available"
  | "downloading"
  | "ready"
  | "installing"
  | "failed";

/** What is known about updates in this session. */
export interface UpdateStatus {
  state: UpdateState;
  /** The version found; with `failed`, the one that could not be installed, `null` when the check failed. */
  version: string | null;
  /** Downloaded and total bytes of the installer; the total is zero while it is not known. */
  downloaded: number;
  total: number;
  /** This copy can replace itself with a newer one. */
  installable: boolean;
}
