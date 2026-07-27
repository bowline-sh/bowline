// The command-name vocabulary has exactly one definition: the wire contract
// (`contracts/wire/commands-v8.json`). Re-exported here rather than restated so
// a command cannot exist on the wire and be missing from the TypeScript union,
// which is how `device key-status` went unlisted.
export {
  KNOWN_COMMAND_NAMES as COMMAND_NAMES,
  type CommandName,
  type KnownCommandName,
} from "./generated/wire-contracts";
