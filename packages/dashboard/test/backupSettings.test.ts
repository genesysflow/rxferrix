import { describe, expect, it } from "vitest";
import type { BackupsEffective } from "../src/lib/api.js";
import { MASK, formError, splitInterval, toForm, toInput } from "../src/lib/backupSettings.js";

const effective: BackupsEffective = {
  target: "/data/backups",
  everySecs: null,
  keep: 7,
  includeAux: false,
  includeFiles: false,
  s3Available: true,
};

describe("backup settings form", () => {
  it("splits intervals into the largest whole unit", () => {
    expect(splitInterval(86_400 * 2)).toEqual({ every: 2, unit: "days" });
    expect(splitInterval(5400)).toEqual({ every: 90, unit: "minutes" });
    expect(splitInterval(21_600)).toEqual({ every: 6, unit: "hours" });
  });

  it("starts from what is in effect when nothing is stored, unscheduled, to the data directory", () => {
    const f = toForm(null, effective);
    expect([f.scheduled, f.kind, f.keep]).toEqual([false, "default", 7]);
    expect(toInput(f)).toEqual({
      everySecs: null,
      keep: 7,
      includeAux: false,
      includeFiles: false,
      destination: { kind: "default" },
      secretAccessKey: null,
    });
  });

  it("round-trips an S3 destination and keeps a stored secret unless a new one is typed", () => {
    const stored = {
      everySecs: 3600,
      keep: 3,
      includeAux: true,
      includeFiles: true,
      destination: { kind: "s3" as const, bucket: "my-bucket", prefix: "prod", region: "eu-west-1", endpoint: null, accessKeyId: "AKID" },
      secretAccessKey: MASK,
    };
    const f = toForm(stored, effective);
    expect([f.every, f.unit, f.secretStored]).toEqual([1, "hours", true]);
    const input = toInput(f);
    expect(input.secretAccessKey).toBe(MASK);
    expect(input.destination).toEqual({ ...stored.destination, endpoint: null });
    expect(toInput({ ...f, secret: "new-secret" }).secretAccessKey).toBe("new-secret");
    // Without an access key id the server's environment credentials are used: no secret is sent.
    expect(toInput({ ...f, accessKeyId: "" }).secretAccessKey).toBeNull();
  });

  it("flags what the server would refuse", () => {
    const f = toForm(null, effective);
    expect(formError({ ...f, scheduled: true, every: 30, unit: "minutes" })).toBeNull();
    expect(formError({ ...f, scheduled: true, every: 0, unit: "minutes" })).toMatch(/interval/);
    expect(formError({ ...f, kind: "local", path: "relative" })).toMatch(/absolute/);
    expect(formError({ ...f, kind: "s3", bucket: "Bad_Bucket" })).toMatch(/bucket/);
    expect(formError({ ...f, kind: "s3", bucket: "good-bucket", accessKeyId: "AKID" })).toMatch(/secret/);
  });
});
