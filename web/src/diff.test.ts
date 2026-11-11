import type { FileDiffMetadata } from '@pierre/diffs';
import { describe, expect, test } from 'vitest';
import type { Range } from './api';
import { excerpt, rows } from './diff';

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
  );

/** The first and last line each hunk takes of the old and the new side. */
const spans = (diff: FileDiffMetadata) =>
  diff.hunks.map((hunk) => ({
    old: [hunk.deletionStart, hunk.deletionStart + hunk.deletionCount - 1],
    new: [hunk.additionStart, hunk.additionStart + hunk.additionCount - 1],
  }));

/** Asserts that expanding every collapsed region shows both files whole. */
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

  test('joins a change and a range whose context meets it into one hunk', () => {
    const oldText = lines(1, 40).join('');
    const newText = [...lines(1, 19), 'changed\n', ...lines(21, 40)].join('');
    const diff = cut(oldText, newText, [{ from: 9, to: 9 }]);
    expect(spans(diff)).toEqual([{ old: [4, 25], new: [4, 25] }]);
    expandsTo(diff, oldText, newText);
  });

  test('keeps a change and a range apart when lines lie between their context', () => {
    const oldText = lines(1, 40).join('');
    const newText = [...lines(1, 19), 'changed\n', ...lines(21, 40)].join('');
    const diff = cut(oldText, newText, [{ from: 2, to: 2 }]);
    expect(spans(diff)).toEqual([
      { old: [1, 7], new: [1, 7] },
      { old: [15, 25], new: [15, 25] },
    ]);
    expandsTo(diff, oldText, newText);
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

  test.each([
    ['the old side', ended.slice(0, -1), ended, true, false],
    ['the new side', ended, ended.slice(0, -1), false, true],
    ['both sides', ended.slice(0, -1), `${ended}changed`, true, true],
  ])(
    'marks a missing newline at the end of %s',
    (_, oldText, newText, deletions, additions) => {
      const diff = cut(oldText, newText, [{ from: 2, to: 2 }]);
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

  test('shows an empty file as no hunks', () => {
    const diff = cut('', '', WHOLE);
    expect(diff.hunks).toEqual([]);
    expandsTo(diff, '', '');
  });
});
