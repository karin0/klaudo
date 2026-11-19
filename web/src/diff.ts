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
 * Where lines of the old side of a diff that hides changes really are. Such a diff
 * numbers its old side as if only the changes it shows were made, and the lines it
 * numbers `from` to `to` are `shift` lines further down the file as of the start.
 */
export interface Shift {
  from: number;
  to: number;
  shift: number;
}

/** The line of the file as of the start that a diff with `shifts` numbers `line`. */
export const real = (shifts: readonly Shift[], line: number) =>
  line +
  (shifts.find(({ from, to }) => line >= from && line <= to)?.shift ?? 0);

/**
 * A git `patch` with the old side of each file numbered as if only the changes of its
 * hunks were made, as the library draws it, and where the lines of each file's old side
 * really are. A patch of every change of a file is numbered so already.
 */
export function isolated(patch: string): { patch: string; shifts: Shift[][] } {
  const shifts: Shift[][] = [];
  let skew = 0;
  const lines = patch.split('\n').map((line) => {
    if (line.startsWith('diff --git ')) {
      shifts.push([]);
      skew = 0;
      return line;
    }
    const header = /^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@(.*)$/su.exec(
      line,
    );
    if (header === null) {
      return line;
    }
    // A side of one line leaves its count out.
    const count = (field: string | undefined) =>
      field === undefined ? 1 : Number(field);
    const [oldStart, oldCount] = [Number(header[1]), count(header[2])];
    const [newStart, newCount] = [Number(header[3]), count(header[4])];
    // A side of no lines counts from the line before it.
    const from = (newCount === 0 ? newStart : newStart - 1) + skew;
    const shift = (oldCount === 0 ? oldStart : oldStart - 1) - from;
    if (oldCount > 0 && shift !== 0) {
      shifts.at(-1)?.push({ from: from + 1, to: from + oldCount, shift });
    }
    skew += oldCount - newCount;
    const start = oldCount === 0 ? from : from + 1;
    return `@@ -${String(start)},${String(oldCount)} +${String(newStart)},${String(newCount)} @@${header[5] ?? ''}`;
  });
  return { patch: lines.join('\n'), shifts };
}

/** The lines of `text`, each with the newline that ends it. */
const split = (text: string) => text.match(/[^\n]*\n|[^\n]+$/gu) ?? [];

/**
 * The old side of `diff`, which is numbered as `isolated` has it, with the changes its
 * hunks hide: `newText` with the lines each hunk adds replaced by the lines of
 * `oldText` it removes, which `shifts` finds there.
 */
export function undone(
  oldText: string,
  newText: string,
  diff: FileDiffMetadata,
  shifts: readonly Shift[],
): string {
  const [before, after] = [split(oldText), split(newText)];
  const lines: string[] = [];
  let at = 0;
  for (const hunk of diff.hunks) {
    // A side of no lines counts from the line before it.
    const from =
      hunk.additionCount === 0 ? hunk.additionStart : hunk.additionStart - 1;
    const removed = real(shifts, hunk.deletionStart) - 1;
    lines.push(...after.slice(at, from));
    if (hunk.deletionCount > 0) {
      lines.push(...before.slice(removed, removed + hunk.deletionCount));
    }
    at = from + hunk.additionCount;
  }
  lines.push(...after.slice(at));
  // Only the last line of a file may lack its newline.
  return lines
    .map((line, index) =>
      index < lines.length - 1 && !line.endsWith('\n') ? `${line}\n` : line,
    )
    .join('');
}

/**
 * The diff from `oldFile` to `newFile` at the `marked` lines of the new side: its hunks
 * hold those lines and each change a link names a line of, with `context` lines around
 * them, so the view collapses the rest until it is expanded. A change no link names
 * shows only inside that context. The library draws a diff only when the lines it
 * collapses are the same on both sides, so the old side takes every change the hunks
 * hide, and the collapsed lines expand as the new side has them. The hunks are written
 * as a patch for the library to parse, then filled in with both sides.
 */
export function excerpt(
  oldFile: FileContents,
  newFile: FileContents,
  marked: readonly Range[],
  context: number,
): { diff: FileDiffMetadata; shifts: Shift[] } {
  const all = rows(parseDiffFromFile(oldFile, newFile));
  // The lines of the new side a row lies between: its own, or those around a removed row.
  let last = 0;
  const reach = all.map((row): Range => {
    if (row.mark === '-') {
      return { from: last, to: last + 1 };
    }
    last = row.line;
    return { from: last, to: last };
  });
  const named = reach.map((row) =>
    marked.some(({ from, to }) => row.from >= from && row.to <= to),
  );
  // A change is shown whole once a link names one of its rows. `distance` is how far
  // each row is from the nearest shown change, looking both ways.
  const shown = all.map(
    (row, index) => row.mark !== ' ' && named[index] === true,
  );
  const distance = all.map(() => Infinity);
  for (const order of [[...all.keys()], [...all.keys()].reverse()]) {
    let change = -Infinity;
    let spread = false;
    for (const index of order) {
      spread = all[index]?.mark !== ' ' && (spread || shown[index] === true);
      if (spread) {
        shown[index] = true;
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
  // Any other removed row shows between rows of the new side that both show, so a hunk
  // hides no change between two of its rows.
  const beside = (index: number, step: number) => {
    let at = index;
    while (all[at]?.mark === '-') {
      at += step;
    }
    return kept[at] ?? true;
  };
  all.forEach((row, index) => {
    if (row.mark === '-' && kept[index] !== true) {
      kept[index] = beside(index, -1) && beside(index, 1);
    }
  });
  // The old side: every row but the added rows the hunks show and the removed rows
  // they hide. `ends` is whether each of its lines ends in a newline, as only the last
  // line of a file may not, and `place` numbers each row on it.
  const [oldLast, newLast] = [
    counted(oldFile.contents),
    counted(newFile.contents),
  ];
  const side: { text: string; ends: boolean }[] = [];
  const shifts: Shift[] = [];
  let [oldAt, newAt] = [0, 0];
  const place = all.map((row, index) => {
    oldAt += row.mark === '+' ? 0 : 1;
    newAt += row.mark === '-' ? 0 : 1;
    if (row.mark === (kept[index] === true ? '+' : '-')) {
      return 0;
    }
    side.push({
      text: row.text,
      ends:
        row.mark === '+'
          ? newAt < newLast || ended(newFile.contents)
          : oldAt < oldLast || ended(oldFile.contents),
    });
    if (row.mark === '-') {
      const shift = row.line - side.length;
      const last = shifts.at(-1);
      if (last?.shift === shift && last.to === side.length - 1) {
        last.to = side.length;
      } else {
        shifts.push({ from: side.length, to: side.length, shift });
      }
    }
    return side.length;
  });
  const sideEnded = side.at(-1)?.ends ?? true;
  const taking = {
    name: oldFile.name,
    contents: side
      .map(({ text }, index) =>
        index < side.length - 1 || sideEnded ? `${text}\n` : text,
      )
      .join(''),
  };
  // A hunk header counts from the line before a side it takes no line of.
  const start = (at: number, count: number) =>
    `${String(count === 0 ? at : at + 1)},${String(count)}`;
  const patch = [`--- ${oldFile.name}`, `+++ ${newFile.name}`];
  let hunk: { lines: string[]; old: number; new: number } | null = null;
  [oldAt, newAt] = [0, 0];
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
      place[index] === 0 ? oldAt : oldAt + 1,
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
        (row.mark !== '+' && oldNext === side.length && !sideEnded) ||
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
  return {
    diff: hydratePartialDiff('merge', diff, { oldFile: taking, newFile }),
    shifts,
  };
}
