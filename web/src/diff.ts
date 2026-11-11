import {
  hydratePartialDiff,
  parseDiffFromFile,
  parsePatchFiles,
  type FileContents,
  type FileDiffMetadata,
  type SelectionSide,
} from '@pierre/diffs';
import type { Range } from './api';

/** A line as the library's selection names it. */
export interface Point {
  side: SelectionSide;
  line: number;
}

/**
 * A row of a file's diff as the unified view shows it. A removed row is on the
 * deletions side, numbered as of the start. Any other row is on the additions side,
 * numbered as the file is, and `old` numbers a context row as of the start.
 */
export interface Row extends Point {
  old: number;
  mark: ' ' | '-' | '+';
  text: string;
}

/** A line of the library's arrays without the newline they keep on it. */
const unended = (line: string | undefined) => line?.replace(/\n$/, '') ?? '';

/**
 * The rows of `diff`: those of its hunks, and once the library has loaded the whole of
 * both sides, the unchanged lines between them, which expanding a hunk shows.
 */
export function rows(diff: FileDiffMetadata): Row[] {
  const all: Row[] = [];
  let [oldAt, newAt] = [0, 0];
  const between = (oldTo: number, newTo: number) => {
    if (diff.isPartial) {
      [oldAt, newAt] = [oldTo, newTo];
      return;
    }
    for (; newAt < newTo; oldAt++, newAt++) {
      const text = unended(diff.additionLines[newAt]);
      all.push({
        side: 'additions',
        line: newAt + 1,
        old: oldAt + 1,
        mark: ' ',
        text,
      });
    }
  };
  for (const hunk of diff.hunks) {
    // The arrays hold only the hunks' lines until the library loads the whole file.
    const oldBase = hunk.deletionStart - hunk.deletionLineIndex;
    const newBase = hunk.additionStart - hunk.additionLineIndex;
    for (const block of hunk.hunkContent) {
      between(block.deletionLineIndex, block.additionLineIndex);
      const removed = block.type === 'context' ? 0 : block.deletions;
      const added = block.type === 'context' ? block.lines : block.additions;
      for (let index = 0; index < removed; index++) {
        const at = block.deletionLineIndex + index;
        const text = unended(diff.deletionLines[at]);
        all.push({
          side: 'deletions',
          line: at + oldBase,
          old: at + oldBase,
          mark: '-',
          text,
        });
      }
      for (let index = 0; index < added; index++) {
        const at = block.additionLineIndex + index;
        all.push({
          side: 'additions',
          line: at + newBase,
          old: block.deletionLineIndex + index + oldBase,
          mark: block.type === 'context' ? ' ' : '+',
          text: unended(diff.additionLines[at]),
        });
      }
      // An empty side starts at index -1, since the library subtracts one from the
      // hunk header's start of 0.
      oldAt = Math.max(
        0,
        block.deletionLineIndex + (block.type === 'context' ? added : removed),
      );
      newAt = Math.max(0, block.additionLineIndex + added);
    }
  }
  between(diff.deletionLines.length, diff.additionLines.length);
  return all;
}

const ended = (contents: string) => contents === '' || contents.endsWith('\n');

/** How many lines `contents` has. */
const counted = (contents: string) =>
  contents.split('\n').length - (ended(contents) ? 1 : 0);

/**
 * The whole diff from `oldFile` to `newFile`, whose hunks hold only its changes and the
 * `marked` lines of the new side, each with `context` lines around it, so the view
 * collapses the rest until it is expanded. The hunks are written as a patch for the
 * library to parse, then filled in with both files.
 */
export function excerpt(
  oldFile: FileContents,
  newFile: FileContents,
  marked: readonly Range[],
  context: number,
): FileDiffMetadata {
  const all = rows(parseDiffFromFile(oldFile, newFile));
  // How far each row is from the nearest change, looking both ways.
  const distance = all.map(() => Infinity);
  for (const order of [[...all.keys()], [...all.keys()].reverse()]) {
    let change = -Infinity;
    for (const index of order) {
      if (all[index]?.mark !== ' ') {
        change = index;
      }
      distance[index] = Math.min(
        distance[index] ?? Infinity,
        Math.abs(index - change),
      );
    }
  }
  const kept = all.map(
    (row, index) =>
      (distance[index] ?? Infinity) <= context ||
      (row.mark !== '-' &&
        marked.some(
          ({ from, to }) =>
            row.line >= from - context && row.line <= to + context,
        )),
  );
  // A hunk header counts from the line before a side it takes no line of.
  const start = (at: number, count: number) =>
    `${String(count === 0 ? at : at + 1)},${String(count)}`;
  const [oldLast, newLast] = [
    counted(oldFile.contents),
    counted(newFile.contents),
  ];
  const patch = [`--- ${oldFile.name}`, `+++ ${newFile.name}`];
  let hunk: { lines: string[]; old: number; new: number } | null = null;
  let [oldAt, newAt] = [0, 0];
  let [oldFrom, newFrom] = [0, 0];
  const close = () => {
    if (hunk !== null) {
      patch.push(
        `@@ -${start(oldFrom, hunk.old)} +${start(newFrom, hunk.new)} @@`,
        ...hunk.lines,
      );
    }
    hunk = null;
  };
  all.forEach((row, index) => {
    const [oldNext, newNext] = [
      oldAt + (row.mark === '+' ? 0 : 1),
      newAt + (row.mark === '-' ? 0 : 1),
    ];
    if (kept[index] === true) {
      if (hunk === null) {
        hunk = { lines: [], old: 0, new: 0 };
        [oldFrom, newFrom] = [oldAt, newAt];
      }
      hunk.lines.push(row.mark + row.text);
      hunk.old += oldNext - oldAt;
      hunk.new += newNext - newAt;
      if (
        (row.mark !== '+' && oldNext === oldLast && !ended(oldFile.contents)) ||
        (row.mark !== '-' && newNext === newLast && !ended(newFile.contents))
      ) {
        hunk.lines.push('\\ No newline at end of file');
      }
    } else {
      close();
    }
    [oldAt, newAt] = [oldNext, newNext];
  });
  close();
  const [diff] = parsePatchFiles(`${patch.join('\n')}\n`).flatMap(
    (parsed) => parsed.files,
  );
  if (diff === undefined) {
    throw new Error(`the excerpt of ${newFile.name} parses to no diff`);
  }
  return hydratePartialDiff('merge', diff, { oldFile, newFile });
}
