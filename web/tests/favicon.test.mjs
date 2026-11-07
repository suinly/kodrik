import {test} from 'node:test';
import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import {faviconHref} from '../app/utils/favicon.ts';

const icon=readFileSync(new URL('../public/chat/icon.svg',import.meta.url),'utf8');
const svg=href=>decodeURIComponent(href.split(',')[1]);

test('idle favicon preserves the original logo',()=>{
  assert.equal(svg(faviconHref(icon,{busy:false,unread:false},0)),icon);
});
test('running pulses a single blue dot on the right',()=>{
  const first=svg(faviconHref(icon,{busy:true,unread:false},0));
  assert.match(first,/<circle cx="440" cy="72"[^>]+fill="#89b4fa"/);
  assert.equal((first.match(/<circle/g)||[]).length,1);
  assert.notEqual(first,svg(faviconHref(icon,{busy:true,unread:false},3)));
  assert.doesNotMatch(first,/rotate\(/);
});
test('unread replaces the running dot with a static red dot',()=>{
  const unread=faviconHref(icon,{busy:false,unread:true},0);
  assert.equal(unread,faviconHref(icon,{busy:true,unread:true},3));
  assert.match(svg(unread),/<circle cx="440" cy="72"[^>]+fill="#f38ba8"/);
  assert.equal((svg(unread).match(/<circle/g)||[]).length,1);
  assert.doesNotMatch(svg(unread),/#89b4fa/);
});
test('reduced motion keeps the blue dot static',()=>{
  assert.equal(faviconHref(icon,{busy:true,unread:false},0,true),faviconHref(icon,{busy:true,unread:false},3,true));
});
