import * as path from "path";
import Mocha from "mocha";

export function run(): Promise<void> {
  const mocha = new Mocha({
    color: true,
    timeout: 30000,
    ui: "tdd",
  });

  mocha.addFile(path.resolve(__dirname, "setup.test.js"));
  mocha.addFile(path.resolve(__dirname, "smoke.test.js"));

  return new Promise((resolve, reject) => {
    mocha.run((failures) => {
      if (failures > 0) {
        reject(new Error(`${failures} VS Code extension test(s) failed`));
      } else {
        resolve();
      }
    });
  });
}
