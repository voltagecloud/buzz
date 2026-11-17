import assert from "node:assert/strict";
import { compileProfile } from "./seccomp.mjs";

const upstream = {
  defaultAction: "SCMP_ACT_ERRNO", defaultErrnoRet: 1,
  archMap: [{ architecture: "SCMP_ARCH_X86_64", subArchitectures: ["SCMP_ARCH_X86"] },
    { architecture: "SCMP_ARCH_AARCH64", subArchitectures: ["SCMP_ARCH_ARM"] }],
  syscalls: [
    { names: ["read"], action: "SCMP_ACT_ALLOW" },
    { names: ["bpf", "perf_event_open", "reboot"], action: "SCMP_ACT_ALLOW", includes: { caps: ["CAP_SYS_ADMIN"] } },
    { names: ["clone3"], action: "SCMP_ACT_ERRNO", errnoRet: 38, excludes: { caps: ["CAP_SYS_ADMIN"] } },
    { names: ["arch_prctl"], action: "SCMP_ACT_ALLOW", includes: { arches: ["amd64"] } },
  ],
};
for (const architecture of ["x64", "arm64"]) {
  const profile = compileProfile(upstream, architecture);
  assert.equal(profile.defaultAction, "SCMP_ACT_ERRNO");
  assert.ok(!profile.archMap && profile.syscalls.every((rule) => !rule.includes && !rule.excludes));
  const allowed = new Set(profile.syscalls.filter((rule) => rule.action === "SCMP_ACT_ALLOW").flatMap((rule) => rule.names));
  for (const name of ["bpf", "perf_event_open", "reboot"]) assert.ok(!allowed.has(name), name);
  for (const name of ["read", "clone", "unshare", "mount"]) assert.ok(allowed.has(name), name);
  assert.equal(allowed.has("arch_prctl"), architecture === "x64");
  assert.ok(profile.syscalls.some((rule) => rule.names.includes("clone3") && rule.errnoRet === 38));
}
assert.throws(() => compileProfile(upstream, "unsupported"));
assert.throws(() => compileProfile({ ...upstream, syscalls: [{ names: ["bpf"], includes: { unexpected: true } }] }));
console.log("PASS: OCI seccomp compilation preserves capability and architecture restrictions");
