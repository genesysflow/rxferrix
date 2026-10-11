/**
 * The Backups page's settings form (unit-tested in test/backupSettings.test.ts): the stored `backups` setting (or
 * what is in effect when nothing is stored) as editable fields, and back to the `backups` argument of
 * `_system/settings:update`.
 */

import type { BackupDestination, BackupsEffective, BackupsInput, BackupsView } from "./api.js";

export const MASK = "••••";

export type IntervalUnit = "minutes" | "hours" | "days";
export const UNIT_SECS: Record<IntervalUnit, number> = { minutes: 60, hours: 3600, days: 86_400 };

export interface BackupForm {
  scheduled: boolean;
  every: number;
  unit: IntervalUnit;
  keep: number;
  includeAux: boolean;
  includeFiles: boolean;
  kind: BackupDestination["kind"];
  path: string;
  bucket: string;
  prefix: string;
  region: string;
  endpoint: string;
  accessKeyId: string;
  /** A new secret; empty keeps the stored one (`secretStored`). */
  secret: string;
  secretStored: boolean;
}

/** `secs` in the largest unit that divides it. */
export function splitInterval(secs: number): { every: number; unit: IntervalUnit } {
  for (const unit of ["days", "hours", "minutes"] as const) {
    if (secs % UNIT_SECS[unit] === 0) return { every: secs / UNIT_SECS[unit], unit };
  }
  return { every: Math.max(1, Math.round(secs / 60)), unit: "minutes" };
}

export function toForm(stored: BackupsView | null, effective: BackupsEffective): BackupForm {
  const every = stored?.everySecs ?? effective.everySecs;
  const d: BackupDestination = stored?.destination ?? { kind: "default" };
  return {
    scheduled: every !== null,
    ...splitInterval(every ?? 6 * 3600),
    keep: stored?.keep ?? effective.keep,
    includeAux: stored?.includeAux ?? effective.includeAux,
    includeFiles: stored?.includeFiles ?? effective.includeFiles,
    kind: d.kind,
    path: d.kind === "local" ? d.path : "",
    bucket: d.kind === "s3" ? d.bucket : "",
    prefix: d.kind === "s3" ? (d.prefix ?? "") : "",
    region: d.kind === "s3" ? (d.region ?? "") : "",
    endpoint: d.kind === "s3" ? (d.endpoint ?? "") : "",
    accessKeyId: d.kind === "s3" ? (d.accessKeyId ?? "") : "",
    secret: "",
    secretStored: stored?.secretAccessKey === MASK,
  };
}

export function toInput(f: BackupForm): BackupsInput {
  const opt = (s: string) => (s.trim() === "" ? null : s.trim());
  let destination: BackupDestination;
  if (f.kind === "local") destination = { kind: "local", path: f.path.trim() };
  else if (f.kind === "s3") {
    destination = {
      kind: "s3",
      bucket: f.bucket.trim(),
      prefix: f.prefix.trim(),
      region: opt(f.region),
      endpoint: opt(f.endpoint),
      accessKeyId: opt(f.accessKeyId),
    };
  } else destination = { kind: "default" };
  const usesKey = f.kind === "s3" && f.accessKeyId.trim() !== "";
  return {
    everySecs: f.scheduled ? Math.round(f.every * UNIT_SECS[f.unit]) : null,
    keep: f.keep,
    includeAux: f.includeAux,
    includeFiles: f.includeFiles,
    destination,
    secretAccessKey: !usesKey ? null : f.secret !== "" ? f.secret : f.secretStored ? MASK : null,
  };
}

/** A problem to show before saving, or null. */
export function formError(f: BackupForm): string | null {
  if (f.scheduled) {
    const secs = f.every * UNIT_SECS[f.unit];
    if (!(secs >= 60 && secs <= 30 * 86_400)) return "The interval must be between 1 minute and 30 days.";
    if (!(f.keep >= 1 && f.keep <= 1000)) return "Keep between 1 and 1000 backups.";
  }
  if (f.kind === "local" && !f.path.trim().startsWith("/")) return "The directory must be an absolute path on the server.";
  if (f.kind === "s3") {
    if (!/^[a-z0-9][a-z0-9.-]{1,61}[a-z0-9]$/.test(f.bucket.trim())) return "Enter a valid bucket name.";
    if (f.accessKeyId.trim() !== "" && f.secret === "" && !f.secretStored) return "Enter the secret access key.";
  }
  return null;
}
