// SPDX-License-Identifier: Apache-2.0
// Moby's pinned default, with only bubblewrap's namespace operations added.
// No container receives CAP_SYS_ADMIN: mount operations are usable only after
// bubblewrap enters an unprivileged user namespace.
import { createHash } from "node:crypto";
import { mkdir, writeFile } from "node:fs/promises";
import path from "node:path";
import { pathToFileURL } from "node:url";

/** Compile Docker's conditional profile into OCI for a capability-free Pod. */
export function compileProfile(base, architecture = process.arch) {
  const names = { x64: ["amd64", "SCMP_ARCH_X86_64"], arm64: ["arm64", "SCMP_ARCH_AARCH64"] };
  const target = names[architecture];
  if (!target || base.defaultAction !== "SCMP_ACT_ERRNO") throw new Error("unsupported seccomp target");
  const arch = base.archMap.find((entry) => entry.architecture === target[1]);
  if (!arch) throw new Error("missing seccomp architecture");
  const syscalls = base.syscalls.filter(({ includes = {}, excludes = {} }) => {
    for (const condition of [includes, excludes]) {
      if (Object.keys(condition).some((key) => !["arches", "caps", "minKernel"].includes(key))) {
        throw new Error("unsupported seccomp condition");
      }
      if (condition.minKernel && condition.minKernel !== "4.8") throw new Error("unsupported kernel condition");
    }
    // Supported daemon kernels are >= 4.8. No container capabilities are present.
    return !includes.caps?.length && (!includes.arches || includes.arches.includes(target[0]))
      && !excludes.arches?.includes(target[0]);
  }).map(({ includes, excludes, ...rule }) => rule);
  syscalls.push({
    names: ["clone", "unshare", "setns", "mount", "umount", "umount2", "pivot_root", "sethostname", "setdomainname", "chroot"],
    action: "SCMP_ACT_ALLOW",
    comment: "Allow bubblewrap's unprivileged namespaces; the container has no host capabilities.",
  });
  return { defaultAction: base.defaultAction, defaultErrnoRet: base.defaultErrnoRet,
    architectures: [arch.architecture, ...(arch.subArchitectures ?? [])], syscalls };
}

const revision = "f862455628f78f84c937e967e4a03902fb1dee11";
const files = [
  ["seccomp/default.json", "3320527d4568265ac97a077609b1bfd4cfd0d789eeb9239b7e094b1526675406"],
  ["LICENSE", "cfc7749b96f63bd31c3c42b5c471bf756814053e847c10f3eb003417bc523d30"],
];
async function main(destination) {
if (!destination) throw new Error("usage: node seccomp.mjs <output-directory>");
await mkdir(destination, { recursive: true });
for (const [file, checksum] of files) {
  const response = await fetch(`https://raw.githubusercontent.com/moby/profiles/${revision}/${file}`, {
    signal: AbortSignal.timeout(30_000),
  });
  if (!response.ok) throw new Error(`seccomp source download failed: ${response.status}`);
  let bytes = Buffer.alloc(0);
  for await (const chunk of response.body) {
    bytes = Buffer.concat([bytes, chunk]);
    if (bytes.length > 128 * 1024) throw new Error("seccomp source exceeds download limit");
  }
  if (createHash("sha256").update(bytes).digest("hex") !== checksum) {
    throw new Error("seccomp source checksum mismatch");
  }
  if (file === "LICENSE") {
    await writeFile(path.join(destination, "seccomp.LICENSE"), bytes);
    continue;
  }
  const profile = JSON.parse(bytes);
  if (profile.defaultAction !== "SCMP_ACT_ERRNO" || !Array.isArray(profile.syscalls)) {
    throw new Error("unexpected upstream seccomp profile");
  }
  await writeFile(path.join(destination, "seccomp.json"), `${JSON.stringify(compileProfile(profile), null, 2)}\n`);
}
}
if (process.argv[1] && import.meta.url === pathToFileURL(path.resolve(process.argv[1])).href) {
  await main(process.argv[2]);
}
