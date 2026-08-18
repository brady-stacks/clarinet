import { afterAll, beforeAll, beforeEach, afterEach, RunnerTask } from "vitest";

import "./clarityValuesMatchers";

function getFullTestName(task: RunnerTask, names: string[]) {
  const fullNames = [task.name, ...names];
  if (task.suite?.name) {
    return getFullTestName(task.suite, fullNames);
  }
  return fullNames;
}

/*
  The `initBeforeEach` options controls the initialisation of the session.
  If the session is initialised before each test, the reports are collected after each test.
  If the session is not initialised before each test, it'll be initialized in the `beforeAll`, which
  will run for all test file. In that case reports are collected in the after all.
*/

beforeEach(async (ctx) => {
  const { coverage, initBeforeEach, manifestPath } = global.options.clarinet;

  if (initBeforeEach) {
    await simnet.initSession(process.cwd(), manifestPath);
  }

  if (coverage) {
    const suiteTestNames = getFullTestName(ctx.task, []);
    const fullName = [ctx.task.file?.name || "", ...suiteTestNames].join("__");
    simnet.setCurrentTestName(fullName);
  }
});

afterEach(async (ctx) => {
  const { coverage, costs, initBeforeEach, includeBootContracts, bootContractsPath } =
    global.options.clarinet;

  if (ctx.task.result?.state === "fail") {
    const stackTrace = simnet.getLastContractCallTrace();
    if (stackTrace) {
      console.log(stackTrace);
    }
  }

  if (initBeforeEach && (coverage || costs)) {
    const report = simnet.collectReport(includeBootContracts || false, bootContractsPath || "");
    if (coverage) coverageReports.push(report.coverage);
    if (costs) costsReports.push(report.costs);
  }
});

beforeAll(async () => {
  const debugPort = process.env["CLARINET_DEBUG_PORT"]
    ? Number(process.env["CLARINET_DEBUG_PORT"])
    : undefined;

  if (debugPort) {
    const { DebugSimnet } = await import("@stacks/clarinet-sdk");
    // Replace the global simnet with a synchronous debug-server-backed instance.
    (global as any).simnet = await DebugSimnet.connect(debugPort);
    // Disable beforeEach re-init: debug sessions are not reset between tests.
    global.options.clarinet.initBeforeEach = false;
    return;
  }

  const { initBeforeEach, manifestPath } = global.options.clarinet;

  if (!initBeforeEach) {
    await simnet.initSession(process.cwd(), manifestPath);
  }
});

afterAll(() => {
  const { coverage, costs, initBeforeEach, includeBootContracts, bootContractsPath } =
    global.options.clarinet;

  if (!initBeforeEach && (coverage || costs)) {
    const report = simnet.collectReport(includeBootContracts || false, bootContractsPath || "");
    if (coverage) coverageReports.push(report.coverage);
    if (costs) costsReports.push(report.costs);
  }
});
