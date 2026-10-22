import {setLanguage} from '../app/utils/i18n.ts';
setLanguage('ru');
import {test} from 'node:test';
import assert from 'node:assert/strict';
import {createUiDateFormatter} from '../app/utils/ui-date.ts';

test('date formatting preserves calendar dates and applies zones only to timestamps',()=>{
  assert.equal(createUiDateFormatter()('2026-10-21'),'21.10.2026');
  assert.equal(createUiDateFormatter({timeZone:'America/Los_Angeles'})('2026-10-21'),'21.10.2026');
  assert.equal(createUiDateFormatter({timeZone:'Europe/Moscow'})('2026-10-21T23:30:00Z'),'22.10.2026');
  assert.match(createUiDateFormatter('long')('2026-10-21'),/^21 октября 2026/);
  assert.equal(createUiDateFormatter({locale:'en-US',month:'long',year:'numeric'})('2026-10-21'),'October 2026');
  assert.equal(createUiDateFormatter({timeStyle:'short',timeZone:'Europe/Moscow'})('2026-10-21T23:30:00Z'),'02:30');
  assert.equal(createUiDateFormatter()(null),'Нет данных');
  assert.equal(createUiDateFormatter()(undefined),'Нет данных');
  assert.equal(createUiDateFormatter()('2024-02-29'),'29.02.2024');
});

test('date formatting rejects impossible dates and ambiguous timestamps',()=>{
  for(const value of ['2026-02-29','2026-04-31','2026-13-01','21.10.2026','2026-10-21T12:30','2026-10-21T24:00:00Z','',0,{},true]) {
    assert.throws(()=>createUiDateFormatter()(value),undefined,JSON.stringify(value));
  }
});

test('date options are closed JSON settings validated before use',()=>{
  for(const options of [[],null,42,'bad',{dateStyle:'bad'},{timeZone:'Invalid/Zone'},{locale:'bad@locale'},{locale:'zz-ZZ'},{format:'DD.MM.YYYY'},{hour12:'yes'},{dateStyle:'short',day:'2-digit'}]) {
    assert.throws(()=>createUiDateFormatter(options),undefined,JSON.stringify(options));
  }
});
