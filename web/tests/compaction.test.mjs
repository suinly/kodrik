import {test} from 'node:test';
import assert from 'node:assert/strict';
import {setLanguage} from '../app/utils/i18n.ts';
import {compactionActive, formatCompaction} from '../app/utils/compaction.ts';

const start='2026-11-04T12:00:00Z';
const progress={completed_tokens:250,total_tokens:1000,completed_chunks:1,total_chunks:4,model_calls:2,started_at:start,request_started_at:'2026-11-04T12:00:10Z',finished_at:null,background:true,before_tokens:12000,after_tokens:null};
const now=Date.parse('2026-11-04T12:00:25Z');
setLanguage('ru');

test('compaction reports completed estimates only while request waiting time advances',()=>{
  const state={status:'preparing',progress};
  const text=formatCompaction(state,now);
  assert.match(text,/в фоне/);
  assert.match(text,/25%.*оценка/);
  assert.match(text,/1\/4/);
  assert.match(text,/25 с/);
  assert.match(text,/15 с/);
  assert.match(text,/2/);
  const later=formatCompaction(state,now+10000);
  assert.match(later,/25%/);
  assert.match(later,/25 с/);
  assert.equal(compactionActive(state),true);
  assert.doesNotMatch(text,/ETA|осталось|remaining/);
});

test('finished results retain before/after tokens and never advance their clock',()=>{
  const state={status:'applied',progress:{...progress,finished_at:'2026-11-04T12:00:20Z',after_tokens:3000}};
  assert.equal(compactionActive(state),false);
  assert.match(formatCompaction(state,now),/12000 → 3000/);
  assert.match(formatCompaction(state,now),/20 с/);
  assert.equal(formatCompaction(state,now),formatCompaction(state,now+10000));
});

test('cancelled failed and stale jobs have no ticking or active request labels',()=>{
  for(const status of ['cancelled','failed','stale']) {
    const state={status,progress};
    assert.equal(compactionActive(state),false);
    assert.equal(formatCompaction(state,now),formatCompaction(state,now+10000));
    assert.doesNotMatch(formatCompaction(state,now),/ожидание|waiting/);
  }
});

test('legacy jobs and zero totals work in both languages',()=>{
  assert.equal(formatCompaction({status:'preparing'},now),'Готовлю краткий контекст…');
  try {
    setLanguage('en');
    assert.equal(formatCompaction({status:'failed'},now),'Could not prepare the context summary');
    const text=formatCompaction({status:'preparing',progress:{...progress,total_tokens:0,completed_tokens:0,background:false}},now);
    assert.doesNotMatch(text,/NaN|Infinity|background/);
    assert.match(text,/Waiting/);
    assert.match(text,/0%/);
  } finally {setLanguage('ru');}
});
