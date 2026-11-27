// Numbers, sizes and times as the language of the interface writes them.

import { useMemo } from "react";
import { useT } from "./i18n";
import type { ArchiveFormat, Method } from "./types";

const UNITS = ["b", "kb", "mb", "gb", "tb"] as const;

/** Names of the compression methods: they are the same in every language. */
const METHOD_NAMES: Record<Method, string> = {
  store: "Store",
  deflate: "Deflate",
  deflate64: "Deflate64",
  bzip2: "BZip2",
  lzma: "LZMA",
  lzma2: "LZMA2",
  ppmd: "PPMd",
  zstd: "Zstandard",
  xz: "XZ",
};

export function methodName(method: Method): string {
  return METHOD_NAMES[method];
}

/** A format as it is written: `ZIP`, `7Z`, `TAR.GZ`. */
export function formatName(format: ArchiveFormat): string {
  return format.toUpperCase();
}

/** The last part of a path, whichever way its slashes lean. */
export function baseName(path: string): string {
  const trimmed = path.replace(/[\\/]+$/, "");
  const cut = Math.max(trimmed.lastIndexOf("/"), trimmed.lastIndexOf("\\"));
  return cut < 0 ? trimmed : trimmed.slice(cut + 1);
}

/** The folder a path is in; empty for a name alone. */
export function parentPath(path: string): string {
  const trimmed = path.replace(/[\\/]+$/, "");
  const cut = Math.max(trimmed.lastIndexOf("/"), trimmed.lastIndexOf("\\"));
  return cut < 0 ? "" : trimmed.slice(0, cut);
}

export interface Formatter {
  /** A number of bytes: `1.5 MB`. */
  size: (bytes: number) => string;
  /** A whole number with the separators of the language. */
  count: (value: number) => string;
  /** A moment given in seconds since 1970. */
  date: (seconds: number) => string;
  /** A length of time in seconds: `3 min 20 s`. */
  duration: (seconds: number) => string;
  /** How much smaller the packed size is, in percent of the full one. */
  ratio: (packed: number, full: number) => string;
}

export function useFormat(): Formatter {
  const { language, t, td } = useT();
  return useMemo(() => {
    const whole = new Intl.NumberFormat(language, { maximumFractionDigits: 0 });
    const moment = new Intl.DateTimeFormat(language, { dateStyle: "short", timeStyle: "short" });
    return {
      size: (bytes) => {
        let value = Math.max(0, bytes);
        let unit = 0;
        while (value >= 1024 && unit < UNITS.length - 1) {
          value /= 1024;
          unit += 1;
        }
        const digits = unit === 0 || value >= 100 ? 0 : value >= 10 ? 1 : 2;
        const text = new Intl.NumberFormat(language, {
          minimumFractionDigits: digits,
          maximumFractionDigits: digits,
        }).format(value);
        return td(`cli.unit.${UNITS[unit]}`, { value: text });
      },
      count: (value) => whole.format(value),
      date: (seconds) => moment.format(new Date(seconds * 1000)),
      duration: (seconds) => {
        const total = Math.max(0, Math.round(seconds));
        const hours = Math.floor(total / 3600);
        const minutes = Math.floor((total % 3600) / 60);
        const rest = total % 60;
        if (hours > 0) {
          return t("time.hours", { hours, minutes });
        }
        if (minutes > 0) {
          return t("time.minutes", { minutes, seconds: rest });
        }
        return t("time.seconds", { seconds: rest });
      },
      ratio: (packed, full) => {
        if (full <= 0) {
          return whole.format(100);
        }
        return whole.format(Math.min(999, Math.round((packed / full) * 100)));
      },
    };
  }, [language, t, td]);
}
