import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import ts from 'typescript';

// The module as the page runs it, measured with a ruler of its own: a
// column a character, two for one of a script written without spaces.
const path = new URL('../src/wrap.ts', import.meta.url);
const plain = ts.transpileModule(readFileSync(path, 'utf8'), {
  compilerOptions: { target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.ESNext },
}).outputText;
const { breakLines } = await import(`data:text/javascript,${encodeURIComponent(plain)}`);

const columns = (text) => Array.from(text).reduce((sum, ch) => sum + (ch.codePointAt(0) >= 0x2e80 ? 2 : 1), 0);
const lay = (text, room, rows = 99) => breakLines(text, room, rows, columns);

test('lines break after a space, and a word too long for one where it overflows', () => {
  assert.deepEqual(lay('the quick brown fox', 10), { lines: ['the quick', 'brown fox'], leftOver: false });
  assert.deepEqual(lay('abcdefghijkl', 5).lines, ['abcde', 'fghij', 'kl']);
});

test('a script written without spaces breaks between its characters, not before closing punctuation', () => {
  assert.deepEqual(lay('中文的句子很长', 6).lines, ['中文的', '句子很', '长']);
  assert.deepEqual(lay('中文。句子', 4).lines, ['中', '文。', '句子']);
});

test('a letter keeps its accent, and an emoji its parts', () => {
  const marks = (text) => Array.from(text).filter((ch) => !/\p{M}|\u200d/u.test(ch)).length;
  assert.deepEqual(breakLines('ab\u0301c', 1, 99, marks).lines, ['a', 'b\u0301', 'c']);
  const family = '\u{1f468}\u200d\u{1f469}\u200d\u{1f467}';
  // Wider than the box, it is not split to fit.
  assert.deepEqual(breakLines(`${family}x`, 2, 99, marks).lines, [family, 'x']);
});

test('the text breaks at its own line breaks, and keeps its empty lines', () => {
  assert.deepEqual(lay('one\r\n\ntwo\n', 10).lines, ['one', '', 'two']);
});

test('what does not fit in the rows is left over', () => {
  assert.deepEqual(lay('a b c d e f', 3, 2), { lines: ['a b', 'c d'], leftOver: true });
  assert.deepEqual(lay('one\ntwo', 10, 1), { lines: ['one'], leftOver: true });
  assert.deepEqual(lay('one\ntwo', 10, 2), { lines: ['one', 'two'], leftOver: false });
});
