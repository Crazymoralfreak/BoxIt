// Events the core sends to the window.

import { listen } from "@tauri-apps/api/event";
import { useEffect, useEffectEvent } from "react";
import type { JobInfo, Opening, UpdateStatus } from "../types";

interface AppEvents {
  /** A job of the queue has changed. */
  "jobs:changed": JobInfo;
  /** The reading of a list has found more entries. */
  "archive:opening": Opening;
  /** A second start of BoxIt handed over what it was started with. */
  "open:files": string[];
  /** A check for updates, or the installation of one, has moved on. */
  "update:status": UpdateStatus;
}

/** Calls `handler` for every event `name` while the component is mounted. */
export function useAppEvent<Name extends keyof AppEvents>(name: Name, handler: (payload: AppEvents[Name]) => void) {
  const onEvent = useEffectEvent(handler);
  useEffect(() => {
    let cancelled = false;
    let unlisten: (() => void) | undefined;
    void listen<AppEvents[Name]>(name, (event) => onEvent(event.payload)).then((stop) => {
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
  }, [name]);
}
