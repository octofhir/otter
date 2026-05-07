import data from "./data.json";
import { installedValue } from "fixture-tool";
import { workspaceValue } from "workspace-lib";

function fail() {
  return undefined.x;
}

if (data.value + installedValue + workspaceValue !== 6) {
  fail();
}

