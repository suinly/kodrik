import {test} from 'node:test';
import assert from 'node:assert/strict';
import {commandAuditReason} from '../app/utils/command-audit.ts';
import {setLanguage} from '../app/utils/i18n.ts';

test('approval reasons localize known codes and legacy text while unknown text stays literal',()=>{
  for(const [code,english,russian] of [
    ['safe','Command assessed as safe','Команда признана безопасной'],
    ['destructive','May delete data, overwrite work or change a live service','Может удалить данные, перезаписать работу или изменить работающий сервис'],
    ['disclosure','May disclose credentials or private data','Может раскрыть учётные данные или личную информацию'],
    ['security','May change permissions or weaken security','Может изменить права доступа или ослабить защиту'],
    ['uncertain','Command effects could not be established safely','Не удалось безопасно определить последствия команды'],
    ['unavailable','Independent command review was unavailable','Независимая проверка команды недоступна'],
    ['ask','This session requires permission for every command','Режим «Спрашивать всегда» требует разрешения для каждой команды'],
  ]) {
    setLanguage('ru');
    assert.equal(commandAuditReason({reason_code:code,reason:'ignored'}),russian);
    assert.equal(commandAuditReason({reason:english}),russian);
    setLanguage('en');
    assert.equal(commandAuditReason({reason_code:code,reason:'ignored'}),english);
  }
  setLanguage('ru');
  for(const reason_code of [undefined,'unknown','__proto__','constructor']) {
    assert.equal(commandAuditReason({reason_code,reason:'<script>unknown</script>'}),'<script>unknown</script>');
  }
});
