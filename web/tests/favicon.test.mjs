import {test} from 'node:test';
import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import {faviconHref} from '../app/utils/favicon.ts';

const icon=readFileSync(new URL('../public/chat/icon.svg',import.meta.url),'utf8');
const svg=href=>decodeURIComponent(href.split(',')[1]);

test('idle favicon preserves the original logo',()=>{
  assert.equal(svg(faviconHref(icon,{busy:false,unread:false},0)),icon);
});
test('running pulses without changing the unread marker',()=>{
  const first=svg(faviconHref(icon,{busy:true,unread:true},0));
  const next=svg(faviconHref(icon,{busy:true,unread:true},3));
  assert.notEqual(first,next);
  assert.match(first,/<g transform=/);
  assert.match(first,/<circle[^>]+cx="440"[^>]+fill="#f38ba8"/);
  assert.equal(first.match(/<circle[^>]+cx="440"[^>]+>/)[0],next.match(/<circle[^>]+cx="440"[^>]+>/)[0]);
});
test('unread stays static and does not imply a running agent',()=>{
  const first=faviconHref(icon,{busy:false,unread:true},0);
  assert.equal(first,faviconHref(icon,{busy:false,unread:true},5));
  assert.equal((svg(first).match(/<circle/g)||[]).length,1);
});
test('reduced motion keeps the running marker static',()=>{
  assert.equal(faviconHref(icon,{busy:true,unread:true},0,true),faviconHref(icon,{busy:true,unread:true},5,true));
});
