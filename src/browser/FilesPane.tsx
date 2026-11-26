// The folder on the disk beside the archive: files go into the archive
// from here, and out of the archive to here.

import { useEffect, useState } from "react";
import { api, toAppError } from "../api";
import { Icon } from "../components/Icon";
import { useFormat } from "../format";
import { useT } from "../i18n";
import type { AppError, DiskEntry } from "../types";

export function FilesPane({
  folder,
  onFolder,
  selected,
  onSelected,
  onFailure,
}: {
  folder: string;
  onFolder: (path: string) => void;
  selected: string[];
  onSelected: (paths: string[]) => void;
  onFailure: (error: AppError) => void;
}) {
  const { t } = useT();
  const format = useFormat();
  const [rows, setRows] = useState<DiskEntry[]>([]);

  useEffect(() => {
    let alive = true;
    api
      .filesList(folder)
      .then((found) => {
        if (alive) {
          setRows(found);
        }
      })
      .catch((error: unknown) => onFailure(toAppError(error)));
    return () => {
      alive = false;
    };
  }, [folder]);

  const parent = folder.replace(/[\\/][^\\/]+$/, "");

  return (
    <aside className="files">
      <div className="pathbar">
        <button
          type="button"
          className="ghost square"
          title={t("files.up")}
          disabled={parent === folder || parent === ""}
          onClick={() => onFolder(parent)}
        >
          <Icon name="up" />
        </button>
        <span className="label" title={folder}>
          {folder}
        </span>
      </div>
      <ul className="file-list">
        {rows.map((row) => {
          const on = selected.includes(row.path);
          return (
            <li key={row.path}>
              <button
                type="button"
                className={on ? "file-row selected" : "file-row"}
                onClick={(event) => {
                  if (event.ctrlKey || event.metaKey) {
                    onSelected(on ? selected.filter((path) => path !== row.path) : [...selected, row.path]);
                  } else {
                    onSelected([row.path]);
                  }
                }}
                onDoubleClick={() => {
                  if (row.directory) {
                    onFolder(row.path);
                  }
                }}
              >
                <Icon name={row.directory ? "folder" : "file"} />
                <span className="label">{row.name}</span>
                {!row.directory && <span className="dim">{format.size(row.size)}</span>}
              </button>
            </li>
          );
        })}
      </ul>
    </aside>
  );
}
