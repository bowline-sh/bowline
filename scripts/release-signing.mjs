import { spawnSync } from "node:child_process";
import { readFileSync, rmSync } from "node:fs";

export const releaseSigningIdentity = "bowline-release";
export const releaseSigningNamespace = "bowline-release";

export function releaseAllowedSignersLine(publicKey) {
  return `${releaseSigningIdentity} ${publicKey.trim()}\n`;
}

export function signReleaseFile(file, keyFile, options = {}) {
  if (!keyFile) return null;
  rmSync(`${file}.sig`, { force: true });
  const args = [
    "-Y",
    "sign",
    "-q",
    "-f",
    keyFile,
    "-n",
    releaseSigningNamespace,
    file,
  ];
  options.log?.(`run ssh-keygen ${args.join(" ")}`);
  const result = spawnSync("ssh-keygen", args, {
    cwd: options.cwd ?? process.cwd(),
    encoding: "utf8",
    stdio: options.capture ? ["ignore", "pipe", "pipe"] : "inherit",
  });
  if (result.status !== 0) {
    const stderr = options.capture ? `\n${result.stderr}` : "";
    throw new Error(`ssh-keygen ${args.join(" ")} failed${stderr}`);
  }
  return `${file}.sig`;
}

export function verifyReleaseFile(
  file,
  signature,
  allowedSigners,
  options = {},
) {
  const args = [
    "-Y",
    "verify",
    "-f",
    allowedSigners,
    "-I",
    releaseSigningIdentity,
    "-n",
    releaseSigningNamespace,
    "-s",
    signature,
  ];
  const result = spawnSync("ssh-keygen", args, {
    cwd: options.cwd ?? process.cwd(),
    encoding: "utf8",
    input: readFileSync(file),
    stdio: ["pipe", "pipe", "pipe"],
  });
  if (result.status !== 0) {
    throw new Error(
      `ssh-keygen ${args.join(" ")} failed\nstdout:\n${result.stdout}\nstderr:\n${result.stderr}`,
    );
  }
  return result.stdout.trim();
}
