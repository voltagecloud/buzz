// A plugin's own files, under the folder Buzz gives it
// (`BUZZ_PLUGIN_DATA_DIR`), so they survive updates and never sit in the
// plugin's folder, which may be read-only.

import { mkdirSync, readFileSync, renameSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { log } from "./rpc.ts";

/** Where the plugin keeps its files: Buzz's folder for it, else `.data` here. */
export function dataDir(): string {
  const dir = process.env.BUZZ_PLUGIN_DATA_DIR || join(process.cwd(), ".data");
  mkdirSync(dir, { recursive: true });
  return dir;
}

/** The JSON saved as `name`, or `fallback` when there is none or it is unreadable. */
export function readJson<T>(name: string, fallback: T): T {
  try {
    return JSON.parse(readFileSync(join(dataDir(), name), "utf8")) as T;
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code !== "ENOENT") log(`could not read ${name}:`, error);
    return fallback;
  }
}

/** Saves `value` as `name`, whole or not at all. */
export function writeJson(name: string, value: unknown): void {
  const path = join(dataDir(), name);
  const temporary = `${path}.${process.pid}.tmp`;
  writeFileSync(temporary, JSON.stringify(value));
  renameSync(temporary, path);
}
