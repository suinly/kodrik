import {setLanguage} from '../app/utils/i18n.ts';
setLanguage('ru');
import {test} from 'node:test';
import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';
import {buildCatalog} from '../scripts/generate-ui-catalog.mjs';

test('catalog comes from the installed package and excludes application-only components',async()=>{
  const catalog=await buildCatalog();
  const pkg=JSON.parse(await readFile(new URL('../node_modules/@nuxt/ui/package.json',import.meta.url)));
  assert.equal(catalog.version,pkg.version);
  for(const name of ['u-chart','u-form','u-calendar','u-tabs','u-table','u-modal','u-select','u-input']) assert.ok(catalog.components[name],name);
  for(const name of ['u-editor','u-editor-toolbar','u-chat-prompt','u-container','u-page-grid','u-dashboard-panel','u-sidebar','u-auth-form','u-file-upload','u-theme','u-pricing-table','u-app']) assert.equal(catalog.components[name],undefined,name);
  assert.ok(catalog.components['u-chart'].props.includes('data'));
  assert.match(catalog.components['u-chart'].api,/tooltip/);
  assert.match(catalog.components['u-card'].api,/header/);
  assert.ok(catalog.components['u-select'].models.includes('modelValue'));
});

test('checked-in client and agent catalogs match the installed package',async()=>{
  const catalog=await buildCatalog();
  assert.deepEqual(JSON.parse(await readFile(new URL('../../src/agent/tools/ui_catalog.json',import.meta.url))),catalog);
  const manifest=JSON.parse(await readFile(new URL('../app/utils/ui-components.json',import.meta.url)));
  assert.deepEqual(Object.keys(manifest.components),Object.keys(catalog.components));
  assert.equal(manifest.version,catalog.version);
  for(const [tag,component] of Object.entries(catalog.components)) assert.deepEqual(manifest.components[tag],{name:component.name,props:component.props,booleans:component.booleans,models:component.models});
});
