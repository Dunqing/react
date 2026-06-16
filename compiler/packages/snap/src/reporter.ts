/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import chalk from 'chalk';
import fs from 'fs';
import invariant from 'invariant';
import {diff} from 'jest-diff';
import path from 'path';

function wrapWithTripleBackticks(s: string, ext: string | null = null): string {
  return `\`\`\`${ext ?? ''}
${s}
\`\`\``;
}
const SPROUT_SEPARATOR = '\n### Eval output\n';

export function writeOutputToString(
  input: string,
  compilerOutput: string | null,
  evaluatorOutput: string | null,
  logs: string | null,
  errorMessage: string | null,
) {
  // leading newline intentional
  let result = `
## Input

${wrapWithTripleBackticks(input, 'javascript')}
`; // trailing newline + space internional

  if (compilerOutput != null) {
    result += `
## Code

${wrapWithTripleBackticks(compilerOutput, 'javascript')}
`;
  } else {
    result += '\n';
  }

  if (logs != null) {
    result += `
## Logs

${wrapWithTripleBackticks(logs, null)}
`;
  }

  if (errorMessage != null) {
    result += `
## Error

${wrapWithTripleBackticks(errorMessage.replace(/^\/.*?:\s/, ''))}
          \n`;
  }
  result += `      `;
  if (evaluatorOutput != null) {
    result += SPROUT_SEPARATOR + evaluatorOutput;
  }
  return result;
}

export type TestResult = {
  actual: string | null; // null == input did not exist
  expected: string | null; // null == output did not exist
  outputPath: string;
  unexpectedError: string | null;
};
export type TestResults = Map<string, TestResult>;

/**
 * Update the fixtures directory given the compilation results
 */
export async function update(results: TestResults): Promise<void> {
  let deleted = 0;
  let updated = 0;
  let created = 0;
  const failed = [];
  for (const [basename, result] of results) {
    if (result.unexpectedError != null) {
      console.log(
        chalk.red.inverse.bold(' FAILED ') + ' ' + chalk.dim(basename),
      );
      failed.push([basename, result.unexpectedError]);
    } else if (result.actual == null) {
      // Input was deleted but the expect file still existed, remove it
      console.log(
        chalk.red.inverse.bold(' REMOVE ') + ' ' + chalk.dim(basename),
      );
      try {
        fs.unlinkSync(result.outputPath);
        console.log(' remove  ' + result.outputPath);
        deleted++;
      } catch (e) {
        console.error(
          '[Snap tester error]: failed to remove ' + result.outputPath,
        );
        failed.push([basename, result.unexpectedError]);
      }
    } else if (result.actual !== result.expected) {
      // Expected output has changed
      console.log(
        chalk.blue.inverse.bold(' UPDATE ') + ' ' + chalk.dim(basename),
      );
      try {
        fs.writeFileSync(result.outputPath, result.actual, 'utf8');
      } catch (e) {
        if (e?.code === 'ENOENT') {
          // May have failed to create nested dir, so make a directory and retry
          fs.mkdirSync(path.dirname(result.outputPath), {recursive: true});
          fs.writeFileSync(result.outputPath, result.actual, 'utf8');
        }
      }
      if (result.expected == null) {
        created++;
      } else {
        updated++;
      }
    } else {
      // Expected output is current
      console.log(
        chalk.green.inverse.bold('  OKAY  ') + ' ' + chalk.dim(basename),
      );
    }
  }
  console.log(
    `${deleted} Deleted, ${created} Created, ${updated} Updated, ${failed.length} Failed`,
  );
  for (const [basename, errorMsg] of failed) {
    console.log(`${chalk.red.bold('Fail:')} ${basename}\n${errorMsg}`);
  }
}

/**
 * Report test results to the user
 * @returns boolean indicatig whether all tests passed
 */
// Fixtures whose committed snapshots reflect the (now-removed) Rust compiler
// output as the source of truth, and so diverge from the TS compiler. These
// are skipped when running the TS compiler.
const TS_SKIP_FIXTURES: Set<string> = new Set([
  // Rust compiles successfully, TS would error. Renamed from error.todo-/error.bug-.
  'todo-hoist-type-alias-before-declaration',
  // Error message/format divergences
  'fbt/error.todo-locally-require-fbt',
  // Minor output difference (TS adds unused runtime import)
  'use-no-forget-multiple-with-eslint-suppression',
  // Cosmetic blank-line/unused-var differences between Rust and TS codegen
  'debugger',
  'debugger-memoized',
  'idx-no-outlining',
  'optional-call-with-independently-memoizable-arg',
]);
export function report(
  results: TestResults,
  verbose: boolean = false,
): boolean {
  const failures: Array<[string, TestResult]> = [];
  for (const [basename, result] of results) {
    if (TS_SKIP_FIXTURES.has(basename)) {
      continue;
    }
    const actual = result.actual;
    const expected = result.expected;
    if (actual === expected && result.unexpectedError == null) {
      if (verbose) {
        console.log(
          chalk.green.inverse.bold(' PASS ') + ' ' + chalk.dim(basename),
        );
      }
    } else {
      if (verbose) {
        console.log(
          chalk.red.inverse.bold(' FAIL ') + ' ' + chalk.dim(basename),
        );
      }
      failures.push([basename, result]);
    }
  }

  if (failures.length !== 0) {
    console.log('\n' + chalk.red.bold('Failures:') + '\n');

    for (const [basename, result] of failures) {
      console.log(chalk.red.bold('FAIL:') + ' ' + basename);
      if (result.unexpectedError != null) {
        console.log(
          ` >> Unexpected error during test: \n${result.unexpectedError}`,
        );
      } else {
        const actual = result.actual;
        const expected = result.expected;
        if (expected == null) {
          invariant(actual != null, '[Tester] Internal failure.');
          console.log(
            chalk.red('[ expected fixture output is absent ]') + '\n',
          );
        } else if (actual == null) {
          invariant(expected != null, '[Tester] Internal failure.');
          console.log(
            chalk.red(`[ fixture input for ${result.outputPath} is absent ]`) +
              '\n',
          );
        } else {
          console.log(diff(expected, actual) + '\n');
        }
      }
    }
  }

  console.log(
    `${results.size} Tests, ${results.size - failures.length} Passed, ${
      failures.length
    } Failed`,
  );
  return failures.length === 0;
}
