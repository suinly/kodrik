import {test} from 'node:test';
import assert from 'node:assert/strict';
import {commandAuditReason,commandApprovalDetails} from '../app/utils/command-audit.ts';
import {setLanguage} from '../app/utils/i18n.ts';

test('approval reasons localize known codes and legacy text while unknown text stays literal',()=>{
  for(const [code,english,russian] of [
    ['safe','Tool call assessed as safe','Вызов инструмента признан безопасным'],
    ['destructive','May delete data, overwrite work or change a live service','Может удалить данные, перезаписать работу или изменить работающий сервис'],
    ['disclosure','May disclose credentials or private data','Может раскрыть учётные данные или личную информацию'],
    ['security','May change permissions or weaken security','Может изменить права доступа или ослабить защиту'],
    ['uncertain','Tool effects could not be established safely','Не удалось безопасно определить последствия вызова'],
    ['unavailable','Independent tool review was unavailable','Независимая проверка инструмента недоступна'],
    ['ask','This session requires permission for every tool call','Режим «Спрашивать всегда» требует разрешения для каждого вызова инструмента'],
  ]) {
    setLanguage('ru');
    assert.equal(commandAuditReason({reason_code:code,reason:'ignored'}),russian);
    assert.equal(commandAuditReason({reason:english}),russian);
    setLanguage('en');
    assert.equal(commandAuditReason({reason_code:code,reason:'ignored'}),english);
  }
  setLanguage('en');
  assert.equal(commandAuditReason({reason:'This session requires permission for every command'}),'This session requires permission for every tool call');
  setLanguage('ru');
  for(const reason_code of [undefined,'unknown','__proto__','constructor']) {
    assert.equal(commandAuditReason({reason_code,reason:'<script>unknown</script>'}),'<script>unknown</script>');
  }
});

test('tool approval details show the exact tool and full arguments with visible control characters',()=>{
  const approval={tool_name:'mcp__files__delete',arguments_json:JSON.stringify({path:'important.txt',note:'\u202e\n'}),cmd:'',workdir:null};
  const details=commandApprovalDetails(approval);
  assert.match(details,/mcp__files__delete/);
  assert.match(details,/important\.txt/);
  assert.ok(details.includes('\\u202e\\n'));
  assert.ok(!details.includes('\u202e'));
  const legacy=JSON.parse(commandApprovalDetails({cmd:'rm important.txt',workdir:'/workspace'}));
  assert.deepEqual(legacy,{tool:'exec_command',arguments:{cmd:'rm important.txt',workdir:'/workspace'}});
});

test('tool approval display preserves server arguments beyond JavaScript integer precision',()=>{
  const approval=JSON.parse(String.raw`{"tool_name":"mcp__records__delete","arguments_json":"{\"record_id\":9007199254740993}","cmd":"","workdir":null}`);
  const details=commandApprovalDetails(approval);
  assert.ok(details.includes('9007199254740993'));
  assert.ok(!details.includes('9007199254740992'));
});
