import {
  hydratePartialDiff,
  parsePatchFiles,
  type FileDiffMetadata,
} from '@pierre/diffs';
import { describe, expect, test } from 'vitest';
import type { Range } from './api';
import { excerpt, isolated, real, rows, undone } from './diff';

const CONTEXT = 5;
const WHOLE = [{ from: 1, to: Infinity }];

/** Lines `from` to `to` of a file whose line n reads `line n`. */
const lines = (from: number, to: number) =>
  Array.from(
    { length: to - from + 1 },
    (_, index) => `line ${String(from + index)}\n`,
  );

/** A file of 20 lines that ends in a newline. */
const ended = lines(1, 20).join('');

const cut = (oldText: string, newText: string, marked: readonly Range[]) =>
  excerpt(
    { name: 'a.ts', contents: oldText },
    { name: 'a.ts', contents: newText },
    marked,
    CONTEXT,
  ).diff;

/** The first and last line each hunk takes of the old and the new side. */
const spans = (diff: FileDiffMetadata) =>
  diff.hunks.map((hunk) => ({
    old: [hunk.deletionStart, hunk.deletionStart + hunk.deletionCount - 1],
    new: [hunk.additionStart, hunk.additionStart + hunk.additionCount - 1],
  }));

/** Asserts that expanding every collapsed region shows both sides whole. */
function expandsTo(diff: FileDiffMetadata, oldText: string, newText: string) {
  expect(diff.isPartial).toBe(false);
  const all = rows(diff);
  const side = (mark: ' ' | '-' | '+') =>
    all.filter((row) => row.mark !== mark).map((row) => row.text);
  const split = (text: string) =>
    text.match(/[^\n]*\n|[^\n]+$/g)?.map((line) => line.replace(/\n$/, '')) ??
    [];
  expect(side('+')).toEqual(split(oldText));
  expect(side('-')).toEqual(split(newText));
}

describe('excerpt', () => {
  test('keeps each range of an unchanged file with its context and collapses the rest', () => {
    const text = lines(1, 40).join('');
    const diff = cut(text, text, [
      { from: 10, to: 10 },
      { from: 30, to: 31 },
    ]);
    expect(spans(diff)).toEqual([
      { old: [5, 15], new: [5, 15] },
      { old: [25, 36], new: [25, 36] },
    ]);
    expandsTo(diff, text, text);
  });

  test('shows a change inside the context of a range', () => {
    const oldText = lines(1, 40).join('');
    const newText = [...lines(1, 19), 'changed\n', ...lines(21, 40)].join('');
    const diff = cut(oldText, newText, [{ from: 16, to: 16 }]);
    expect(spans(diff)).toEqual([{ old: [11, 21], new: [11, 21] }]);
    expandsTo(diff, oldText, newText);
  });

  test('collapses a change no link names, which expands as the file is', () => {
    const oldText = lines(1, 40).join('');
    const newText = [...lines(1, 19), 'changed\n', ...lines(21, 40)].join('');
    const diff = cut(oldText, newText, [{ from: 2, to: 2 }]);
    expect(spans(diff)).toEqual([{ old: [1, 7], new: [1, 7] }]);
    expect(rows(diff).every((row) => row.mark === ' ')).toBe(true);
    expandsTo(diff, newText, newText);
  });

  test('collapses a change of the last line and its missing newline', () => {
    const oldText = ended.slice(0, -1);
    const newText = `${ended}more\n`;
    const diff = cut(oldText, newText, [{ from: 1, to: 1 }]);
    expect(spans(diff)).toEqual([{ old: [1, 6], new: [1, 6] }]);
    expect(diff.hunks[0]).toMatchObject({
      noEOFCRDeletions: false,
      noEOFCRAdditions: false,
    });
    expandsTo(diff, newText, newText);
  });

  test('shows the whole of a change a link names a line of', () => {
    const oldText = lines(1, 60).join('');
    const changed = Array.from({ length: 12 }, () => 'changed\n');
    const newText = [
      ...lines(1, 3),
      'added\n',
      ...lines(4, 29),
      ...changed,
      ...lines(40, 60),
    ].join('');
    const { diff, shifts } = excerpt(
      { name: 'a.ts', contents: oldText },
      { name: 'a.ts', contents: newText },
      [{ from: 35, to: 35 }],
      CONTEXT,
    );
    // The old side takes the line added above, which the hunk hides, so it numbers the
    // removed lines one past where the file as of the start has them.
    expect(spans(diff)).toEqual([{ old: [26, 45], new: [26, 47] }]);
    const all = rows(diff);
    const removed = all.filter((row) => row.mark === '-');
    expect(removed.map((row) => real(shifts, row.line))).toEqual(
      Array.from({ length: 10 }, (_, index) => 30 + index),
    );
    expect(all.filter((row) => row.mark === '+')).toHaveLength(12);
    const taking = [...lines(1, 3), 'added\n', ...lines(4, 60)].join('');
    expandsTo(diff, taking, newText);
  });

  test('shows the lines removed between two lines a link names', () => {
    const oldText = lines(1, 40).join('');
    const newText = [...lines(1, 19), ...lines(23, 40)].join('');
    const named = cut(oldText, newText, [{ from: 19, to: 20 }]);
    expect(spans(named)).toEqual([{ old: [14, 28], new: [14, 25] }]);
    // Past the context of a range, a removal stays collapsed.
    const apart = cut(oldText, newText, [{ from: 10, to: 10 }]);
    expect(spans(apart)).toEqual([{ old: [5, 15], new: [5, 15] }]);
    expandsTo(apart, newText, newText);
  });

  test('keeps the whole file for a link naming no lines', () => {
    const text = lines(1, 40).join('');
    const diff = cut(text, text, WHOLE);
    expect(spans(diff)).toEqual([{ old: [1, 40], new: [1, 40] }]);
    expandsTo(diff, text, text);
  });

  test('adds every line of a file the start lacks', () => {
    const text = lines(1, 20).join('');
    const diff = cut('', text, [{ from: 3, to: 3 }]);
    expect(diff.hunks).toHaveLength(1);
    expect(diff.hunks[0]).toMatchObject({
      deletionStart: 0,
      deletionCount: 0,
      additionStart: 1,
      additionCount: 20,
    });
    expect(rows(diff).every((row) => row.mark === '+')).toBe(true);
    expandsTo(diff, '', text);
  });

  test('removes every line of a file emptied since the start', () => {
    const text = lines(1, 20).join('');
    const diff = cut(text, '', WHOLE);
    expect(diff.hunks).toHaveLength(1);
    expect(diff.hunks[0]).toMatchObject({
      deletionStart: 1,
      deletionCount: 20,
      additionStart: 0,
      additionCount: 0,
    });
    expect(rows(diff).every((row) => row.mark === '-')).toBe(true);
    expandsTo(diff, text, '');
  });

  test.each([
    ['the old side', ended.slice(0, -1), ended, true, false],
    ['the new side', ended, ended.slice(0, -1), false, true],
    ['both sides', ended.slice(0, -1), `${ended}changed`, true, true],
  ])(
    'marks a missing newline at the end of %s',
    (_, oldText, newText, deletions, additions) => {
      const diff = cut(oldText, newText, [{ from: 20, to: 20 }]);
      expect(diff.hunks.at(-1)).toMatchObject({
        noEOFCRDeletions: deletions,
        noEOFCRAdditions: additions,
      });
      expandsTo(diff, oldText, newText);
    },
  );

  test('keeps the last line of an unchanged file without a final newline', () => {
    const text = ended.slice(0, -1);
    const diff = cut(text, text, [{ from: 20, to: 20 }]);
    expect(spans(diff)).toEqual([{ old: [15, 20], new: [15, 20] }]);
    expect(diff.hunks[0]).toMatchObject({
      noEOFCRDeletions: true,
      noEOFCRAdditions: true,
    });
    expandsTo(diff, text, text);
  });

  test('collapses the whole of an unchanged file whose ranges lie past its end', () => {
    const text = lines(1, 40).join('');
    const diff = cut(text, text, [{ from: 60, to: 60 }]);
    expect(diff.hunks).toEqual([]);
    expandsTo(diff, text, text);
  });

  test('shows an empty file as no hunks', () => {
    const diff = cut('', '', WHOLE);
    expect(diff.hunks).toEqual([]);
    expandsTo(diff, '', '');
  });
});

