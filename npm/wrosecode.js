#!/usr/bin/env node
const { spawnSync } = require("node:child_process");
const binary = process.env.WROSECODE_BINARY || "wrosecode-native";
const result = spawnSync(binary, process.argv.slice(2), { stdio: "inherit" });
if (result.error) {
  console.error("Install the WROSECODE release binary or set WROSECODE_BINARY.");
  process.exit(1);
}
process.exit(result.status ?? 1);
