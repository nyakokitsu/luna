#!/usr/bin/env node
// Sign Luna release checksums with the maintainer's Ed25519 key.
//
// The updater embeds only the public key. Keep the private key off the
// repository and off CI; releases are built locally.
//
//   node tools/sign-release.mjs keygen luna-update-key.pem
//   node tools/sign-release.mjs pubkey luna-update-key.pem
//   node tools/sign-release.mjs sign dist/release/SHA256SUMS luna-update-key.pem
//   node tools/sign-release.mjs verify dist/release/SHA256SUMS luna-update-key.pem.pub.pem
//
// `sign` writes a raw 64-byte SHA256SUMS.sig next to the checksums. The
// updater also accepts the same signature as 128 hex characters.

import {
  createPrivateKey,
  createPublicKey,
  generateKeyPairSync,
  sign,
  verify,
} from "node:crypto";
import { chmodSync, readFileSync, writeFileSync } from "node:fs";

const [command, ...args] = process.argv.slice(2);

function fail(message) {
  console.error(`sign-release: ${message}`);
  process.exit(1);
}

function usage() {
  console.error(`usage:
  sign-release.mjs keygen [private-key.pem]
  sign-release.mjs pubkey <private-key.pem>
  sign-release.mjs sign <SHA256SUMS> <private-key.pem>
  sign-release.mjs verify <SHA256SUMS> <public-key.pem>`);
  process.exit(1);
}

function rawPublicKey(key) {
  return key.export({ type: "spki", format: "der" }).subarray(-32).toString("hex");
}

function loadPublicKey(path) {
  const key = createPublicKey(readFileSync(path));
  if (key.asymmetricKeyType !== "ed25519") fail(`${path} is not an Ed25519 key`);
  return key;
}

switch (command) {
  case "keygen": {
    const path = args[0] || "luna-update-key.pem";
    const { publicKey, privateKey } = generateKeyPairSync("ed25519");
    writeFileSync(path, privateKey.export({ type: "pkcs8", format: "pem" }), { mode: 0o600 });
    chmodSync(path, 0o600);
    writeFileSync(`${path}.pub.pem`, publicKey.export({ type: "spki", format: "pem" }));
    console.log(`LUNA_UPDATE_PUBLIC_KEY=${rawPublicKey(publicKey)}`);
    console.log(`private key: ${path}`);
    console.log(`public key:  ${path}.pub.pem`);
    break;
  }
  case "pubkey": {
    if (!args[0]) usage();
    const privateKey = createPrivateKey(readFileSync(args[0]));
    if (privateKey.asymmetricKeyType !== "ed25519") fail(`${args[0]} is not an Ed25519 key`);
    console.log(rawPublicKey(createPublicKey(privateKey)));
    break;
  }
  case "sign": {
    const [sumsPath, keyPath] = args;
    if (!sumsPath || !keyPath) usage();
    const data = readFileSync(sumsPath);
    const privateKey = createPrivateKey(readFileSync(keyPath));
    if (privateKey.asymmetricKeyType !== "ed25519") fail(`${keyPath} is not an Ed25519 key`);
    const signature = sign(null, data, privateKey);
    if (!verify(null, data, createPublicKey(privateKey), signature)) {
      fail("signature self-check failed");
    }
    const output = `${sumsPath}.sig`;
    writeFileSync(output, signature);
    console.log(`signed ${sumsPath} -> ${output} (${signature.length} bytes)`);
    break;
  }
  case "verify": {
    const [sumsPath, keyPath] = args;
    if (!sumsPath || !keyPath) usage();
    const data = readFileSync(sumsPath);
    const signature = readFileSync(`${sumsPath}.sig`);
    if (!verify(null, data, loadPublicKey(keyPath), signature)) fail("signature is invalid");
    console.log(`signature is valid: ${sumsPath}`);
    break;
  }
  default:
    usage();
}