describe('isolated', () => {
  const oldText = lines(1, 40).join('');
  const newText = [
    ...lines(1, 4),
    'added\n',
    'added\n',
    ...lines(5, 29),
    'changed\n',
    ...lines(31, 40),
  ].join('');
  // As `git log -L` writes the change of the line that reads `changed` alone.
  const patch = [
    'diff --git a/a.ts b/a.ts',
    '--- a/a.ts',
    '+++ b/a.ts',
    '@@ -30 +32 @@ fn x()',
    '-line 30',
    '+changed',
    '',
  ].join('\n');

  test('numbers a hunk as if the changes outside the patch were made', () => {
    const numbered = isolated(patch);
    expect(numbered.patch).toContain('\n@@ -32,1 +32,1 @@ fn x()\n');
    expect(numbered.shifts).toEqual([[{ from: 32, to: 32, shift: -2 }]]);
    const [diff] = parsePatchFiles(numbered.patch).flatMap(
      (parsed) => parsed.files,
    );
    if (diff === undefined) {
      throw new Error('the patch parses to no diff');
    }
    const taking = undone(oldText, newText, diff, numbered.shifts[0] ?? []);
    expect(taking).toBe(newText.replace('changed\n', 'line 30\n'));
    const whole = hydratePartialDiff('merge', diff, {
      oldFile: { name: 'a.ts', contents: taking },
      newFile: { name: 'a.ts', contents: newText },
    });
    expandsTo(whole, taking, newText);
  });

  test('leaves a patch of every change of a file as git numbered it', () => {
    const every = [
      'diff --git a/a.ts b/a.ts',
      '--- a/a.ts',
      '+++ b/a.ts',
      '@@ -4,0 +5,2 @@',
      '+added',
      '+added',
      '@@ -30,1 +32,1 @@',
      '-line 30',
      '+changed',
      '',
    ].join('\n');
    const numbered = isolated(every);
    expect(numbered).toEqual({ patch: every, shifts: [[]] });
    const [diff] = parsePatchFiles(every).flatMap((parsed) => parsed.files);
    if (diff === undefined) {
      throw new Error('the patch parses to no diff');
    }
    expect(undone(oldText, newText, diff, [])).toBe(oldText);
  });
});
